//! #286: lint a definition set, not one file at a time

use super::*;

/// #286's own reproduction: a workflow that lints clean per-file because both
/// of its dangling references live in files `lint <file>` never opens.
#[test]
fn set_mode_catches_references_a_single_file_lint_cannot_see() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("probe.json"),
        r#"{"workflow_id":"probe","name":"probe","tasks":[
            {"id":"c","name":"c","function":{"name":"data_query","input":{
              "connector":"does-not-exist","query":{"source":"y"},
              "output":"temp_data.z"}}},
            {"id":"d","name":"d","function":{"name":"channel_call","input":{
              "channel":"no-such-channel","data":{},"output":"temp_data.q"}}}]}"#,
    )
    .unwrap();

    // Per-file: unchanged, still clean — that is the bug being demonstrated.
    let out = Command::new(orion_bin())
        .args(["lint", dir.join("probe.json").to_str().unwrap()])
        .output()
        .expect("run lint");
    assert!(
        out.status.success(),
        "per-file lint must keep its behaviour"
    );

    let (ok, report) = lint_dir(dir, &[]);
    assert!(!ok, "set mode must fail: {report}");
    assert!(report.contains("closure.connector"), "{report}");
    assert!(report.contains("closure.channel_call"), "{report}");
}

/// A reference the set deliberately does not contain is declarable, the way a
/// package declares `requires`.
#[test]
fn a_boundary_admits_a_reference_the_set_does_not_contain() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"workflow_id":"w","name":"w","tasks":[
            {"id":"d","name":"d","function":{"name":"channel_call","input":{
              "channel":"deployed-elsewhere","data":{},"output":"temp_data.q"}}}]}"#,
    )
    .unwrap();

    let (ok, report) = lint_dir(dir, &[]);
    assert!(!ok, "empty boundary is the default: {report}");

    let (ok, report) = lint_dir(dir, &["--requires-channel", "deployed-elsewhere"]);
    assert!(ok, "a declared boundary must satisfy closure: {report}");
}

/// Entities are found by shape, and a fixture beside them is reported as
/// skipped rather than silently dropped or misread as a broken entity.
#[test]
fn non_entities_are_reported_not_silently_ignored() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::create_dir_all(dir.join("nested")).unwrap();
    std::fs::write(
        dir.join("nested/wf.json"),
        r#"{"workflow_id":"w","name":"w","tasks":[{"id":"t1","name":"t1","function":{"name":"map","input":{"mappings":[]}}}]}"#,
    )
    .unwrap();
    // The shape `examples/packages/*/request.json` has.
    std::fs::write(dir.join("request.json"), r#"{"data":{"amount":5}}"#).unwrap();
    std::fs::write(dir.join("broken.json"), r#"{oops"#).unwrap();

    let (_, report) = lint_dir(dir, &[]);
    assert!(
        report.contains("request.json is not a channel, workflow or connector"),
        "a skipped file must be named: {report}"
    );
    assert!(
        report.contains("broken.json is not readable JSON"),
        "a file that does not parse is a likely mistake, not a quiet skip: {report}"
    );
    assert!(
        report.contains("1 workflow(s)"),
        "the nested entity must be found — a one-level walk would miss it: {report}"
    );
}

/// A directory that yields nothing is an error, never a green run — the same
/// rule `orion-cli test` applies to a suite that matched no cases.
#[test]
fn a_directory_with_no_definitions_is_an_error() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(dir.join("notes.md"), "nothing here").unwrap();
    let (ok, report) = lint_dir(dir, &[]);
    assert!(!ok, "an empty set must not look like a pass: {report}");
    assert!(report.contains("no definitions found"), "{report}");
}

/// Two channels claiming one route are served by whichever loads second,
/// which is not a property anyone chose.
#[test]
fn a_route_claimed_twice_is_an_error() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"workflow_id":"w","name":"w","tasks":[{"id":"t1","name":"t1","function":{"name":"map","input":{"mappings":[]}}}]}"#,
    )
    .unwrap();
    for (i, name) in ["a", "b"].iter().enumerate() {
        std::fs::write(
            dir.join(format!("ch{i}.json")),
            format!(
                r#"{{"channel_id":"c{i}","name":"{name}","channel_type":"sync","protocol":"rest",
                    "route_pattern":"/users","methods":["GET"],"workflow_id":"w"}}"#
            ),
        )
        .unwrap();
    }
    let (ok, report) = lint_dir(dir, &[]);
    assert!(!ok, "{report}");
    assert!(report.contains("duplicate.route_pattern"), "{report}");
    assert!(
        report.contains("GET /users"),
        "the diff must name the route: {report}"
    );
}

/// A connector of the wrong type parses, imports, activates, and fails at the
/// first request. Set mode is where it can be caught offline.
#[test]
fn a_connector_of_the_wrong_type_is_an_error() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("conn.json"),
        r#"{"name":"pg","connector_type":"db",
            "config":{"driver":"postgresql","connection_string":"postgres://h/d"}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"workflow_id":"w","name":"w","tasks":[
            {"id":"t","name":"t","function":{"name":"http_call","input":{
              "connector":"pg","method":"GET","path":"/x","output":"data.r"}}}]}"#,
    )
    .unwrap();
    let (ok, report) = lint_dir(dir, &[]);
    assert!(!ok, "{report}");
    assert!(report.contains("type.connector"), "{report}");
    assert!(
        report.contains("needs a http connector"),
        "the message must name both types: {report}"
    );
}

/// Warnings do not fail set mode, and `--deny-warnings` promotes them — the
/// same flag and the same meaning it already has for a single file.
#[test]
fn set_mode_warnings_gate_only_under_deny_warnings() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"workflow_id":"w","name":"w","tasks":[
            {"id":"t","name":"t","function":{"name":"cache_write","input":{
              "connector":"m","key":"k",
              "value":{"at":{"cat":["a","b"]}}}}}]}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("conn.json"),
        r#"{"name":"m","connector_type":"cache","config":{"backend":"memory"}}"#,
    )
    .unwrap();

    let (ok, report) = lint_dir(dir, &[]);
    assert!(ok, "a warning must not fail set mode: {report}");
    assert!(report.contains("logic.unresolvable"), "{report}");

    let (ok, _) = lint_dir(dir, &["--deny-warnings"]);
    assert!(
        !ok,
        "--deny-warnings must gate the same way it does per-file"
    );
}

/// A `$`-prefixed key in a template position loses one `$` when the engine
/// emits it, so a MongoDB update document composed in a `map` task turns from
/// `{"$set": …}` into `{"set": …}` and the write replaces the document instead
/// of updating it. Nothing fails — which is exactly why `lint` has to say so.
#[test]
fn set_mode_warns_about_a_silently_escaped_template_key() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"workflow_id":"w","name":"w","tasks":[
            {"id":"t","name":"t","function":{"name":"map","input":{"mappings":[
              {"path":"data.update","logic":{"$set":{"seen":true}}}]}}}]}"#,
    )
    .unwrap();

    let (ok, report) = lint_dir(dir, &[]);
    assert!(ok, "an advisory must not fail set mode: {report}");
    assert!(report.contains("logic.escaped_template_key"), "{report}");
}

/// The doubled spelling is the documented fix, so warning about it would leave
/// an author who has already fixed the problem with a warning they cannot clear
/// and a `--deny-warnings` build that can never go green. The engine reports
/// every `$`-prefixed key — right for an audit, wrong for a gate.
#[test]
fn the_deliberate_doubled_spelling_is_not_warned_about() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"workflow_id":"w","name":"w","tasks":[
            {"id":"t","name":"t","function":{"name":"map","input":{"mappings":[
              {"path":"data.update","logic":{"$$set":{"seen":true}}}]}}}]}"#,
    )
    .unwrap();

    let (ok, report) = lint_dir(dir, &["--deny-warnings"]);
    assert!(
        ok,
        "doubling the prefix is the fix, and must clear the warning: {report}"
    );
}

/// A destination is JSONLogic too, so one task can fan its results out by
/// message content instead of writing every branch to a fixed path.
///
/// The clippy analysis has to stay honest about this: a computed destination is
/// an *unknown* write, not the absence of one, or every overwrite rule
/// concludes "nothing later writes this" from a step that might write anything.
#[test]
fn a_computed_output_destination_writes_where_the_message_says() {
    let wf = write_temp(
        r#"{
        "name": "fan-out",
        "tasks": [
            {"id":"seed","name":"seed","function":{"name":"map","input":{"mappings":[
                {"path":"data.tenant","logic":"acme"}]}}},
            {"id":"r","name":"r","function":{"name":"cache_read","input":{
                "connector":"cache","key":"k",
                "output":{"cat":["data.by_tenant.",{"var":"data.tenant"}]}}}}
        ]
    }"#,
        "fan-out-wf",
    );
    let input = write_temp(r#"{}"#, "fan-out-input");
    let stubs = write_temp(r#"{"cache_read":{"*":{"hit":1}}}"#, "fan-out-stubs");
    let out = Command::new(orion_bin())
        .args(["dry-run", "-w", &wf, "-i", &input, "--stubs", &stubs])
        .output()
        .expect("invoke orion-cli dry-run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json");
    assert_eq!(
        parsed["data"]["by_tenant"]["acme"],
        serde_json::json!({"hit": 1}),
        "{stdout}"
    );

    for f in [&wf, &input, &stubs] {
        let _ = std::fs::remove_file(f);
    }
}

/// The repository's own packages are a real definition set and must lint
/// clean — the closure checks resolve `channel_call` and connector references
/// across nine packages.
#[test]
fn the_shipped_examples_lint_clean_as_a_set() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/packages");
    let (ok, report) = lint_dir(&dir, &[]);
    assert!(
        ok,
        "the shipped packages must lint clean as a set: {report}"
    );
    assert!(report.contains("0 error(s)"), "{report}");
}
