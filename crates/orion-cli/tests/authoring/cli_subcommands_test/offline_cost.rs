//! Offline runs cost what a node's run costs (#353)

use super::*;

/// A case runs the way a node runs an untraced message: 3000 sweeps finish in
/// a moment, where running them under the trace would take tens of gigabytes.
#[test]
fn a_long_loop_case_runs_at_a_nodes_cost() {
    let scratch = suite_with(LONG_LOOP_WORKFLOW);
    let dir = scratch.path();
    std::fs::write(
        dir.join("long.case.json"),
        r#"{"workflow": "wf.json", "input": {}, "expect": {"data.turns": 3000, "data.board": 2999}}"#,
    )
    .unwrap();

    let started = std::time::Instant::now();
    let (ok, out) = run_suite(dir);
    assert!(ok, "{out}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "a 3000-sweep case must not take {:?}",
        started.elapsed()
    );
}

/// `expect_tasks` does not read the trace. It lists a task once per sweep,
/// and still credits a task that failed.
#[test]
fn expect_tasks_counts_every_sweep_without_a_trace() {
    let scratch = suite_with(
        r#"{
        "name": "three-sweeps",
        "loop": {"max": 3},
        "tasks": [
            {"id":"a","name":"A","function":{"name":"log","input":{"message":"x"}}},
            {"id":"skipped","name":"Skipped","condition":false,
             "function":{"name":"log","input":{"message":"x"}}}
        ]
    }"#,
    );
    let dir = scratch.path();
    std::fs::write(
        dir.join("sweeps.case.json"),
        r#"{"workflow": "wf.json", "input": {}, "expect_tasks": ["a", "a", "a"]}"#,
    )
    .unwrap();

    let (ok, out) = run_suite(dir);
    assert!(ok, "{out}");
}

/// A fan-out's task runs once per element, and the record says so.
#[test]
fn expect_tasks_lists_a_fan_out_once_per_element() {
    let scratch = suite_with(
        r#"{
        "name": "fan-out",
        "tasks": [
            {"id":"fan","name":"Fan","for_each":{"over":[1, 2, 3],"as":"id"},
             "function":{"name":"http_call","input":{
                "connector":"crm","method":"GET","path":"/c"}}}
        ]
    }"#,
    );
    let dir = scratch.path();
    std::fs::write(
        dir.join("fan.case.json"),
        r#"{
            "workflow": "wf.json",
            "input": {},
            "stubs": {"http_call": {"crm": {"ok": true}}},
            "expect_tasks": ["fan", "fan", "fan"]
        }"#,
    )
    .unwrap();

    let (ok, out) = run_suite(dir);
    assert!(ok, "{out}");
}

/// Per-write capture is off unless a case reads it: a path rooted at
/// `audit_trail` still sees each write's changes.
#[test]
fn a_case_reading_audit_trail_changes_still_sees_them() {
    let scratch = suite_with(
        r#"{
        "name": "one-write",
        "tasks": [
            {"id":"w","name":"W","function":{"name":"map","input":{"mappings":[
                {"path":"data.x","logic":7}]}}}
        ]
    }"#,
    );
    let dir = scratch.path();
    std::fs::write(
        dir.join("changes.case.json"),
        r#"{
            "workflow": "wf.json",
            "input": {},
            "expect": {"audit_trail[0].changes[0].new_value": 7}
        }"#,
    )
    .unwrap();

    let (ok, out) = run_suite(dir);
    assert!(ok, "{out}");
}

/// Each step carries its own changes and no snapshot, so a dry run's trace
/// grows with its steps rather than their square: the context after a step
/// is replayed from `initial`, not stored with it.
#[test]
fn a_dry_run_trace_grows_with_its_steps_not_their_square() {
    let workflow = LONG_LOOP_WORKFLOW.replace("\"max\": 3000", "\"max\": 50");
    let out = dry_run_json(&workflow, &[]);
    assert!(out["trace"]["initial"].is_object(), "{}", out["trace"]);
    let steps: Vec<&serde_json::Value> = out["trace"]["steps"]
        .as_array()
        .expect("trace.steps")
        .iter()
        .filter(|s| s["task_id"].is_string())
        .collect();
    assert_eq!(steps.len(), 100);
    for step in &steps {
        assert!(step.get("message").is_none(), "a step holds no snapshot");
        let changes = step["changes"].as_array().expect("changes");
        assert!(
            changes.len() <= 3,
            "a step holds only its own writes, got {}",
            changes.len()
        );
    }
    assert_eq!(out["tasks"].as_array().map(Vec::len), Some(100));
    assert_eq!(out["matched"], true);
}

/// `--trace steps` keeps the path without changes; `--trace none` records
/// nothing but the task list.
#[test]
fn dry_run_trace_modes_shape_the_output() {
    let workflow = LONG_LOOP_WORKFLOW.replace("\"max\": 3000", "\"max\": 5");

    let steps = dry_run_json(&workflow, &["--trace", "steps"]);
    let recorded: Vec<&serde_json::Value> = steps["trace"]["steps"]
        .as_array()
        .expect("trace.steps")
        .iter()
        .filter(|s| s["task_id"].is_string())
        .collect();
    assert_eq!(recorded.len(), 10);
    assert!(
        recorded
            .iter()
            .all(|s| s["changes"].as_array().is_some_and(Vec::is_empty)),
        "{recorded:?}"
    );

    let none = dry_run_json(&workflow, &["--trace", "none"]);
    assert!(none.get("trace").is_none(), "{none}");
    assert_eq!(none["tasks"].as_array().map(Vec::len), Some(10));
    assert_eq!(none["data"]["turns"], 5);
}

/// `matched` is whether any task ran. It read "the trace has a step", and a
/// workflow whose condition was false records a skipped step.
#[test]
fn a_dry_run_whose_workflow_does_not_match_says_so() {
    let out = dry_run_json(
        r#"{
        "name": "never",
        "condition": false,
        "tasks": [{"id":"t","name":"T","function":{"name":"log","input":{"message":"x"}}}]
    }"#,
        &[],
    );
    assert_eq!(out["matched"], false, "{out}");
    assert_eq!(out["tasks"], serde_json::json!([]));
}

/// #351: a directory lint walks a loop's `setup` steps like the body's. A
/// connector used only there, and missing from the set, passed `lint` and
/// failed at run time.
#[test]
fn a_set_lint_sees_a_connector_used_only_in_loop_setup() {
    let scratch = suite_with(
        r#"{
        "workflow_id": "wf",
        "name": "wf",
        "loop": {"max": 2, "setup": [
            {"id": "prime", "name": "Prime", "function": {"name": "http_call", "input": {
                "connector": "ghost", "method": "GET", "path": "/p", "output": "data.p"}}}
        ]},
        "tasks": [{"id": "body", "name": "Body", "function": {"name": "log", "input": {"message": "x"}}}]
    }"#,
    );
    let out = Command::new(orion_bin())
        .args(["lint", scratch.path().to_str().unwrap()])
        .output()
        .expect("run lint");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.status.success(), "{combined}");
    assert!(
        combined.contains("[closure.connector]") && combined.contains("'ghost'"),
        "{combined}"
    );
}
