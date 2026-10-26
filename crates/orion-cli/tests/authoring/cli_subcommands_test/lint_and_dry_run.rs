use super::*;

#[test]
fn lint_valid_workflow_exits_zero() {
    let wf = write_temp(
        r#"{
        "name": "smoke",
        "tasks": [
            {"id":"t1","name":"log","function":{"name":"log","input":{"message":"ok"}}}
        ]
    }"#,
        "lint-valid",
    );
    let out = Command::new(orion_bin())
        // Authoring must not construct the remote client or load the node's
        // configuration, even when those settings would be invalid.
        .args(["--server", "not a URL", "lint", &wf])
        .env("ORION_STORAGE__URL", "not a database URL")
        .output()
        .expect("invoke orion-cli lint");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout={stdout}, stderr={stderr}");
    assert!(stdout.contains("is valid"), "stdout={stdout}");
    let _ = std::fs::remove_file(&wf);
}

#[test]
fn lint_invalid_workflow_exits_nonzero_with_field_path() {
    // cache_read requires `connector` — omit it on purpose.
    let wf = write_temp(
        r#"{
        "name": "broken",
        "tasks": [
            {"id":"t1","name":"x","function":{"name":"cache_read","input":{"key":"k"}}}
        ]
    }"#,
        "lint-invalid",
    );
    let out = Command::new(orion_bin())
        .args(["lint", &wf])
        .output()
        .expect("invoke orion-cli lint");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "expected non-zero exit, stderr={stderr}"
    );
    assert!(
        stderr.contains("tasks[0].function.input.connector"),
        "stderr should name the missing field, got: {stderr}"
    );
    assert!(
        stderr.contains("REQUIRED"),
        "stderr should mention the REQUIRED code, got: {stderr}"
    );
    let _ = std::fs::remove_file(&wf);
}

/// A connector task's scalar fields are JSONLogic, and its document-shaped
/// fields are not — the two-tier split, asserted in one run.
///
/// `cache_read.key` composed with `cat` was impossible before dataflow-rs 3.9:
/// the field folded `{"var": …}` and stored every other node as the literal
/// object it was, so a per-tenant key had to be built in a `map` task first.
/// `cache_write.value` still folds, and must, because one `$` comes off every
/// key in a position the engine evaluates and a stored document may carry `$`
/// keys of its own.
#[test]
fn scalar_connector_fields_evaluate_and_documents_still_fold() {
    let wf = write_temp(
        r#"{
        "name": "two-tier",
        "tasks": [
            {"id":"seed","name":"seed","function":{"name":"map","input":{"mappings":[
                {"path":"data.tenant","logic":"acme"},
                {"path":"data.id","logic":42},
                {"path":"data.page","logic":5}]}}},
            {"id":"c","name":"c","function":{"name":"cache_read","input":{
                "connector":"cache",
                "key":{"cat":["tenant:",{"var":"data.tenant"},":order:",{"var":"data.id"}]},
                "output":"data.hit"}}},
            {"id":"m","name":"m","function":{"name":"cache_write","input":{
                "connector":"cache","key":"orders",
                "value":{"tenant":{"var":"data.tenant"},"$set":{"page":1}},
                "ttl_secs":{"var":"data.page"}}}}
        ]
    }"#,
        "two-tier-wf",
    );
    let input = write_temp(r#"{}"#, "two-tier-input");
    let stubs = write_temp(
        r#"{"cache_read":{"*":{"cached":true}},"cache_write":{"*":{"ok":true}}}"#,
        "two-tier-stubs",
    );
    let out = Command::new(orion_bin())
        .args(["dry-run", "-w", &wf, "-i", &input, "--stubs", &stubs])
        .output()
        .expect("invoke orion-cli dry-run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout={stdout}, stderr={stderr}");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json");

    // The expression field: evaluated, not stored as the operator node.
    assert_eq!(
        parsed["calls"]["cache_read"][0]["input"]["key"],
        serde_json::json!("tenant:acme:order:42"),
        "{stdout}"
    );
    // A scalar bound is evaluated too.
    assert_eq!(
        parsed["calls"]["cache_write"][0]["input"]["ttl_secs"], 5,
        "{stdout}"
    );
    // The document field: `{"var": …}` folded, everything else — the `$` key
    // included — left alone.
    assert_eq!(
        parsed["calls"]["cache_write"][0]["input"]["value"],
        serde_json::json!({"tenant": "acme", "$set": {"page": 1}}),
        "{stdout}"
    );

    for f in [&wf, &input, &stubs] {
        let _ = std::fs::remove_file(f);
    }
}

#[test]
fn dry_run_executes_workflow_and_prints_trace() {
    let wf = write_temp(
        r#"{
        "name": "dry-run-smoke",
        "tasks": [
            {"id":"t1","name":"log","function":{"name":"log","input":{"message":"hi"}}}
        ]
    }"#,
        "dry-run-wf",
    );
    let input = write_temp(r#"{"x": 1}"#, "dry-run-input");
    let out = Command::new(orion_bin())
        .args(["dry-run", "-w", &wf, "-i", &input])
        .output()
        .expect("invoke orion-cli dry-run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout={stdout}, stderr={stderr}");
    // Output is JSON with steps[].task_id.
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("dry-run output is not valid JSON: {e}\nstdout={stdout}");
    });
    let steps = parsed["trace"]["steps"]
        .as_array()
        .expect("trace.steps must be an array");
    assert!(steps.iter().any(|s| s["task_id"] == "t1"));
    let _ = std::fs::remove_file(&wf);
    let _ = std::fs::remove_file(&input);
}
