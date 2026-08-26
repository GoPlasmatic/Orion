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
    std::fs::write(
        dir.join("fragments/guard.json"),
        r#"{ "fragments": { "deny": {
              "params": { "message": { "default": "Denied." } },
              "tasks": [ { "id": "write", "name": "Write the refusal",
                "function": { "name": "map", "input": { "mappings": [
                  { "path": "data.denied", "logic": { "$param": "message" } } ] } } } ] } } }"#,
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
    // The splice merged rather than replaced: the shared fields arrived and
    // the call site's own survived beside them.
    assert_eq!(tasks[1]["function"]["input"]["connector"], "mongo");
    assert_eq!(tasks[1]["function"]["input"]["database"], "app");
    assert_eq!(tasks[1]["function"]["input"]["collection"], "users");
    // The deep one — inside a mapping's `logic`, where the admin API used to
    // accept it with a 201 and run it verbatim.
    assert_eq!(
        tasks[2]["function"]["input"]["mappings"][0]["logic"]["body"],
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
