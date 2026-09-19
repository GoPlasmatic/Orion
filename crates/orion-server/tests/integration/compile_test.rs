//! `orion-server compile`: a definition set in, files the admin API accepts out.
//!
//! The command exists because nothing in the product performed the expansion a
//! deploy tool needs. `package export` reads a live server, which only ever
//! stored compiled documents, so the only path from `definitions/` to a
//! running instance was a tool that reimplemented the expander — and #295 is
//! what that costs when the reimplementation misses a case.
//!
//! What these assert, therefore, is not just "it wrote a file" but the two
//! properties that make the output trustworthy: **nothing source-form
//! survives**, and the artifact is the same kind of document `package export`
//! writes, hash included — which `package lint` is asked to confirm rather
//! than this test asserting it by hand.

use crate::common::{ScratchDir, orion_bin};
use std::process::Command;

fn run(args: &[&str]) -> (bool, String) {
    let out = Command::new(orion_bin())
        .args(args)
        .output()
        .expect("run orion-server");
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), combined)
}

/// A set that uses every authoring convenience at once: a fragment with an
/// argument, a `$from` in a task input, and a `$from` buried in a `map`
/// mapping's `logic` — the last being the one the admin API used to store
/// uncompiled.
fn sugared_set(label: &str) -> ScratchDir {
    // A distinct label per test: `ScratchDir` removes its directory on drop,
    // and these run in parallel, so a shared name lets one test delete the
    // files another is still reading.
    let scratch = ScratchDir::new(label);
    let dir = scratch.path();
    std::fs::create_dir_all(dir.join("fragments")).unwrap();
    std::fs::write(
        dir.join("common.json"),
        r#"{ "constants": { "db": { "connector": "mongo", "database": "app" } },
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
        r#"{ "name": "mongo", "connector_type": "db",
             "config": { "connection_string": "mongodb://host/app" } }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("wf.json"),
        r#"{ "workflow_id": "lookup", "name": "Lookup", "tasks": [
             { "id": "_g", "use": "deny", "with": { "message": "Please sign in." } },
             { "id": "read", "name": "Read", "function": { "name": "mongo_read",
               "input": { "$from": "constants.db", "collection": "users",
                          "filter": {}, "output": "temp_data.u" } } },
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
fn an_artifact_is_fully_compiled_and_passes_package_lint() {
    let scratch = sugared_set("compile-artifact");
    let dir = scratch.path();
    let out = dir.join("dist/package.json");

    let (ok, report) = run(&[
        "compile",
        dir.to_str().unwrap(),
        "--name",
        "demo",
        "--version",
        "1.0.0",
        "-o",
        out.to_str().unwrap(),
    ]);
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

    let raw = std::fs::read_to_string(&out).expect("artifact written");
    assert!(
        !raw.contains("$from") && !raw.contains("\"use\""),
        "no source form may survive compilation:\n{raw}"
    );

    let artifact: serde_json::Value = serde_json::from_str(&raw).expect("artifact is JSON");
    let tasks = &artifact["workflows"][0]["tasks"];
    // The fragment inlined, with its inner task's id namespaced by the call
    // site (`_g` + `write`) and its parameter substituted from `with`.
    assert_eq!(tasks[0]["id"], "_g.write");
    assert_eq!(
        tasks[0]["function"]["input"]["mappings"][0]["logic"],
        "Please sign in."
    );
    // Namespacing reaches inside the fragment's task group, and so do its
    // parameters (#294) — an artifact carrying a bare `log` here would collide
    // with any host task of that name, and with a second instance of the
    // fragment.
    assert_eq!(tasks[1]["id"], "_g.refused");
    assert_eq!(tasks[1]["tasks"][0]["id"], "_g.log");
    assert_eq!(
        tasks[1]["tasks"][0]["function"]["input"]["mappings"][0]["logic"],
        "Please sign in."
    );
    // The splice merged rather than replaced: the shared fields arrived and
    // the call site's own survived beside them.
    assert_eq!(tasks[2]["function"]["input"]["connector"], "mongo");
    assert_eq!(tasks[2]["function"]["input"]["database"], "app");
    assert_eq!(tasks[2]["function"]["input"]["collection"], "users");
    // The deep one — inside a mapping's `logic`, where the admin API used to
    // accept it with a 201 and run it verbatim.
    assert_eq!(
        tasks[3]["function"]["input"]["mappings"][0]["logic"]["body"],
        "User Not Found !"
    );
    // A directory has no stored status, so a compiled definition is meant to
    // run; `package apply` reads this.
    assert_eq!(artifact["workflows"][0]["activate"], true);
    assert_eq!(artifact["channels"][0]["activate"], true);

    // Asked of the package surface rather than asserted here: an artifact this
    // command writes must be indistinguishable from one `export` writes, and
    // the hash is the part a hand-rolled emitter would get wrong.
    let (ok, report) = run(&["package", "lint", "-f", out.to_str().unwrap()]);
    assert!(
        ok,
        "compile must emit an artifact package lint accepts:\n{report}"
    );
    assert!(report.contains("demo@1.0.0"), "{report}");
}

#[test]
fn no_activate_leaves_the_artifact_staging_only() {
    let scratch = sugared_set("compile-noactivate");
    let dir = scratch.path();
    let out = dir.join("dist/drafts.json");
    let (ok, report) = run(&[
        "compile",
        dir.to_str().unwrap(),
        "--name",
        "demo",
        "--version",
        "1.0.0",
        "--no-activate",
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(ok, "{report}");
    let artifact: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert!(artifact["workflows"][0]["activate"].is_null());
    assert!(artifact["channels"][0]["activate"].is_null());
}

#[test]
fn dir_format_mirrors_the_tree_and_consumes_the_catalog() {
    let scratch = sugared_set("compile-dir");
    let dir = scratch.path();
    let out = dir.join("dist-tree");
    let (ok, report) = run(&[
        "compile",
        dir.to_str().unwrap(),
        "--format",
        "dir",
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(ok, "{report}");

    // One file in, one file out, at the same relative path — so a diff of the
    // two trees is exactly what the compiler did.
    for name in ["wf.json", "ch.json", "conn.json"] {
        assert!(out.join(name).exists(), "{name} must be emitted");
    }
    // The catalog is the compiler's *input*. Copying it through would put a
    // document in the output that is no entity at all.
    assert!(!out.join("common.json").exists());
    assert!(!out.join("fragments/guard.json").exists());

    let wf = std::fs::read_to_string(out.join("wf.json")).unwrap();
    assert!(!wf.contains("$from") && !wf.contains("\"use\""), "{wf}");
    // No activation intent outside the artifact form: `activate` is a package
    // concept, and these files are POST bodies.
    assert!(!wf.contains("activate"), "{wf}");
}

#[test]
fn bulk_format_writes_one_array_per_kind() {
    let scratch = sugared_set("compile-bulk");
    let dir = scratch.path();
    let out = dir.join("dist-bulk");
    let (ok, report) = run(&[
        "compile",
        dir.to_str().unwrap(),
        "--format",
        "bulk",
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(ok, "{report}");
    for (file, expected) in [
        ("connectors.json", "mongo"),
        ("workflows.json", "Lookup"),
        ("channels.json", "lookup-api"),
    ] {
        let raw = std::fs::read_to_string(out.join(file)).expect(file);
        let entries: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(entries.as_array().map(Vec::len), Some(1), "{file}: {raw}");
        assert_eq!(entries[0]["name"], expected);
        assert!(
            !raw.contains("$from") && !raw.contains("\"use\""),
            "{file}: {raw}"
        );
    }
}

#[test]
fn a_reference_that_does_not_resolve_writes_nothing() {
    let scratch = sugared_set("compile-unresolved");
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{ "workflow_id": "lookup", "name": "Lookup", "tasks": [
             { "id": "read", "name": "Read", "function": { "name": "mongo_read",
               "input": { "$from": "constants.dbb", "collection": "users",
                          "filter": {}, "output": "temp_data.u" } } } ] }"#,
    )
    .unwrap();
    let out = dir.join("dist/broken.json");

    let (ok, report) = run(&[
        "compile",
        dir.to_str().unwrap(),
        "--name",
        "demo",
        "--version",
        "1.0.0",
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(!ok, "{report}");
    assert!(report.contains("constants.dbb"), "{report}");
    assert!(
        !out.exists(),
        "a refused compile must leave no artifact behind — a stale one would \
         apply cleanly and be wrong"
    );
}

/// `compile` gates with `lint <dir>`'s own pass, so a set that fails the linter
/// cannot be emitted. Without this, an artifact reaches `package apply` having
/// passed CI and fails there instead.
#[test]
fn the_gate_is_the_linters_own() {
    let scratch = sugared_set("compile-gate");
    let dir = scratch.path();
    std::fs::write(
        dir.join("dangling.json"),
        r#"{ "workflow_id": "dangling", "name": "Dangling", "tasks": [
             { "id": "c", "name": "c", "function": { "name": "mongo_read",
               "input": { "connector": "no-such-connector", "database": "x",
                          "collection": "y", "filter": {}, "output": "temp_data.z" } } } ] }"#,
    )
    .unwrap();
    let out = dir.join("dist/pkg.json");
    let (ok, report) = run(&[
        "compile",
        dir.to_str().unwrap(),
        "--name",
        "demo",
        "--version",
        "1.0.0",
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(!ok, "{report}");
    assert!(report.contains("no-such-connector"), "{report}");
    assert!(!out.exists());

    // ...and the boundary flags are the linter's too: a name declared external
    // is not a dangling reference, it is a `requires` entry.
    let (ok, report) = run(&[
        "compile",
        dir.to_str().unwrap(),
        "--name",
        "demo",
        "--version",
        "1.0.0",
        "--requires-connector",
        "no-such-connector",
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(ok, "{report}");
    let artifact: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(artifact["requires"]["connectors"][0], "no-such-connector");
}

/// An artifact's entities are referenced by id across the file — a channel
/// names its `workflow_id`, and activation intent is keyed on it — so `compile`
/// demands ids where `lint <dir>` tolerates a draft without one.
#[test]
fn the_artifact_form_requires_ids() {
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

    let out = dir.join("pkg.json");
    let (ok, report) = run(&[
        "compile",
        dir.to_str().unwrap(),
        "--name",
        "demo",
        "--version",
        "1.0.0",
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(!ok, "an artifact cannot carry an id-less entity:\n{report}");

    // The other two forms emit request bodies, and the API derives an id from
    // the name exactly as it does for a hand-written POST — so demanding one
    // would refuse a set that deploys correctly today. Leaving `channel_id`
    // out is how the 62-channel set that motivated this command is authored.
    for format in ["dir", "bulk"] {
        let (ok, report) = run(&[
            "compile",
            dir.to_str().unwrap(),
            "--format",
            format,
            "-o",
            dir.join(format).to_str().unwrap(),
        ]);
        assert!(
            ok,
            "--format {format} must accept an id-less entity:\n{report}"
        );
    }
}

#[test]
fn the_output_flag_is_required_where_several_files_are_written() {
    let scratch = sugared_set("compile-output");
    let dir = scratch.path();
    for format in ["dir", "bulk"] {
        let (ok, report) = run(&["compile", dir.to_str().unwrap(), "--format", format]);
        assert!(!ok, "--format {format} without -o must refuse:\n{report}");
        assert!(report.contains("-o"), "{report}");
    }
    // The artifact form is one document, so stdout is a sensible default.
    let (ok, report) = run(&[
        "compile",
        dir.to_str().unwrap(),
        "--name",
        "demo",
        "--version",
        "1.0.0",
    ]);
    assert!(ok, "{report}");
    assert!(report.contains("\"package\""), "{report}");
}

#[test]
fn the_artifact_form_needs_a_name_and_version() {
    let scratch = sugared_set("compile-nameversion");
    let (ok, report) = run(&["compile", scratch.path().to_str().unwrap()]);
    assert!(!ok, "{report}");
    assert!(report.contains("--name"), "{report}");
}

/// Compiling twice must produce the same bytes: the artifact is hashed, and a
/// hash that moved between two runs over an unchanged set would make every
/// re-apply look like a content change and collide with receipt immutability.
#[test]
fn compilation_is_reproducible() {
    let scratch = sugared_set("compile-repro");
    let dir = scratch.path();
    let hash = |file: &str| -> String {
        let out = dir.join(file);
        let (ok, report) = run(&[
            "compile",
            dir.to_str().unwrap(),
            "--name",
            "demo",
            "--version",
            "1.0.0",
            "-o",
            out.to_str().unwrap(),
        ]);
        assert!(ok, "{report}");
        let artifact: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        artifact["package"]["content_hash"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(hash("a.json"), hash("b.json"));
}

/// Compile `dir` with `--version <version>` (and any extra flags) into
/// `out`, returning the artifact and the report.
fn compile_versioned(
    dir: &std::path::Path,
    out: &str,
    extra: &[&str],
) -> (bool, String, serde_json::Value) {
    let target = dir.join(out);
    let mut args = vec![
        "compile",
        dir.to_str().unwrap(),
        "--name",
        "demo",
        "-o",
        target.to_str().unwrap(),
    ];
    args.extend_from_slice(extra);
    let (ok, report) = run(&args);
    let artifact = if ok {
        serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap()
    } else {
        serde_json::Value::Null
    };
    (ok, report, artifact)
}

/// #339: `--version content` names the package after its own content hash.
#[test]
fn a_content_version_is_the_hash_prefix() {
    let scratch = sugared_set("compile-content-version");
    let dir = scratch.path();
    let (ok, report, artifact) = compile_versioned(dir, "a.json", &["--version", "content"]);
    assert!(ok, "{report}");
    let hash = artifact["package"]["content_hash"].as_str().unwrap();
    let version = artifact["package"]["version"].as_str().unwrap();
    assert_eq!(version, format!("content-{}", &hash[7..19]));
    assert!(
        report.contains(&format!("wrote demo@{version} (")),
        "{report}"
    );
    let (ok, report) = run(&[
        "package",
        "lint",
        "-f",
        dir.join("a.json").to_str().unwrap(),
    ]);
    assert!(ok, "{report}");

    // A hand-edited version that names other content is caught offline.
    let mut edited = artifact.clone();
    edited["package"]["version"] = serde_json::json!("content-000000000000");
    std::fs::write(dir.join("edited.json"), edited.to_string()).unwrap();
    let (ok, report) = run(&[
        "package",
        "lint",
        "-f",
        dir.join("edited.json").to_str().unwrap(),
    ]);
    assert!(!ok, "{report}");
    assert!(
        report.contains("names content the entities do not hash to") && report.contains(version),
        "{report}"
    );

    // With a prefix.
    let (ok, report, artifact) = compile_versioned(
        dir,
        "b.json",
        &["--version", "content", "--version-prefix", "1.4.0"],
    );
    assert!(ok, "{report}");
    assert_eq!(
        artifact["package"]["version"],
        format!("1.4.0-{}", &hash[7..19])
    );
}

/// The version moves exactly when the content does: a second compile of the
/// same tree, or one whose files were only reformatted, keeps it; a task
/// edit moves it.
#[test]
fn a_content_version_is_stable_across_compiles_and_moves_with_content() {
    let scratch = sugared_set("compile-content-stable");
    let dir = scratch.path();
    let version = |out: &str| {
        let (ok, report, artifact) = compile_versioned(dir, out, &["--version", "content"]);
        assert!(ok, "{report}");
        artifact["package"]["version"].as_str().unwrap().to_string()
    };
    let first = version("a.json");
    assert_eq!(version("b.json"), first);

    // Whitespace and key order are not content.
    let wf = dir.join("wf.json");
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&wf).unwrap()).unwrap();
    std::fs::write(&wf, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    assert_eq!(version("c.json"), first);

    // A task edit is.
    let text = std::fs::read_to_string(&wf).unwrap();
    std::fs::write(&wf, text.replace("\"users\"", "\"accounts\"")).unwrap();
    assert_ne!(version("d.json"), first);
}

#[test]
fn a_version_prefix_needs_the_content_keyword() {
    let scratch = sugared_set("compile-prefix-alone");
    let (ok, report, _) = compile_versioned(
        scratch.path(),
        "a.json",
        &["--version", "1.0.0", "--version-prefix", "1.4.0"],
    );
    assert!(!ok);
    assert!(
        report.contains("--version-prefix only applies with --version content"),
        "{report}"
    );
}

/// A version the target would refuse with a `400` at `apply` is refused
/// where it is chosen, before anything is written.
#[test]
fn a_version_the_receipt_would_refuse_is_refused_at_compile() {
    let scratch = sugared_set("compile-bad-version");
    let dir = scratch.path();
    for (flags, expected) in [
        (
            &["--version", "1.0/rc"][..],
            "--version contains unsupported character '/'",
        ),
        (
            &["--version", "content", "--version-prefix", "1.4.0+build"][..],
            "--version-prefix contains unsupported character '+'",
        ),
    ] {
        let (ok, report, _) = compile_versioned(dir, "a.json", flags);
        assert!(!ok, "{flags:?}");
        assert!(report.contains(expected), "{report}");
        assert!(!dir.join("a.json").exists());
    }
}

/// What the hash does not see cannot move a content version: a rollout-only
/// change keeps it, and the compile says so rather than letting a re-apply
/// be a silent no-op.
#[test]
fn a_rollout_does_not_move_a_content_version() {
    let scratch = sugared_set("compile-content-rollout");
    let dir = scratch.path();
    let (ok, report, artifact) = compile_versioned(dir, "a.json", &["--version", "content"]);
    assert!(ok, "{report}");
    assert!(!report.contains("rollout_percentage"), "{report}");
    let wf = dir.join("wf.json");
    let mut doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&wf).unwrap()).unwrap();
    doc["rollout_percentage"] = serde_json::json!(25);
    std::fs::write(&wf, doc.to_string()).unwrap();
    let (ok, report, rolled) = compile_versioned(dir, "b.json", &["--version", "content"]);
    assert!(ok, "{report}");
    assert_eq!(rolled["package"]["version"], artifact["package"]["version"]);
    assert!(
        report.contains("rollout_percentage is not part of the content hash"),
        "{report}"
    );
}

/// The fixture plugin's upload manifest and component, as a set carries them:
/// `plugin.toml` beside the component it names.
const PLUGIN_MANIFEST: &str = include_str!("../fixtures/plugins/fixture-upload.toml");
const PLUGIN_COMPONENT: &[u8] = include_bytes!("../fixtures/plugins/fixture.wasm");

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
            "config": {"connection_string": "sqlite::memory:"}}"#,
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

/// #332: `$sql` compiles to the inline statement, in normal form; a comment
/// edit moves neither the text nor the hash, and `package lint` accepts the
/// result.
#[test]
fn a_sql_file_compiles_to_the_inline_query() {
    let scratch = sql_set("compile-sql");
    let dir = scratch.path();
    let (ok, report, first) = compile_versioned(dir, "a.json", &["--version", "1.0.0"]);
    assert!(ok, "{report}");
    assert!(
        report.contains("compiled: shared.sql rewrote 1 document(s)"),
        "{report}"
    );
    assert_eq!(
        first["workflows"][0]["tasks"][0]["function"]["input"]["query"],
        "SELECT id, total FROM orders WHERE customer_id = $1 ORDER BY created_at DESC"
    );
    let (ok, report) = run(&[
        "package",
        "lint",
        "-f",
        dir.join("a.json").to_str().unwrap(),
    ]);
    assert!(ok, "{report}");

    std::fs::write(
        dir.join("sql/recent.sql"),
        "/* reworded */ SELECT id, total FROM orders -- still\nWHERE customer_id = $1 ORDER BY created_at DESC",
    )
    .unwrap();
    let (ok, report, second) = compile_versioned(dir, "b.json", &["--version", "1.0.0"]);
    assert!(ok, "{report}");
    assert_eq!(
        second["package"]["content_hash"],
        first["package"]["content_hash"]
    );

    // `--format dir` consumes the file rather than copying it.
    let out = dir.join("out-dir");
    let (ok, report) = run(&[
        "compile",
        dir.to_str().unwrap(),
        "--format",
        "dir",
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(ok, "{report}");
    assert!(!out.join("sql/recent.sql").exists());
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
        let (ok, report, _) = compile_versioned(dir, "x.json", &["--version", "1.0.0"]);
        assert!(!ok, "{target}: {report}");
        assert!(report.contains(expected), "{target}: {report}");
        assert!(!dir.join("x.json").exists());
    }
}

/// #343: the set's `package` document names the artifact and carries its
/// range into `requires.orion`, which is not content; `--name` and
/// `--requires-orion` win over it.
#[test]
fn compile_carries_the_declared_package_into_the_artifact() {
    let scratch = sugared_set("compile-package-decl");
    let dir = scratch.path();
    std::fs::write(
        dir.join("package.json"),
        r#"{"package": {"name": "orders", "requires": {"orion": ">=1.0.0, <99"}}}"#,
    )
    .unwrap();
    let compile = |out: &str, extra: &[&str]| -> (bool, String, serde_json::Value) {
        let target = dir.join(out);
        let mut args = vec![
            "compile",
            dir.to_str().unwrap(),
            "--version",
            "1.0.0",
            "-o",
            target.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        let (ok, report) = run(&args);
        let artifact = if ok {
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap()
        } else {
            serde_json::Value::Null
        };
        (ok, report, artifact)
    };
    let (ok, report, declared) = compile("a.json", &[]);
    assert!(ok, "{report}");
    assert_eq!(declared["package"]["name"], "orders");
    assert_eq!(declared["requires"]["orion"], ">=1.0.0, <99");
    let (ok, report) = run(&[
        "package",
        "lint",
        "-f",
        dir.join("a.json").to_str().unwrap(),
    ]);
    assert!(ok, "{report}");

    // The flags win, with a note for the name.
    let (ok, report, flagged) = compile(
        "b.json",
        &["--name", "billing", "--requires-orion", ">=1.2, <99"],
    );
    assert!(ok, "{report}");
    assert!(
        report.contains("--name 'billing' overrides package.name 'orders'"),
        "{report}"
    );
    assert_eq!(flagged["package"]["name"], "billing");
    assert_eq!(flagged["requires"]["orion"], ">=1.2, <99");
    // A range is a gate, not content.
    assert_eq!(
        flagged["package"]["content_hash"],
        declared["package"]["content_hash"]
    );

    // An artifact this binary is too old for stops `package lint` first.
    let mut too_new = declared.clone();
    too_new["requires"]["orion"] = serde_json::json!(">=99.0.0");
    std::fs::write(dir.join("too-new.json"), too_new.to_string()).unwrap();
    let (ok, report) = run(&[
        "package",
        "lint",
        "-f",
        dir.join("too-new.json").to_str().unwrap(),
    ]);
    assert!(!ok);
    assert!(
        report.contains("orders@1.0.0 requires Orion >=99.0.0"),
        "{report}"
    );
    too_new["requires"]["orion"] = serde_json::json!("not a range");
    std::fs::write(dir.join("bad.json"), too_new.to_string()).unwrap();
    let (ok, report) = run(&[
        "package",
        "lint",
        "-f",
        dir.join("bad.json").to_str().unwrap(),
    ]);
    assert!(!ok);
    assert!(report.contains("is not a version range"), "{report}");

    // Without the document, the name is required.
    std::fs::remove_file(dir.join("package.json")).unwrap();
    let (ok, report, _) = compile("c.json", &[]);
    assert!(!ok);
    assert!(
        report.contains("or declare package.name in the set"),
        "{report}"
    );
}

/// #340: a build-time signer's `.sig` files land in the artifact's entries,
/// and — a signature not being content — the hash does not move.
#[test]
fn compile_signatures_lands_in_the_entries_and_package_lint_accepts_it() {
    let scratch = plugin_set("compile-signatures", true);
    let dir = scratch.path();
    let sigs = ScratchDir::new("compile-signatures-sigs");
    let key = orion::crypto::ed25519::SigningKey::generate();
    let signature = key.sign(&orion::crypto::sha256_digest(PLUGIN_COMPONENT));
    std::fs::write(
        sigs.path().join("fixture.wasm.sig"),
        format!("{signature}\n"),
    )
    .unwrap();

    let compile = |out: &str, extra: &[&str]| -> serde_json::Value {
        let target = dir.join(out);
        let mut args = vec![
            "compile",
            dir.to_str().unwrap(),
            "--name",
            "codec",
            "--version",
            "1.0.0",
            "-o",
            target.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        let (ok, report) = run(&args);
        assert!(ok, "{report}");
        serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap()
    };
    let unsigned = compile("unsigned.json", &[]);
    let signed = compile(
        "signed.json",
        &["--signatures", sigs.path().to_str().unwrap()],
    );
    assert_eq!(signed["plugins"][0]["signature"], signature);
    assert!(unsigned["plugins"][0].get("signature").is_none());
    assert_eq!(
        signed["package"]["content_hash"],
        unsigned["package"]["content_hash"]
    );
    let (ok, report) = run(&[
        "package",
        "lint",
        "-f",
        dir.join("signed.json").to_str().unwrap(),
    ]);
    assert!(ok, "{report}");

    // A carried signature that is not one is caught offline.
    let mut garbage = signed.clone();
    garbage["plugins"][0]["signature"] = serde_json::json!("not base64!");
    std::fs::write(dir.join("garbage.json"), garbage.to_string()).unwrap();
    let (ok, report) = run(&[
        "package",
        "lint",
        "-f",
        dir.join("garbage.json").to_str().unwrap(),
    ]);
    assert!(!ok);
    assert!(
        report.contains("plugins[0].signature: not base64"),
        "{report}"
    );
}

/// A misnamed `.sig` must not leave a plugin silently unsigned: the file no
/// subject claims stops the compile, naming the names that would match.
#[test]
fn an_orphan_sig_fails_compile_and_writes_nothing() {
    let scratch = plugin_set("compile-orphan-sig", true);
    let dir = scratch.path();
    let sigs = ScratchDir::new("compile-orphan-sigs");
    let key = orion::crypto::ed25519::SigningKey::generate();
    std::fs::write(sigs.path().join("fixtures.wasm.sig"), key.sign("sha256:00")).unwrap();
    let out = dir.join("out.json");
    let (ok, report) = run(&[
        "compile",
        dir.to_str().unwrap(),
        "--name",
        "codec",
        "--version",
        "1.0.0",
        "--signatures",
        sigs.path().to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(!ok, "{report}");
    assert!(
        report.contains("fixtures.wasm.sig matches no plugin or model")
            && report.contains("test.fixture.sig")
            && report.contains("fixture.wasm.sig"),
        "{report}"
    );
    assert!(!out.exists());
}

/// A `plugin.toml` in the set compiles into the artifact with its component
/// inlined and its digest computed, marked for activation like the workflow
/// that calls it — and `package lint` accepts the result, validating the
/// workflow's input against the manifest that travels with it.
#[test]
fn a_plugin_in_the_set_compiles_into_the_artifact_with_its_component() {
    use base64::Engine as _;
    let scratch = plugin_set("compile-plugin", true);
    let dir = scratch.path();
    let out = dir.join("dist/package.json");

    let (ok, report) = run(&[
        "compile",
        dir.to_str().unwrap(),
        "--name",
        "codec",
        "--version",
        "1.0.0",
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(ok, "{report}");
    assert!(
        report.contains("[plugin.manifest] plugin 'test.fixture'"),
        "the manifest is inventoried: {report}"
    );
    assert!(report.contains("1 plugins,"), "{report}");

    let artifact: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).expect("artifact")).expect("json");
    let plugin = &artifact["plugins"][0];
    assert_eq!(plugin["plugin_id"], "test.fixture");
    assert_eq!(plugin["activate"], true);
    assert_eq!(
        plugin["digest"],
        serde_json::json!(orion::plugin::WasmRuntime::digest(PLUGIN_COMPONENT))
    );
    let carried = base64::engine::general_purpose::STANDARD
        .decode(plugin["component"].as_str().expect("component inlined"))
        .expect("base64");
    assert_eq!(carried, PLUGIN_COMPONENT, "the bytes travel intact");
    assert_eq!(plugin["manifest"]["name"], "test.fixture");

    let (ok, report) = run(&["package", "lint", "-f", out.to_str().unwrap()]);
    assert!(ok, "{report}");
    assert!(report.contains("1 plugins,"), "{report}");

    // `--no-activate` leaves the plugin a draft too.
    let drafts = dir.join("dist/drafts.json");
    let (ok, report) = run(&[
        "compile",
        dir.to_str().unwrap(),
        "--name",
        "codec",
        "--version",
        "1.0.1",
        "--no-activate",
        "-o",
        drafts.to_str().unwrap(),
    ]);
    assert!(ok, "{report}");
    let artifact: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&drafts).expect("artifact")).expect("json");
    assert!(
        artifact["plugins"][0].get("activate").is_none(),
        "{artifact}"
    );

    // The bulk form writes the same items to plugins.json.
    let bulk = dir.join("dist/bulk");
    let (ok, report) = run(&[
        "compile",
        dir.to_str().unwrap(),
        "--format",
        "bulk",
        "-o",
        bulk.to_str().unwrap(),
    ]);
    assert!(ok, "{report}");
    let items: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(bulk.join("plugins.json")).expect("bulk"))
            .expect("json");
    assert_eq!(items[0]["plugin_id"], "test.fixture");
    assert!(items[0]["component"].is_string());
}

/// A manifest with no component beside it lints — the workflow is checked
/// against the manifest — but cannot become an artifact: the artifact is
/// what carries the bytes, and one without them would fail at apply.
#[test]
fn an_artifact_refuses_a_manifest_whose_component_is_missing() {
    let scratch = plugin_set("compile-plugin-no-component", false);
    let dir = scratch.path();

    let (ok, report) = run(&["lint", dir.to_str().unwrap()]);
    assert!(ok, "the manifest alone validates the set: {report}");
    assert!(
        report.contains("no component beside the manifest"),
        "{report}"
    );

    let out = dir.join("dist/package.json");
    let (ok, report) = run(&[
        "compile",
        dir.to_str().unwrap(),
        "--name",
        "codec",
        "--version",
        "1.0.0",
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(!ok, "an artifact must carry the bytes: {report}");
    assert!(report.contains("test.fixture"), "{report}");
    assert!(
        report.contains("no component beside the manifest"),
        "{report}"
    );
    assert!(!out.exists(), "nothing is written on refusal");
}

/// The fixture's bytes, for the digest the artifact must name.
const MODEL_ONNX: &[u8] = include_bytes!("../fixtures/models/c4-tiny/c4-tiny.onnx");
const MODEL_MANIFEST: &str = include_str!("../fixtures/models/c4-tiny/model.json");

/// A set with the fixture model in it: the manifest — with a `reference`
/// unless told otherwise — the graph beside it unless told otherwise, and a
/// workflow that calls it behind a channel.
fn model_set(label: &str, with_reference: bool, with_artifact: bool) -> ScratchDir {
    let scratch = ScratchDir::new(label);
    let dir = scratch.path();
    std::fs::create_dir_all(dir.join("models/c4")).unwrap();
    let mut manifest: serde_json::Value = serde_json::from_str(MODEL_MANIFEST).unwrap();
    if with_reference {
        manifest["reference"] = serde_json::json!({"connector": "models", "key": "c4/0.1.0.onnx"});
    }
    std::fs::write(
        dir.join("models/c4/model.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
    if with_artifact {
        std::fs::write(dir.join("models/c4/c4-tiny.onnx"), MODEL_ONNX).unwrap();
    }
    std::fs::write(
        dir.join("wf.json"),
        r#"{ "workflow_id": "score", "name": "Score", "tasks": [
             { "id": "parse", "name": "Parse", "function": { "name": "parse_json",
               "input": { "source": "payload", "target": "board" } } },
             { "id": "infer", "name": "Infer", "function": { "name": "model_infer",
               "input": { "model": "ada.c4-tiny", "input": { "var": "" }, "output": "data.policy" } } } ] }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("ch.json"),
        r#"{ "channel_id": "score-api", "name": "score-api", "channel_type": "sync",
             "protocol": "rest", "methods": ["POST"], "route_pattern": "/score",
             "workflow_id": "score" }"#,
    )
    .unwrap();
    scratch
}

/// A model manifest in the set compiles into the artifact as the fifth
/// member: the manifest without its local path, the reference the manifest
/// names, the digest of the file beside it, marked for activation — and the
/// storage connector it is fetched through goes to `requires.storage`.
/// `package lint` accepts the result, and the bulk form writes the same
/// items to `models.json`.
#[test]
fn a_model_in_the_set_compiles_into_the_artifact_with_its_reference() {
    let scratch = model_set("compile-model", true, true);
    let dir = scratch.path();
    let out = dir.join("dist/package.json");

    let (ok, report) = run(&[
        "compile",
        dir.to_str().unwrap(),
        "--name",
        "score",
        "--version",
        "1.0.0",
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(ok, "{report}");
    assert!(
        report.contains("[model.manifest] model 'ada.c4-tiny'"),
        "{report}"
    );
    assert!(report.contains("[model.stats]"), "{report}");
    assert!(report.contains("1 models,"), "{report}");

    let artifact: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).expect("artifact")).expect("json");
    let model = &artifact["models"][0];
    assert_eq!(model["model_id"], "ada.c4-tiny");
    assert_eq!(model["activate"], true);
    assert_eq!(model["artifact"]["connector"], "models");
    assert_eq!(model["artifact"]["key"], "c4/0.1.0.onnx");
    assert_eq!(
        model["artifact"]["digest"],
        serde_json::json!(orion::crypto::sha256_digest(MODEL_ONNX))
    );
    assert!(
        model["manifest"].get("artifact").is_none(),
        "the local path does not travel: {model}"
    );
    assert_eq!(model["manifest"]["name"], "ada.c4-tiny");
    assert_eq!(model["manifest"]["reference"]["key"], "c4/0.1.0.onnx");
    assert!(
        model.get("component").is_none() && model.get("bytes").is_none(),
        "a model travels as a reference, never bytes: {model}"
    );
    assert_eq!(
        artifact["requires"]["storage"],
        serde_json::json!(["models"])
    );
    assert!(artifact["requires"].get("models").is_none());

    let (ok, report) = run(&["package", "lint", "-f", out.to_str().unwrap()]);
    assert!(ok, "{report}");
    assert!(report.contains("1 models,"), "{report}");

    // `--no-activate` leaves the model a draft too.
    let drafts = dir.join("dist/drafts.json");
    let (ok, report) = run(&[
        "compile",
        dir.to_str().unwrap(),
        "--name",
        "score",
        "--version",
        "1.0.1",
        "--no-activate",
        "-o",
        drafts.to_str().unwrap(),
    ]);
    assert!(ok, "{report}");
    let artifact: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&drafts).expect("artifact")).expect("json");
    assert!(
        artifact["models"][0].get("activate").is_none(),
        "{artifact}"
    );

    // The bulk form writes the same items to models.json, and the dir form
    // mirrors the manifest with its graph.
    let bulk = dir.join("dist/bulk");
    let (ok, report) = run(&[
        "compile",
        dir.to_str().unwrap(),
        "--format",
        "bulk",
        "-o",
        bulk.to_str().unwrap(),
    ]);
    assert!(ok, "{report}");
    let items: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(bulk.join("models.json")).expect("bulk"))
            .expect("json");
    assert_eq!(items[0]["model_id"], "ada.c4-tiny");
    assert_eq!(items[0]["artifact"]["connector"], "models");
    let mirrored = dir.join("dist/mirror");
    let (ok, report) = run(&[
        "compile",
        dir.to_str().unwrap(),
        "--format",
        "dir",
        "-o",
        mirrored.to_str().unwrap(),
    ]);
    assert!(ok, "{report}");
    assert!(mirrored.join("models/c4/model.json").is_file());
    assert_eq!(
        std::fs::read(mirrored.join("models/c4/c4-tiny.onnx")).expect("copied"),
        MODEL_ONNX
    );
}

/// A manifest with no `reference`, or no artifact beside it, lints — the
/// workflow's reference resolves against the manifest alone — but cannot
/// become an artifact, and the refusal names what to add.
#[test]
fn an_artifact_refuses_a_model_missing_its_reference_or_its_bytes() {
    let scratch = model_set("compile-model-no-reference", false, true);
    let dir = scratch.path();
    let (ok, report) = run(&["lint", dir.to_str().unwrap()]);
    assert!(ok, "the manifest alone validates the set: {report}");
    let out = dir.join("dist/package.json");
    let (ok, report) = run(&[
        "compile",
        dir.to_str().unwrap(),
        "--name",
        "score",
        "--version",
        "1.0.0",
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(!ok, "an artifact must name where the bytes are: {report}");
    assert!(report.contains("ada.c4-tiny"), "{report}");
    assert!(
        report.contains("reference = { connector, key }"),
        "{report}"
    );
    assert!(!out.exists(), "nothing is written on refusal");

    let scratch = model_set("compile-model-no-bytes", true, false);
    let dir = scratch.path();
    let (ok, report) = run(&["lint", dir.to_str().unwrap()]);
    assert!(ok, "{report}");
    assert!(report.contains("[model.artifact_missing]"), "{report}");
    let out = dir.join("dist/package.json");
    let (ok, report) = run(&[
        "compile",
        dir.to_str().unwrap(),
        "--name",
        "score",
        "--version",
        "1.0.0",
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(!ok, "an artifact must carry the digest: {report}");
    assert!(
        report.contains("no artifact beside the manifest"),
        "{report}"
    );
    assert!(!out.exists(), "nothing is written on refusal");
}
