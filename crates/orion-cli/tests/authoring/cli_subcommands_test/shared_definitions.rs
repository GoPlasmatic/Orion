//! #285: shared definition sources — fragments and a value catalog

use super::*;

/// The whole point: one catalog entry, spliced, so an error string cannot
/// drift into three spellings across the set.
#[test]
fn a_shared_reference_expands_and_runs() {
    let scratch = temp_defs_with_shared();
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"name":"w","condition":true,"tasks":[
            {"id":"g","use":"deny","with":{"message":"Please sign in again."}},
            {"id":"err","name":"Error body","function":{"name":"map","input":{"mappings":[
               {"path":"data.out","logic":{"$from":"errors.USER_NOT_FOUND"}}]}}}]}"#,
    )
    .unwrap();
    let input = write_temp("{}", "shared-in");

    let out = Command::new(orion_bin())
        .args([
            "dry-run",
            "-w",
            dir.join("wf.json").to_str().unwrap(),
            "-i",
            &input,
            "--definitions",
            dir.to_str().unwrap(),
        ])
        .output()
        .expect("run dry-run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");

    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("JSON");
    assert_eq!(
        parsed["data"]["denied"], "Please sign in again.",
        "the call site's argument must beat the fragment's default"
    );
    assert_eq!(parsed["data"]["out"]["body"], "User Not Found !");
    // Namespaced, so two instances of one fragment cannot collide.
    assert_eq!(parsed["calls"], serde_json::json!({}));
    assert_eq!(parsed["trace"]["steps"][0]["task_id"], "g.write");

    let _ = std::fs::remove_file(&input);
}

/// Without a catalog, an unexpanded reference reaches validation as a task
/// missing its `name` and `function` — an error describing the symptom. The
/// command names the cause instead.
#[test]
fn a_reference_without_a_catalog_names_the_cause() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"name":"w","tasks":[{"id":"g","use":"deny"}]}"#,
    )
    .unwrap();
    let out = Command::new(orion_bin())
        .args(["lint", dir.join("wf.json").to_str().unwrap()])
        .output()
        .expect("run lint");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("fragment 'deny'") && stderr.contains("--definitions"),
        "the error must name the reference and the missing flag: {stderr}"
    );
}

/// A typo'd reference fails at lint, which is the whole reason the set lint
/// (#286) is this feature's prerequisite.
#[test]
fn an_unresolvable_reference_fails_the_set_lint() {
    let scratch = temp_defs_with_shared();
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"workflow_id":"w","name":"w","tasks":[
            {"id":"r","name":"r","function":{"name":"data_query","input":{
               "$from":"constants.dbb","query":{"source":"users"},
               "output":"temp_data.u"}}}]}"#,
    )
    .unwrap();
    let (ok, report) = lint_dir(dir, &[]);
    assert!(!ok, "{report}");
    assert!(report.contains("closure.shared_value"), "{report}");
    assert!(report.contains("constants.dbb"), "{report}");
}

/// In set mode the catalog is found without a flag, and the splice satisfies
/// the schema — `connector` comes from the shared value, so a failure to
/// splice would surface as a REQUIRED error.
#[test]
fn set_mode_resolves_the_catalog_without_a_flag() {
    let scratch = temp_defs_with_shared();
    let dir = scratch.path();
    std::fs::write(
        dir.join("conn.json"),
        r#"{"name":"pg","connector_type":"db",
            "config":{"driver":"postgresql","connection_string":"postgres://h/app"}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"workflow_id":"w","name":"w","tasks":[
            {"id":"r","name":"r","function":{"name":"data_query","input":{
               "$from":"constants.db","query":{"source":"users"},
               "output":"temp_data.u"}}}]}"#,
    )
    .unwrap();
    let (ok, report) = lint_dir(dir, &[]);
    assert!(
        ok,
        "the spliced connector must satisfy the schema: {report}"
    );
    assert!(
        report.contains("2 shared value(s), 1 fragment(s)"),
        "the summary must report the catalog: {report}"
    );
}

/// A sibling key overrides the shared value, which is what lets one field be
/// changed at a call site without copying the rest.
#[test]
fn a_call_site_can_override_one_field_of_a_shared_value() {
    let scratch = temp_defs_with_shared();
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"name":"w","condition":true,"tasks":[
            {"id":"m","name":"m","function":{"name":"map","input":{"mappings":[
               {"path":"data.x","logic":{"$from":"constants.db","database":"other"}}]}}}]}"#,
    )
    .unwrap();
    let input = write_temp("{}", "override-in");
    let out = Command::new(orion_bin())
        .args([
            "dry-run",
            "-w",
            dir.join("wf.json").to_str().unwrap(),
            "-i",
            &input,
            "--definitions",
            dir.to_str().unwrap(),
        ])
        .output()
        .expect("run dry-run");
    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("JSON");
    assert_eq!(parsed["data"]["x"]["database"], "other", "sibling wins");
    assert_eq!(
        parsed["data"]["x"]["connector"], "pg",
        "the rest is spliced"
    );
    let _ = std::fs::remove_file(&input);
}
