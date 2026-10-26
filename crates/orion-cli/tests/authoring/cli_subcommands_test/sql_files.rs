//! #332: `$sql` — statements kept in `.sql` files

use super::*;

#[test]
fn single_file_commands_resolve_sql_without_definitions() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::create_dir_all(dir.join("sql")).unwrap();
    std::fs::write(dir.join("sql/one.sql"), "SELECT 1 AS one -- a constant\n").unwrap();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"workflow_id":"wf","name":"wf","condition":true,"tasks":[
             {"id":"r","name":"r","function":{"name":"db_read","input":{
               "connector":"db","query":{"$sql":"sql/one.sql"},"output":"data.rows"}}}]}"#,
    )
    .unwrap();
    std::fs::write(dir.join("in.json"), r#"{"data": {}}"#).unwrap();
    let wf = dir.join("wf.json");
    let (ok, stdout, stderr) = run_bin(&["lint", wf.to_str().unwrap()]);
    assert!(ok, "{stdout}{stderr}");

    // dry-run runs what the file says — the stub answers the call, and the
    // call log carries the inlined statement.
    let stubs = dir.join("stubs.json");
    std::fs::write(&stubs, r#"{"db_read": {"db": [{"one": 1}]}}"#).unwrap();
    let (ok, stdout, stderr) = run_bin(&[
        "dry-run",
        "-w",
        wf.to_str().unwrap(),
        "-i",
        dir.join("in.json").to_str().unwrap(),
        "--stubs",
        stubs.to_str().unwrap(),
    ]);
    assert!(ok, "{stdout}{stderr}");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json");
    assert_eq!(
        parsed["calls"]["db_read"][0]["input"]["query"], "SELECT 1 AS one",
        "{stdout}"
    );
}

/// A finding about the statement names the `.sql` file it came from.
#[test]
fn a_statement_finding_names_its_sql_file() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::create_dir_all(dir.join("sql")).unwrap();
    std::fs::write(
        dir.join("sql/purge.sql"),
        "DELETE FROM sessions WHERE expired\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("conn.json"),
        r#"{"name":"db","connector_type":"db","config":{"driver":"sqlite","connection_string":"sqlite::memory:"}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"workflow_id":"wf","name":"wf","tasks":[
             {"id":"p","name":"p","function":{"name":"db_read","input":{
               "connector":"db","query":{"$sql":"sql/purge.sql"},"output":"data.rows"}}}]}"#,
    )
    .unwrap();
    let (ok, report) = lint_dir(dir, &[]);
    assert!(!ok, "{report}");
    assert!(
        report.contains("[sql.read_only]")
            && report.contains("(in ")
            && report.contains("sql/purge.sql"),
        "{report}"
    );
}
