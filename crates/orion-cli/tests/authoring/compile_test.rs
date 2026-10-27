//! `orion-cli compile`: a definition set in, a deployment directory out.
//!
//! The command exists because nothing else performs the expansion a deploy
//! needs: the admin API stores only compiled documents, so the only path from
//! `definitions/` to a running instance would otherwise be a tool that
//! reimplemented the expander — and #295 is what that costs when the
//! reimplementation misses a case.
//!
//! What these assert, therefore, is not just "it wrote a file" but the two
//! properties that make the output trustworthy: **nothing source-form
//! survives**, and what is written is what a server stores (the last section
//! uploads a compiled directory and plans it).

use crate::common::{ScratchDir, orion_bin};
use std::process::Command;

fn run(args: &[&str]) -> (bool, String) {
    let out = Command::new(orion_bin())
        .args(args)
        .output()
        .expect("run orion-cli");
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), combined)
}

/// The definition the compiled deployment at `out` carries for member
/// `(kind, id)`, as its file holds it: `{"kind": …, "definition": {…}}`.
fn definition_of(out: &std::path::Path, kind: &str, id: &str) -> serde_json::Value {
    let deployment = read_json(&out.join("deployment.json"));
    let member = deployment["members"]
        .as_array()
        .and_then(|members| members.iter().find(|m| m["kind"] == kind && m["id"] == id))
        .unwrap_or_else(|| panic!("no {kind} '{id}' in {deployment}"));
    let digest = member["definition"].as_str().expect("a definition digest");
    read_json(
        &out.join("definitions")
            .join(format!("{}.json", digest.trim_start_matches("sha256:"))),
    )
}

/// A set that uses every authoring convenience at once: a fragment with an
/// argument, a `$from` in a task input, and a `$from` buried in a `map`
/// mapping's `logic` — the last being the one a residue check that skipped
/// mapping logic would let reach the admin API uncompiled.
fn sugared_set(label: &str) -> ScratchDir {
    // A distinct label per test: `ScratchDir` removes its directory on drop,
    // and these run in parallel, so a shared name lets one test delete the
    // files another is still reading.
    let scratch = ScratchDir::new(label);
    let dir = scratch.path();
    std::fs::create_dir_all(dir.join("fragments")).unwrap();
    std::fs::write(
        dir.join("common.json"),
        r#"{ "constants": { "db": { "connector": "pg", "schema": { "unmapped": "identity" } } },
             "errors": { "USER_NOT_FOUND": { "status": 404, "body": "User Not Found !" } } }"#,
    )
    .unwrap();
    // The fragment carries a task group on purpose: a guard clause is the
    // shape 1.2.0 encourages, and until #294 the ids inside such a group were
    // emitted verbatim into the host workflow's namespace.
    std::fs::write(
        dir.join("fragments/guard.json"),
        r#"{ "fragments": { "deny": {
              "params": { "message": { "default": "Denied." } },
              "tasks": [ { "id": "write", "name": "Write the refusal",
                "function": { "name": "map", "input": { "mappings": [
                  { "path": "data.denied", "logic": { "$param": "message" } } ] } } },
                { "id": "refused", "condition": true, "tasks": [
                  { "id": "log", "name": "Log the refusal",
                    "function": { "name": "map", "input": { "mappings": [
                      { "path": "data.logged", "logic": { "$param": "message" } } ] } } } ] } ] } } }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("conn.json"),
        r#"{ "name": "pg", "connector_type": "db",
             "config": { "driver": "postgresql", "connection_string": "postgres://host/app" } }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("wf.json"),
        r#"{ "workflow_id": "lookup", "name": "Lookup", "tasks": [
             { "id": "_g", "use": "deny", "with": { "message": "Please sign in." } },
             { "id": "read", "name": "Read", "function": { "name": "data_query",
               "input": { "$from": "constants.db", "query": { "source": "users" },
                          "output": "temp_data.u" } } },
             { "id": "err", "name": "Err", "function": { "name": "map",
               "input": { "mappings": [
                 { "path": "data.out", "logic": { "$from": "errors.USER_NOT_FOUND" } } ] } } } ] }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("ch.json"),
        r#"{ "channel_id": "lookup-api", "name": "lookup-api", "channel_type": "sync",
             "protocol": "rest", "methods": ["POST"], "route_pattern": "/lookup",
             "workflow_id": "lookup" }"#,
    )
    .unwrap();
    scratch
}

#[test]
fn a_deployment_is_fully_compiled() {
    let scratch = sugared_set("compile-sugared");
    let dir = scratch.path();
    let out = dir.join("dist");

    let (ok, report) = compile_deployment(dir, &out);
    assert!(ok, "{report}");
    // The compiler says what it did rather than leaving the author to diff the
    // output to find out.
    assert!(
        report.contains("shared.fragments rewrote 1 document(s)"),
        "{report}"
    );
    assert!(
        report.contains("shared.values rewrote 1 document(s)"),
        "{report}"
    );

    for (name, bytes) in tree(&out) {
        let raw = String::from_utf8(bytes).expect("utf-8");
        assert!(
            !raw.contains("$from") && !raw.contains("\"use\""),
            "no source form may survive compilation: {name}\n{raw}"
        );
    }

    let workflow = definition_of(&out, "workflow", "lookup");
    let tasks = &workflow["definition"]["tasks"];
    // The fragment inlined, with its inner task's id namespaced by the call
    // site (`_g` + `write`) and its parameter substituted from `with`.
    assert_eq!(tasks[0]["id"], "_g.write");
    assert_eq!(
        tasks[0]["function"]["input"]["mappings"][0]["logic"],
        "Please sign in."
    );
    // Namespacing reaches inside the fragment's task group, and so do its
    // parameters (#294) — a bare `log` here would collide with any host task
    // of that name, and with a second instance of the fragment.
    assert_eq!(tasks[1]["id"], "_g.refused");
    assert_eq!(tasks[1]["tasks"][0]["id"], "_g.log");
    assert_eq!(
        tasks[1]["tasks"][0]["function"]["input"]["mappings"][0]["logic"],
        "Please sign in."
    );
    // The splice merged rather than replaced: the shared fields arrived and
    // the call site's own survived beside them.
    assert_eq!(tasks[2]["function"]["input"]["connector"], "pg");
    assert_eq!(
        tasks[2]["function"]["input"]["schema"],
        serde_json::json!({ "unmapped": "identity" })
    );
    assert_eq!(tasks[2]["function"]["input"]["query"]["source"], "users");
    // The deep one — inside a mapping's `logic`, where an uncompiled `$from`
    // would otherwise run verbatim.
    assert_eq!(
        tasks[3]["function"]["input"]["mappings"][0]["logic"]["body"],
        "User Not Found !"
    );
}

#[test]
fn a_reference_that_does_not_resolve_writes_nothing() {
    let scratch = sugared_set("compile-unresolved");
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{ "workflow_id": "lookup", "name": "Lookup", "tasks": [
             { "id": "read", "name": "Read", "function": { "name": "data_query",
               "input": { "$from": "constants.dbb", "query": { "source": "users" },
                          "output": "temp_data.u" } } } ] }"#,
    )
    .unwrap();
    let out = dir.join("dist");

    let (ok, report) = compile_deployment(dir, &out);
    assert!(!ok, "{report}");
    assert!(report.contains("constants.dbb"), "{report}");
    assert!(
        !out.join("deployment.json").exists(),
        "a refused compile must leave no deployment behind — a stale one would \
         apply cleanly and be wrong"
    );
}

/// `compile` gates with `lint <dir>`'s own pass, so a set that fails the linter
/// cannot be emitted. Without this, a deployment reaches apply having passed
/// CI and fails there instead.
#[test]
fn the_gate_is_the_linters_own() {
    let scratch = sugared_set("compile-gate");
    let dir = scratch.path();
    std::fs::write(
        dir.join("dangling.json"),
        r#"{ "workflow_id": "dangling", "name": "Dangling", "tasks": [
             { "id": "c", "name": "c", "function": { "name": "data_query",
               "input": { "connector": "no-such-connector", "query": { "source": "y" },
                          "output": "temp_data.z" } } } ] }"#,
    )
    .unwrap();
    let out = dir.join("dist");
    let (ok, report) = compile_deployment(dir, &out);
    assert!(!ok, "{report}");
    assert!(report.contains("no-such-connector"), "{report}");
    assert!(!out.join("deployment.json").exists());
}

/// A deployment's members are addressed by id — a member is its kind and id —
/// so `compile` demands ids where `lint <dir>` tolerates a draft without one.
#[test]
fn a_deployment_requires_ids() {
    let scratch = ScratchDir::new("compile-noid");
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{ "name": "No id", "tasks": [ { "id": "t", "name": "T",
             "function": { "name": "map", "input": { "mappings": [
               { "path": "data.ok", "logic": true } ] } } } ] }"#,
    )
    .unwrap();

    // `lint` accepts it: a directory being authored may not have chosen ids yet.
    let (ok, report) = run(&["lint", dir.to_str().unwrap()]);
    assert!(ok, "{report}");

    let (ok, report) = compile_deployment(dir, &dir.join("dist"));
    assert!(
        !ok,
        "a deployment cannot carry an id-less entity:\n{report}"
    );
}

/// A deployment is several files, so there is no stdout default.
#[test]
fn the_output_flag_is_required() {
    let scratch = sugared_set("compile-output");
    let (ok, report) = run(&["compile", scratch.path().to_str().unwrap()]);
    assert!(!ok, "compile without -o must refuse:\n{report}");
    assert!(report.contains("-o"), "{report}");
}

/// The fixture plugin's upload manifest and component, as a set carries them:
/// `plugin.toml` beside the component it names.
const PLUGIN_MANIFEST: &str =
    include_str!("../../../orion-server/tests/fixtures/plugins/fixture-upload.toml");
const PLUGIN_COMPONENT: &[u8] =
    include_bytes!("../../../orion-server/tests/fixtures/plugins/fixture.wasm");

/// A set whose workflow calls a plugin function, with the plugin's manifest
/// in the tree. `with_component` leaves the bytes out to model a manifest
/// whose component was never built.
fn plugin_set(label: &str, with_component: bool) -> ScratchDir {
    let scratch = ScratchDir::new(label);
    let dir = scratch.path();
    std::fs::create_dir_all(dir.join("codec")).unwrap();
    std::fs::write(dir.join("codec/plugin.toml"), PLUGIN_MANIFEST).unwrap();
    if with_component {
        std::fs::write(dir.join("codec/fixture.wasm"), PLUGIN_COMPONENT).unwrap();
    }
    std::fs::write(
        dir.join("wf.json"),
        r#"{ "workflow_id": "wrap", "name": "Wrap", "tasks": [
             { "id": "parse", "name": "Parse", "function": { "name": "parse_json",
               "input": { "source": "payload", "target": "input" } } },
             { "id": "wrap", "name": "Wrap", "function": { "name": "test.fixture.wrap",
               "input": { "message": { "var": "data.input.msg" }, "output": "data.result" } } } ] }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("ch.json"),
        r#"{ "channel_id": "wrap-api", "name": "wrap-api", "channel_type": "sync",
             "protocol": "rest", "methods": ["POST"], "route_pattern": "/wrap",
             "workflow_id": "wrap" }"#,
    )
    .unwrap();
    scratch
}

/// A set with a statement kept in a `.sql` file and no shared document —
/// the pipeline must still run.
fn sql_set(label: &str) -> ScratchDir {
    let scratch = ScratchDir::new(label);
    let dir = scratch.path();
    std::fs::create_dir_all(dir.join("sql")).unwrap();
    std::fs::create_dir_all(dir.join("orders")).unwrap();
    std::fs::write(
        dir.join("sql/recent.sql"),
        "-- The customer's orders, newest first.\nSELECT id,\n       total   -- cents\n  FROM orders\n WHERE customer_id = $1\n ORDER BY created_at DESC;\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("conn.json"),
        r#"{"name": "orders-db", "connector_type": "db",
            "config": {"driver": "sqlite", "connection_string": "sqlite::memory:"}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("orders/wf.json"),
        r#"{"workflow_id": "recent", "name": "Recent", "tasks": [
             {"id": "read", "name": "Read", "function": {"name": "db_read",
               "input": {"connector": "orders-db", "query": {"$sql": "../sql/recent.sql"},
                         "params": [{"var": "data.customer"}], "output": "data.rows"}}}]}"#,
    )
    .unwrap();
    scratch
}

/// #332: `$sql` compiles to the inline statement, in normal form, and a
/// comment edit moves neither the text nor the definition's digest.
#[test]
fn a_sql_file_compiles_to_the_inline_query() {
    let scratch = sql_set("compile-sql");
    let dir = scratch.path();
    let out = dir.join("a");
    let (ok, report) = compile_deployment(dir, &out);
    assert!(ok, "{report}");
    assert!(
        report.contains("compiled: shared.sql rewrote 1 document(s)"),
        "{report}"
    );
    let first = definition_of(&out, "workflow", "recent");
    assert_eq!(
        first["definition"]["tasks"][0]["function"]["input"]["query"],
        "SELECT id, total FROM orders WHERE customer_id = $1 ORDER BY created_at DESC"
    );
    // The statement is inlined, not copied beside the deployment.
    assert!(tree(&out).keys().all(|name| !name.ends_with(".sql")));

    std::fs::write(
        dir.join("sql/recent.sql"),
        "/* reworded */ SELECT id, total FROM orders -- still\nWHERE customer_id = $1 ORDER BY created_at DESC",
    )
    .unwrap();
    let out_b = dir.join("b");
    let (ok, report) = compile_deployment(dir, &out_b);
    assert!(ok, "{report}");
    assert_eq!(
        std::fs::read_to_string(out_b.join("deployment.json")).unwrap(),
        std::fs::read_to_string(out.join("deployment.json")).unwrap(),
        "a comment edit moves no digest"
    );
}

#[test]
fn a_missing_or_escaping_sql_file_writes_nothing() {
    let scratch = sql_set("compile-sql-missing");
    let dir = scratch.path();
    let wf = dir.join("orders/wf.json");
    let text = std::fs::read_to_string(&wf).unwrap();
    for (target, expected) in [
        ("../sql/nope.sql", "closure.sql_file"),
        ("../../outside.sql", "leaves the definition set"),
    ] {
        std::fs::write(&wf, text.replace("../sql/recent.sql", target)).unwrap();
        let out = dir.join("x");
        let (ok, report) = compile_deployment(dir, &out);
        assert!(!ok, "{target}: {report}");
        assert!(report.contains(expected), "{target}: {report}");
        assert!(!out.join("deployment.json").exists());
    }
}

/// A `plugin.toml` in the set compiles into a plugin definition with its
/// component inlined, and the workflow calling it is a member beside it.
#[test]
fn a_plugin_in_the_set_compiles_into_the_deployment_with_its_component() {
    use base64::Engine as _;
    let scratch = plugin_set("compile-plugin", true);
    let dir = scratch.path();
    let out = dir.join("dist");

    let (ok, report) = compile_deployment(dir, &out);
    assert!(ok, "{report}");
    assert!(
        report.contains("[plugin.manifest] plugin 'test.fixture'"),
        "the manifest is inventoried: {report}"
    );

    let plugin = definition_of(&out, "plugin", "test.fixture");
    assert_eq!(plugin["kind"], "plugin");
    let carried = base64::engine::general_purpose::STANDARD
        .decode(
            plugin["definition"]["component"]
                .as_str()
                .expect("component inlined"),
        )
        .expect("base64");
    assert_eq!(carried, PLUGIN_COMPONENT, "the bytes travel intact");
    definition_of(&out, "workflow", "wrap");
}

/// A detached signature beside the component — the file `orion-cli plugin
/// sign` writes — travels with the plugin, so a node with `[plugins.trust]`
/// can verify it. Without one the definition carries none.
#[test]
fn a_signature_beside_the_component_travels_with_the_plugin() {
    use base64::Engine as _;
    let scratch = plugin_set("compile-plugin-signed", true);
    let dir = scratch.path();
    let signature = base64::engine::general_purpose::STANDARD.encode([7u8; 64]);
    std::fs::write(dir.join("codec/fixture.wasm.sig"), format!("{signature}\n")).unwrap();
    let out = dir.join("dist");

    let (ok, report) = compile_deployment(dir, &out);
    assert!(ok, "{report}");
    let plugin = definition_of(&out, "plugin", "test.fixture");
    assert_eq!(plugin["definition"]["signature"], signature.as_str());

    let unsigned = plugin_set("compile-plugin-unsigned", true);
    let out = unsigned.path().join("dist");
    let (ok, report) = compile_deployment(unsigned.path(), &out);
    assert!(ok, "{report}");
    let plugin = definition_of(&out, "plugin", "test.fixture");
    assert!(
        plugin["definition"]
            .get("signature")
            .is_none_or(|s| s.is_null()),
        "{plugin}"
    );
}

/// A manifest with no component beside it lints — the workflow is checked
/// against the manifest — but cannot become a deployment: the deployment is
/// what carries the bytes, and one without them would fail at apply.
#[test]
fn a_deployment_refuses_a_manifest_whose_component_is_missing() {
    let scratch = plugin_set("compile-plugin-no-component", false);
    let dir = scratch.path();

    let (ok, report) = run(&["lint", dir.to_str().unwrap()]);
    assert!(ok, "the manifest alone validates the set: {report}");
    assert!(
        report.contains("no component beside the manifest"),
        "{report}"
    );

    let out = dir.join("dist");
    let (ok, report) = compile_deployment(dir, &out);
    assert!(!ok, "a deployment must carry the bytes: {report}");
    assert!(report.contains("test.fixture"), "{report}");
    assert!(
        !out.join("deployment.json").exists(),
        "nothing is written on refusal"
    );
}

/// #333: the example set's `$each`, value fragment and `{{seat}}` compile to
/// four ordinary steps, and the compiled workflow runs.
#[test]
fn the_unrolled_example_compiles_to_one_step_per_seat_and_runs() {
    let out = ScratchDir::new("unrolled-out");
    let example = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/packages/unrolled-seats"
    );
    let dir = out.path().join("compiled");
    let (ok, report) = compile_deployment(std::path::Path::new(example), &dir);
    assert!(ok, "{report}");
    let deployment = read_json(&dir.join("deployment.json"));
    let id = deployment["members"]
        .as_array()
        .and_then(|members| members.iter().find(|m| m["kind"] == "workflow"))
        .and_then(|m| m["id"].as_str())
        .expect("a workflow member")
        .to_string();
    let workflow = definition_of(&dir, "workflow", &id)["definition"].clone();
    let ids: Vec<&str> = workflow["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .filter_map(|t| t["id"].as_str())
        .collect();
    assert_eq!(ids, ["parse", "score0", "score1", "score2", "score3"]);
    assert!(
        orion_authoring::definitions::compile::residue(&workflow, "").is_empty(),
        "{workflow}"
    );

    let wf_path = out.path().join("workflow.json");
    let mut runnable = workflow.clone();
    runnable["id"] = serde_json::json!(id);
    std::fs::write(&wf_path, runnable.to_string()).unwrap();
    let input = out.path().join("input.json");
    std::fs::write(&input, r#"{"points": [3, 5, 0, 2]}"#).unwrap();
    let (ok, report) = run(&[
        "dry-run",
        "-w",
        wf_path.to_str().unwrap(),
        "-i",
        input.to_str().unwrap(),
    ]);
    assert!(ok, "{report}");
    assert!(
        report.contains(r#""seat1": 50"#) && report.contains(r#""seat3": 20"#),
        "{report}"
    );
}

// ============================================================
// --format deployment
// ============================================================

/// A set with one of each thing a deployment carries: a workflow on v1's
/// `continue_on_error` calling a connection through `connector_call` and
/// another through `http_call`, the channel serving it, an HTTP connector
/// whose token is a secret reference, and a cache connector.
fn deployment_set(label: &str) -> ScratchDir {
    let scratch = ScratchDir::new(label);
    let dir = scratch.path();
    std::fs::write(
        dir.join("conn-api.json"),
        r#"{ "name": "partner-api", "connector_type": "http", "tags": ["team:payments"],
             "config": { "type": "http", "url": "https://api.example.com",
                         "auth": { "type": "bearer", "token": "env://PARTNER_TOKEN" } } }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("conn-cache.json"),
        r#"{ "name": "local-cache", "connector_type": "cache",
             "config": { "type": "cache", "backend": "memory" } }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("wf.json"),
        r#"{ "workflow_id": "remember", "name": "Remember", "continue_on_error": true,
             "tasks": [
               { "id": "set", "name": "Set", "function": { "name": "connector_call", "input": {
                   "connection": "local-cache", "target_id": "org.orion.memory-cache",
                   "operation": "cache.set", "input": { "key": "k", "value": "v" },
                   "output": "data.set" } } },
               { "id": "ping", "name": "Ping", "function": { "name": "http_call", "input": {
                   "connector": "partner-api", "method": "GET", "output": "data.ping" } } },
               { "id": "done", "name": "Done", "function": { "name": "map", "input": {
                   "mappings": [ { "path": "data.done", "logic": true } ] } } } ] }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("ch.json"),
        r#"{ "channel_id": "remember-api", "name": "remember-api", "channel_type": "sync",
             "protocol": "rest", "methods": ["POST"], "route_pattern": "/remember",
             "workflow_id": "remember" }"#,
    )
    .unwrap();
    scratch
}

fn compile_deployment(dir: &std::path::Path, out: &std::path::Path) -> (bool, String) {
    run(&[
        "--output",
        "json",
        "compile",
        dir.to_str().unwrap(),
        "--format",
        "deployment",
        "--out",
        out.to_str().unwrap(),
    ])
}

fn read_json(path: &std::path::Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Every file of a deployment directory, by relative path.
fn tree(root: &std::path::Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut files = std::collections::BTreeMap::new();
    for sub in ["", "definitions", "connections"] {
        let Ok(entries) = std::fs::read_dir(root.join(sub)) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_file() {
                let name = path.strip_prefix(root).unwrap().display().to_string();
                files.insert(name, std::fs::read(&path).unwrap());
            }
        }
    }
    files
}

#[test]
fn a_deployment_is_written_without_revisions() {
    let scratch = deployment_set("compile-deployment");
    let out = scratch.path().join("dist");
    let (ok, report) = compile_deployment(scratch.path(), &out);
    assert!(ok, "{report}");
    // A connection has no tags, and the compiler says it left them behind.
    assert!(report.contains("a connection has no tags"), "{report}");

    let deployment = read_json(&out.join("deployment.json"));
    assert_eq!(deployment["schema_version"], 1);
    let raw = std::fs::read_to_string(out.join("deployment.json")).unwrap();
    assert!(!raw.contains("revision"), "no revisions anywhere:\n{raw}");

    // Members sorted by kind, then id.
    let members = deployment["members"].as_array().unwrap();
    let order: Vec<(&str, &str)> = members
        .iter()
        .map(|m| (m["kind"].as_str().unwrap(), m["id"].as_str().unwrap()))
        .collect();
    assert_eq!(
        order,
        [
            ("channel", "remember-api"),
            ("connection", "local-cache"),
            ("connection", "partner-api"),
            ("workflow", "remember"),
        ]
    );
    // A channel requires its workflow; a workflow every connection its
    // tasks name — through connector_call's `connection` and through a
    // built-in's `connector` alike.
    assert_eq!(
        members[0]["requires"],
        serde_json::json!([{"kind": "workflow", "id": "remember"}])
    );
    assert_eq!(
        members[3]["requires"],
        serde_json::json!([
            {"kind": "connection", "id": "local-cache"},
            {"kind": "connection", "id": "partner-api"},
        ])
    );
    assert_eq!(members[1]["connection"], "local-cache");

    // Each definition is a file named by the digest of its bytes.
    for member in members.iter().filter(|m| m["kind"] != "connection") {
        let digest = member["definition"].as_str().unwrap();
        let hex = digest.strip_prefix("sha256:").unwrap();
        let bytes = std::fs::read(out.join("definitions").join(format!("{hex}.json"))).unwrap();
        assert_eq!(orion_common::crypto::sha256_digest(&bytes), digest);
        let definition: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(definition["kind"], member["kind"]);
    }
    assert_eq!(
        std::fs::read_dir(out.join("definitions")).unwrap().count(),
        2
    );

    // The workflow: identity gone, v1's failure flag in the engine's words.
    let workflow_digest = members[3]["definition"].as_str().unwrap();
    let workflow = read_json(&out.join("definitions").join(format!(
        "{}.json",
        workflow_digest.trim_start_matches("sha256:")
    )));
    assert_eq!(workflow["definition"]["on_failure"], "continue");
    assert!(workflow["definition"].get("continue_on_error").is_none());
    assert!(workflow["definition"].get("workflow_id").is_none());
    let channel_digest = members[0]["definition"].as_str().unwrap();
    let channel = read_json(&out.join("definitions").join(format!(
        "{}.json",
        channel_digest.trim_start_matches("sha256:")
    )));
    assert!(channel["definition"].get("workflow_id").is_none());
    assert_eq!(channel["definition"]["route_pattern"], "/remember");
}

#[test]
fn a_connections_secret_references_become_its_locators() {
    let scratch = deployment_set("compile-deployment-secrets");
    let out = scratch.path().join("dist");
    let (ok, report) = compile_deployment(scratch.path(), &out);
    assert!(ok, "{report}");

    let api = read_json(&out.join("connections/partner-api.json"));
    assert_eq!(api["connection_id"], "partner-api");
    assert_eq!(api["driver"], "org.orion.http");
    assert_eq!(
        api["provider_digest"].as_str(),
        orion_runtime::extensions::foundation::builtin_provider_digest("org.orion.http").as_deref()
    );
    assert_eq!(
        api["secret_locators"],
        serde_json::json!({"auth.token": "env://PARTNER_TOKEN"})
    );
    // What stays is public, and the type is the driver's to say.
    assert_eq!(
        api["public_config"],
        serde_json::json!({"url": "https://api.example.com", "auth": {"type": "bearer"}})
    );

    let cache = read_json(&out.join("connections/local-cache.json"));
    assert_eq!(cache["driver"], "org.orion.memory-cache");
    assert_eq!(cache["secret_locators"], serde_json::json!({}));
}

/// Compiling the same set twice writes the same bytes, and a recompile into
/// the same directory leaves no file a definition the set no longer has.
#[test]
fn a_deployment_compiles_reproducibly() {
    let scratch = deployment_set("compile-deployment-repro");
    let dir = scratch.path();
    let (ok, report) = compile_deployment(dir, &dir.join("a"));
    assert!(ok, "{report}");
    let (ok, report) = compile_deployment(dir, &dir.join("b"));
    assert!(ok, "{report}");
    assert_eq!(tree(&dir.join("a")), tree(&dir.join("b")));

    // Change the workflow and recompile into `a`: one definition moves,
    // and the old one's file goes.
    let before = tree(&dir.join("a"));
    let wf = std::fs::read_to_string(dir.join("wf.json"))
        .unwrap()
        .replace("\"Remember\"", "\"Remember it\"");
    std::fs::write(dir.join("wf.json"), wf).unwrap();
    let (ok, report) = compile_deployment(dir, &dir.join("a"));
    assert!(ok, "{report}");
    let after = tree(&dir.join("a"));
    let gone: Vec<&String> = before.keys().filter(|k| !after.contains_key(*k)).collect();
    let new: Vec<&String> = after.keys().filter(|k| !before.contains_key(*k)).collect();
    assert_eq!((gone.len(), new.len()), (1, 1), "{gone:?} -> {new:?}");
    assert!(gone[0].starts_with("definitions/"), "{gone:?}");
}

#[test]
fn a_connector_type_this_build_lacks_writes_nothing() {
    let scratch = deployment_set("compile-deployment-storage");
    let dir = scratch.path();
    std::fs::write(
        dir.join("conn-bucket.json"),
        r#"{ "name": "bucket", "connector_type": "storage",
             "config": { "type": "storage", "endpoint": "https://s3.example.com",
                         "region": "us-east-1", "bucket": "b",
                         "access_key": "env://S3_KEY", "secret_key": "env://S3_SECRET" } }"#,
    )
    .unwrap();
    let out = dir.join("dist");
    let (ok, report) = compile_deployment(dir, &out);
    assert!(!ok, "{report}");
    assert!(
        report.contains("conn-bucket.json") && report.contains("unknown variant `storage`"),
        "{report}"
    );
    assert!(!out.join("deployment.json").exists());
}

#[test]
fn a_literal_credential_is_refused() {
    let scratch = deployment_set("compile-deployment-literal");
    let dir = scratch.path();
    std::fs::write(
        dir.join("conn-api.json"),
        r#"{ "name": "partner-api", "connector_type": "http",
             "config": { "type": "http", "url": "https://api.example.com",
                         "auth": { "type": "bearer", "token": "s3cr3t-literal" } } }"#,
    )
    .unwrap();
    let out = dir.join("dist");
    let (ok, report) = compile_deployment(dir, &out);
    assert!(!ok, "{report}");
    assert!(
        report.contains("connector 'partner-api'")
            && report.contains("'token'")
            && report.contains("references only"),
        "{report}"
    );
    assert!(
        !report.contains("s3cr3t-literal"),
        "the credential is never echoed: {report}"
    );
    assert!(!out.join("deployment.json").exists());
}

/// A db connector's driver comes from its `driver` field, so a connection
/// string behind a secret reference compiles: the reference becomes a
/// locator and the field becomes the provider, leaving `public_config`.
#[test]
fn a_db_connection_behind_a_reference_takes_its_driver_from_the_field() {
    let scratch = deployment_set("compile-deployment-db-driver");
    let dir = scratch.path();
    std::fs::write(
        dir.join("conn-db.json"),
        r#"{ "name": "orders-db", "connector_type": "db",
             "config": { "type": "db", "driver": "postgresql",
                         "connection_string": "env://ORDERS_DB_URL", "max_connections": 5 } }"#,
    )
    .unwrap();
    let out = dir.join("dist");
    let (ok, report) = compile_deployment(dir, &out);
    assert!(ok, "{report}");
    let db = read_json(&out.join("connections/orders-db.json"));
    assert_eq!(db["driver"], "org.orion.postgresql");
    assert_eq!(
        db["secret_locators"],
        serde_json::json!({"connection_string": "env://ORDERS_DB_URL"})
    );
    assert_eq!(
        db["public_config"],
        serde_json::json!({"max_connections": 5})
    );
}

/// In a definition set every db connector names its driver — the lint gate
/// refuses one without, with a stable check id, and so does every compile —
/// and a stated driver the visible scheme contradicts is refused too.
#[test]
fn a_set_db_connector_names_a_driver_its_scheme_agrees_with() {
    let scratch = deployment_set("lint-db-driver");
    let dir = scratch.path();
    std::fs::write(
        dir.join("conn-db.json"),
        r#"{ "name": "orders-db", "connector_type": "db",
             "config": { "connection_string": "env://ORDERS_DB_URL" } }"#,
    )
    .unwrap();
    let (ok, report) = run(&["lint", dir.to_str().unwrap()]);
    assert!(!ok, "{report}");
    assert!(
        report.contains("[connector.db_driver]") && report.contains("connector 'orders-db'"),
        "{report}"
    );
    let (ok, report) = compile_deployment(dir, &dir.join("dist"));
    assert!(!ok && report.contains("[connector.db_driver]"), "{report}");

    std::fs::write(
        dir.join("conn-db.json"),
        r#"{ "name": "orders-db", "connector_type": "db",
             "config": { "driver": "mysql", "connection_string": "postgres://db/app" } }"#,
    )
    .unwrap();
    let (ok, report) = run(&["lint", dir.to_str().unwrap()]);
    assert!(!ok, "{report}");
    assert!(
        report.contains("disagrees with the connection string") && !report.contains("db_driver]"),
        "{report}"
    );

    std::fs::write(
        dir.join("conn-db.json"),
        r#"{ "name": "orders-db", "connector_type": "db",
             "config": { "driver": "postgresql", "connection_string": "postgres://db/app" } }"#,
    )
    .unwrap();
    let (ok, report) = run(&["lint", dir.to_str().unwrap()]);
    assert!(ok, "{report}");
}

/// A key no definition defines names the entity and the key.
#[test]
fn a_key_a_definition_does_not_define_is_named() {
    let scratch = deployment_set("compile-deployment-unknown-key");
    let dir = scratch.path();
    std::fs::write(
        dir.join("ch.json"),
        r#"{ "channel_id": "remember-api", "name": "remember-api", "channel_type": "sync",
             "protocol": "rest", "methods": ["POST"], "route_pattern": "/remember",
             "workflow_id": "remember", "activate": false }"#,
    )
    .unwrap();
    let (ok, report) = compile_deployment(dir, &dir.join("dist"));
    assert!(!ok, "{report}");
    assert!(
        report.contains("channel 'remember-api'") && report.contains("`activate`"),
        "{report}"
    );
}
