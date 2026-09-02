use crate::common;

use crate::common::{body_json, json_request};
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

// ============================================================
// 1. Dry-run with matching condition and computed output
// ============================================================

#[tokio::test]
async fn test_dry_run_with_matching_condition() {
    let app = common::test_app().await;

    // Create a workflow with parse_json -> map. The test endpoint puts the
    // request body's "data" field into the message payload. parse_json copies
    // the payload into data.input, then map computes data.computed from it.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "dry-run-match",
                "name": "Dry Run Match",
                "condition": true,
                "tasks": [
                    {
                        "id": "t0",
                        "name": "Parse payload",
                        "function": {
                            "name": "parse_json",
                            "input": { "source": "payload", "target": "input" }
                        }
                    },
                    {
                        "id": "t1",
                        "name": "Compute result",
                        "function": {
                            "name": "map",
                            "input": {
                                "mappings": [{
                                    "path": "data.computed",
                                    "logic": {"*": [{"var": "data.input.x"}, 2]}
                                }]
                            }
                        }
                    }
                ]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Dry-run the workflow with input data
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/dry-run-match/test",
            Some(json!({"data": {"x": 21}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;

    // Should match (condition is true)
    assert_eq!(body["data"]["matched"], true);

    // Output should contain the computed value
    assert_eq!(body["data"]["output"]["computed"], 42);

    // Trace should be present and non-empty
    assert!(body["data"]["trace"].is_object());
    let steps = body["data"]["trace"]["steps"].as_array().unwrap();
    assert!(!steps.is_empty(), "trace should contain at least one step");

    // No errors
    let errors = body["data"]["errors"].as_array().unwrap();
    assert!(errors.is_empty(), "should have no errors");
}

// ============================================================
// 2. Dry-run with unmatched condition
// ============================================================

#[tokio::test]
async fn test_dry_run_unmatched_condition() {
    let app = common::test_app().await;

    // Create a workflow that first parses payload into data.input, then
    // has a condition that checks data.input.priority > 5.
    // We use parse_json as the first task with condition: true on the
    // workflow. BUT the condition is at the workflow level, so it is
    // evaluated BEFORE any tasks run. For the condition to access the
    // data, we need it available in the context before processing.
    //
    // Since the test endpoint spreads request data into payload (not context.data),
    // the workflow condition cannot access it. A condition like
    // {">": [{"var": "data.priority"}, 5]} evaluates against context.data
    // which is empty, so it will always be false.
    //
    // We test this scenario: the workflow condition is unmet, so only a
    // workflow-level skip step is recorded and no tasks execute.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "dry-run-unmatch",
                "name": "Dry Run Unmatched",
                "condition": {">": [{"var": "data.priority"}, 5]},
                "tasks": [
                    {
                        "id": "t1",
                        "name": "Log",
                        "function": {
                            "name": "log",
                            "input": {"message": "should not run"}
                        }
                    }
                ]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Dry-run with priority = 1. The condition evaluates against the
    // message context where data starts empty (payload is separate),
    // so the condition {> [null, 5]} evaluates to false.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/dry-run-unmatch/test",
            Some(json!({"data": {"priority": 1}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;

    // The trace should contain a workflow-level skip step
    let steps = body["data"]["trace"]["steps"].as_array().unwrap();
    assert!(!steps.is_empty(), "trace should have at least one step");

    // All steps should be "skipped" — no task was actually executed
    let all_skipped = steps.iter().all(|s| s["result"] == "skipped");
    assert!(
        all_skipped,
        "all steps should be skipped when condition is unmet"
    );

    // No task-level steps should be present (task_id should be null for workflow skip)
    let has_task_execution = steps.iter().any(|s| s["task_id"].is_string());
    assert!(
        !has_task_execution,
        "no tasks should have executed when workflow condition is false"
    );

    // Output should be empty (no data was produced)
    assert!(
        body["data"]["output"]
            .as_object()
            .is_none_or(|o| o.is_empty()),
        "output should be empty when workflow is skipped"
    );
}

// ============================================================
// 3. Dry-run with connector functions (cache_write)
// ============================================================

#[tokio::test]
async fn test_dry_run_with_connector_functions() {
    let app = common::test_app().await;

    // Create a memory cache connector
    let connector_id =
        common::create_connector(&app, common::cache_connector_memory("dry-run-cache")).await;
    assert!(!connector_id.is_empty());

    // Create a workflow that writes to the cache
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "dry-run-cache-wf",
                "name": "Dry Run Cache Write",
                "condition": true,
                "tasks": [
                    {
                        "id": "t1",
                        "name": "Write to cache",
                        "function": {
                            "name": "cache_write",
                            "input": {
                                "connector": "dry-run-cache",
                                "key": "test",
                                "value": "dry-run-value"
                            }
                        }
                    }
                ]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Dry-run the workflow — cache_write should execute successfully
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/dry-run-cache-wf/test",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;

    assert_eq!(body["data"]["matched"], true);

    // No errors from the cache_write execution
    let errors = body["data"]["errors"].as_array().unwrap();
    assert!(
        errors.is_empty(),
        "cache_write should not produce errors: {errors:?}"
    );
}

// ============================================================
// 4. Versioning: create, list, and activation lifecycle
// ============================================================

#[tokio::test]
async fn test_versioning_create_and_list() {
    let app = common::test_app().await;

    // Create workflow v1 (draft)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "ver-lifecycle",
                "name": "Version Lifecycle v1",
                "condition": true,
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"v1"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["version"], 1);
    assert_eq!(body["data"]["status"], "draft");

    // Activate v1
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/workflows/ver-lifecycle/status",
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["status"], "active");
    assert_eq!(body["data"]["version"], 1);

    // Create new draft version (v2)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/ver-lifecycle/versions",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["version"], 2);
    assert_eq!(body["data"]["status"], "draft");

    // The new draft is editable in place
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/workflows/ver-lifecycle",
            Some(json!({"name": "Version Lifecycle v2"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["name"], "Version Lifecycle v2");

    // List all versions — should see at least v1 and v2
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/workflows/ver-lifecycle/versions",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let total = body["total"].as_i64().unwrap();
    assert!(total >= 2, "should have at least 2 versions, got {total}");

    let versions = body["data"].as_array().unwrap();
    // Every version row carries its workflow_id
    assert!(versions.iter().all(|v| v.get("workflow_id").is_some()));
    let v1 = versions.iter().find(|v| v["version"] == 1).unwrap();
    let v2 = versions.iter().find(|v| v["version"] == 2).unwrap();
    assert_eq!(v1["status"], "active");
    assert_eq!(v2["status"], "draft");

    // Activate v2 — v1 should become archived
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/workflows/ver-lifecycle/status",
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["version"], 2);
    assert_eq!(body["data"]["status"], "active");

    // Verify v1 is now archived by listing versions again
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/workflows/ver-lifecycle/versions",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let versions = body["data"].as_array().unwrap();
    let v1 = versions.iter().find(|v| v["version"] == 1).unwrap();
    let v2 = versions.iter().find(|v| v["version"] == 2).unwrap();
    assert_eq!(
        v1["status"], "archived",
        "v1 should be archived after v2 activation"
    );
    assert_eq!(v2["status"], "active", "v2 should be active");
}

// ============================================================
// 5. Import/export round-trip across two app instances
// ============================================================

#[tokio::test]
async fn test_import_export_round_trip() {
    let app1 = common::test_app().await;

    // Create two workflows on app1
    let resp = app1
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "export-rt-1",
                "name": "Export Round Trip 1",
                "condition": true,
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"wf1"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Activate first workflow
    let resp = app1
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/workflows/export-rt-1/status",
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app1
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "export-rt-2",
                "name": "Export Round Trip 2",
                "condition": {">": [{"var": "data.score"}, 50]},
                "tasks": [
                    {"id":"t1","name":"Map","function":{"name":"map","input":{"mappings":[{"path":"data.result","logic":"passed"}]}}}
                ]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Activate second workflow
    let resp = app1
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/workflows/export-rt-2/status",
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Export all workflows from app1
    let resp = app1
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/workflows/export", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let export_body = body_json(resp).await;
    let exported = export_body["data"].as_array().unwrap();
    assert!(
        exported.len() >= 2,
        "should have at least 2 exported workflows, got {}",
        exported.len()
    );

    // Create a fresh test app (separate in-memory DB)
    let app2 = common::test_app().await;

    // Build import payload from the exported data. The import endpoint accepts
    // an array of workflow creation objects — we need name, condition, and tasks.
    let import_payload: Vec<serde_json::Value> = exported
        .iter()
        .map(|wf| {
            json!({
                "workflow_id": wf["workflow_id"],
                "name": wf["name"],
                "condition": wf.get("condition").cloned().unwrap_or(json!(true)),
                "tasks": wf["tasks"],
            })
        })
        .collect();

    // Import into app2
    let resp = app2
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/import",
            Some(json!(import_payload)),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let import_body = body_json(resp).await;
    assert_eq!(
        import_body["data"]["imported"].as_i64().unwrap(),
        exported.len() as i64,
        "all exported workflows should import successfully"
    );
    assert_eq!(import_body["data"]["failed"], 0);
    assert!(import_body["data"]["errors"].as_array().unwrap().is_empty());

    // Verify workflows exist on app2 as drafts
    let resp = app2
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/workflows", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let list_body = body_json(resp).await;
    let listed = list_body["data"].as_array().unwrap();
    assert!(
        listed.len() >= 2,
        "app2 should have at least 2 workflows after import"
    );

    // Imported workflows should be in draft status
    for wf in listed {
        assert_eq!(
            wf["status"], "draft",
            "imported workflows should be drafts, got: {}",
            wf["status"]
        );
    }
}

// ============================================================
// 6. Validate endpoint: multiple scenarios
// ============================================================

#[tokio::test]
async fn test_validate_endpoint() {
    let app = common::test_app().await;

    // --- Valid workflow ---
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/validate",
            Some(json!({
                "name": "Valid Workflow",
                "condition": true,
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"ok"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["valid"], true);
    assert!(body["data"]["errors"].as_array().unwrap().is_empty());

    // --- Empty name ---
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/validate",
            Some(json!({
                "name": "",
                "condition": true,
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"ok"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["valid"], false);
    let errors = body["data"]["errors"].as_array().unwrap();
    assert!(
        errors.iter().any(|e| e["field"] == "workflow.name"),
        "should have error on the name field: {errors:?}"
    );

    // --- Empty tasks ---
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/validate",
            Some(json!({
                "name": "No Tasks",
                "condition": true,
                "tasks": []
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["valid"], false);
    let errors = body["data"]["errors"].as_array().unwrap();
    assert!(
        errors.iter().any(|e| e["field"] == "tasks"),
        "should have error on 'tasks' field: {errors:?}"
    );

    // --- Duplicate task IDs ---
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/validate",
            Some(json!({
                "name": "Duplicate IDs",
                "condition": true,
                "tasks": [
                    {"id":"dup","name":"First","function":{"name":"log","input":{"message":"a"}}},
                    {"id":"dup","name":"Second","function":{"name":"log","input":{"message":"b"}}}
                ]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["valid"], false);
    let errors = body["data"]["errors"].as_array().unwrap();
    let has_dup_error = errors.iter().any(|e| {
        e["message"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .contains("duplicate")
    });
    assert!(
        has_dup_error,
        "should report duplicate task IDs: {errors:?}"
    );

    // --- Missing task name ---
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/validate",
            Some(json!({
                "name": "Missing Task Name",
                "condition": true,
                "tasks": [
                    {"id":"t1","name":"","function":{"name":"log","input":{"message":"a"}}}
                ]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["valid"], false);
    let errors = body["data"]["errors"].as_array().unwrap();
    let has_name_error = errors
        .iter()
        .any(|e| e["field"].as_str().unwrap_or("").contains("name"));
    assert!(
        has_name_error,
        "should report missing task name: {errors:?}"
    );
}

// ============================================================
// 7. Rollout percentage management
// ============================================================

#[tokio::test]
async fn test_rollout_traffic_split() {
    let app = common::test_app().await;

    // Create and activate a workflow
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "rollout-test",
                "name": "Rollout Test",
                "condition": true,
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"v1"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    // Default rollout percentage is 100
    assert_eq!(body["data"]["rollout_percentage"], 100);

    // Activate the workflow
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/workflows/rollout-test/status",
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // With only one active version, rollout must be 100 — setting to 50 should fail
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/workflows/rollout-test/rollout",
            Some(json!({"rollout_percentage": 50})),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "partial rollout with one active version should fail"
    );

    // Setting rollout to 100 on a single active version should succeed
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/workflows/rollout-test/rollout",
            Some(json!({"rollout_percentage": 100})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["rollout_percentage"], 100);

    // Verify rollout is reflected when fetching the workflow
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/workflows/rollout-test",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["rollout_percentage"], 100);

    // Rollout percentage must be between 1 and 100
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/workflows/rollout-test/rollout",
            Some(json!({"rollout_percentage": 0})),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "rollout_percentage of 0 should be rejected"
    );

    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/workflows/rollout-test/rollout",
            Some(json!({"rollout_percentage": 101})),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "rollout_percentage of 101 should be rejected"
    );
}

// ============================================================
// Rollout actually routes traffic (not just stores percentages)
// ============================================================

/// A 50/50 split must reach both versions.
///
/// This is the end-to-end assertion the rollout mechanism never had. The
/// traffic split was wired by wrapping each version's condition with
/// `{">=": [{"var": "_rollout_bucket"}, min]}` — a **context-root** lookup —
/// while the ingress injected the key at `data._rollout_bucket`. The lookup
/// therefore resolved to null, coerced to `0`, and only the version whose
/// range started at 0 ever matched: 100% of traffic to one version, whatever
/// the configured percentages said. The only test covering it asserted on the
/// *shape* of the generated condition and never evaluated one.
///
/// dataflow-rs 3.1 routes on `Workflow::rollout` against
/// `Message::routing_bucket`, so there is no expression and no namespace to
/// misspell. Buckets are random per request when the caller has no sticky
/// identity, so this drives enough requests that a one-sided split is not a
/// plausible outcome: at 50/50 over 60 requests, missing a version has
/// probability 2⁻⁵⁹.
#[tokio::test]
async fn rollout_percentages_split_traffic_across_versions() {
    let app = common::test_app().await;

    let marker_workflow = |marker: &str| {
        json!({
            "workflow_id": "split",
            "name": "Split",
            "condition": true,
            "tasks": [{
                "id": "t1",
                "name": "Mark the version",
                "function": {
                    "name": "map",
                    "input": {"mappings": [{"path": "data.version", "logic": marker}]}
                }
            }]
        })
    };

    // v1 at 100%, on an active channel.
    common::create_and_activate_channel(&app, "split", marker_workflow("v1")).await;

    // v2 as a new draft, edited to mark itself differently.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/split/versions",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/workflows/split",
            Some(marker_workflow("v2")),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Activate v2 at 50%, which puts v1 at the other 50%.
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/workflows/split/status",
            Some(json!({"status": "active", "rollout_percentage": 50})),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "body = {}",
        body_json(resp).await
    );

    let mut seen = std::collections::HashSet::new();
    for _ in 0..60 {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/data/split",
                Some(json!({"data": {}})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let version = body["data"]["version"]
            .as_str()
            .unwrap_or_else(|| panic!("every request must match a version, got: {body}"))
            .to_string();
        seen.insert(version);
    }

    let mut seen: Vec<String> = seen.into_iter().collect();
    seen.sort();
    assert_eq!(
        seen,
        vec!["v1".to_string(), "v2".to_string()],
        "a 50/50 rollout must reach both versions"
    );
}

/// A dry run that dies partway still reports the steps that ran.
///
/// `process_message_with_trace` builds the trace as a function-local and moves
/// it into the `Ok` arm, so a hard failure discarded every step already
/// recorded — on the one endpoint whose entire purpose is showing them. The
/// steps were in memory the whole time; only the two public entry points
/// inverted a by-reference trace into a by-value return, which dataflow-rs 3.1
/// fixes with `process_message_tracing`.
///
/// Note the failing task's own step is still not recorded: the engine
/// propagates before appending it, so the trace ends at the last known-good
/// step and `error` names what stopped it.
#[tokio::test]
async fn a_failing_dry_run_returns_the_steps_that_ran() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "partial-dry-run",
                "name": "Partial Dry Run",
                "condition": true,
                "tasks": [
                    {
                        "id": "ok",
                        "name": "This one succeeds",
                        "function": {
                            "name": "map",
                            "input": {"mappings": [{"path": "data.reached", "logic": true}]}
                        }
                    },
                    {
                        "id": "boom",
                        "name": "This one cannot resolve its connector",
                        "function": {
                            "name": "http_call",
                            "input": {"connector": "no-such-connector", "path": "/x"}
                        }
                    }
                ]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/partial-dry-run/test",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a failed dry run must report its trace, not a bare 5xx"
    );

    let body = body_json(resp).await;
    assert!(
        body["data"]["error"].is_string(),
        "the failure must be named, got: {body}"
    );

    let steps = body["data"]["trace"]["steps"].as_array().unwrap();
    assert!(
        steps.iter().any(|s| s["task_id"] == "ok"),
        "the step that ran before the failure must survive it, got: {body}"
    );
    assert_eq!(
        body["data"]["output"]["reached"], true,
        "and its writes must be visible in the output, got: {body}"
    );
}

// ============================================================
// `/validate` warns about reads nothing writes
// ============================================================

/// Post a workflow to `/validate` and return the parsed `data` object.
async fn validate(app: &axum::Router, workflow: serde_json::Value) -> serde_json::Value {
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/validate",
            Some(workflow),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await["data"].clone()
}

fn warning_fields(data: &serde_json::Value) -> Vec<String> {
    data["warnings"]
        .as_array()
        .expect("warnings array")
        .iter()
        .map(|w| w["field"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// A mistyped `var` path is the highest-frequency authoring bug and the least
/// visible: JSONLogic resolves it to null, the task succeeds, and the caller
/// gets a `200` with the field quietly absent.
#[tokio::test]
async fn validate_warns_about_a_path_no_task_writes() {
    let app = common::test_app().await;
    let data = validate(
        &app,
        json!({
            "name": "Typo Workflow",
            "condition": true,
            "tasks": [
                {"id": "parse", "name": "Parse", "function": {
                    "name": "parse_json", "input": {"source": "payload", "target": "order"}}},
                {"id": "shape", "name": "Shape", "function": {
                    "name": "map", "input": {"mappings": [
                        // `data.oder` — the typo the warning exists for.
                        {"path": "data.total", "logic": {"var": "data.oder.total"}}
                    ]}}}
            ]
        }),
    )
    .await;

    assert_eq!(
        data["valid"], true,
        "an unwritten read is advisory — create still accepts it (R20): {data}"
    );
    let warnings = data["warnings"].as_array().expect("warnings array");
    assert!(
        warnings.iter().any(|w| w["message"]
            .as_str()
            .unwrap_or_default()
            .contains("data.oder")),
        "expected a warning naming the mistyped path, got: {warnings:?}"
    );
}

/// The engine's own findings reach the endpoint an author calls.
///
/// `check_workflow` reports three codes `Engine::build` does not refuse, so a
/// workflow carrying one is created, activated and served — and says less than
/// its author wrote. `lint` and `preflight` reported them from the start; this
/// endpoint is what the API and the UI call, and it was the one surface that
/// stayed quiet. Warnings, not errors: `valid` still means "`POST /workflows`
/// would accept this", and it would.
#[tokio::test]
async fn validate_warns_about_the_engines_own_shape_findings() {
    let app = common::test_app().await;
    let data = validate(
        &app,
        json!({
            "name": "Decorative Check",
            "condition": true,
            "tasks": [
                // #308's shape: a failing rule records 400, the 4xx branch
                // carries on, and `respond` runs as if it had passed.
                {"id": "check", "name": "Check", "function": {
                    "name": "validation", "input": {"rules": [
                        {"logic": {"==": [{"var": "data.a"}, 1]}, "message": "a must be 1"}
                    ]}}},
                {"id": "respond", "name": "Respond", "function": {
                    "name": "map", "input": {"mappings": [
                        {"path": "data.ok", "logic": true}
                    ]}}}
            ]
        }),
    )
    .await;

    assert_eq!(
        data["valid"], true,
        "the engine builds this workflow, so create accepts it: {data}"
    );
    let warnings = data["warnings"].as_array().expect("warnings array");
    assert!(
        warnings.iter().any(|w| {
            w["field"].as_str().unwrap_or_default() == "task 'check'.halt_on"
                && w["message"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("halt_on")
        }),
        "expected the unguarded-validation advisory, got: {warnings:?}"
    );
}

/// The correctly spelled version of the same workflow is silent.
#[tokio::test]
async fn validate_is_quiet_when_every_read_is_written() {
    let app = common::test_app().await;
    let data = validate(
        &app,
        json!({
            "name": "Clean Workflow",
            "condition": true,
            "tasks": [
                {"id": "parse", "name": "Parse", "function": {
                    "name": "parse_json", "input": {"source": "payload", "target": "order"}}},
                {"id": "shape", "name": "Shape", "function": {
                    "name": "map", "input": {"mappings": [
                        // Written by `parse` (prefix), and by the previous
                        // mapping within this same task (self-reference).
                        {"path": "data.total", "logic": {"var": "data.order.total"}},
                        {"path": "data.doubled", "logic": {"*": [{"var": "data.total"}, 2]}}
                    ]}}}
            ]
        }),
    )
    .await;

    assert_eq!(data["valid"], true);
    assert!(
        warning_fields(&data).is_empty(),
        "a correct workflow must not warn: {data}"
    );
}

/// Reads that are not `data.*` are out of scope and must never warn.
///
/// `metadata` is populated by the ingress, `payload` by the request, and the
/// bare `var` inside a `map`/`reduce` body is rebound to the array element —
/// none of them are writes this walk can see, so warning about them would be
/// pure noise on correct workflows.
#[tokio::test]
async fn validate_does_not_warn_about_metadata_or_iteration_variables() {
    let app = common::test_app().await;
    let data = validate(
        &app,
        json!({
            "name": "Metadata Workflow",
            "condition": true,
            "tasks": [
                {"id": "parse", "name": "Parse", "function": {
                    "name": "parse_json", "input": {"source": "payload", "target": "order"}}},
                {"id": "shape", "name": "Shape", "function": {
                    "name": "map", "input": {"mappings": [
                        {"path": "data.method", "logic": {"var": "metadata.http_method"}},
                        {"path": "data.id", "logic": {"var": "metadata.params.id"}},
                        {"path": "data.sum", "logic": {"reduce": [
                            {"var": "data.order.items"},
                            {"+": [{"var": "accumulator"}, {"var": "current"}]},
                            0
                        ]}}
                    ]}}}
            ]
        }),
    )
    .await;

    assert_eq!(data["valid"], true);
    assert!(
        warning_fields(&data).is_empty(),
        "metadata and iteration variables are not unwritten reads: {data}"
    );
}

/// A connector task's `output` path counts as a write, so reading what an
/// `http_call` produced is not a warning.
#[tokio::test]
async fn validate_counts_a_connector_output_as_a_write() {
    let app = common::test_app().await;
    let data = validate(
        &app,
        json!({
            "name": "Connector Output Workflow",
            "condition": true,
            "tasks": [
                {"id": "parse", "name": "Parse", "function": {
                    "name": "parse_json", "input": {"source": "payload", "target": "order"}}},
                {"id": "call", "name": "Call", "function": {
                    "name": "http_call", "input": {
                        "connector": "crm", "path": "/x", "output": "data.customer"}}},
                {"id": "shape", "name": "Shape", "function": {
                    "name": "map", "input": {"mappings": [
                        {"path": "data.name", "logic": {"var": "data.customer.name"}}
                    ]}}}
            ]
        }),
    )
    .await;

    let unwritten: Vec<_> = data["warnings"]
        .as_array()
        .expect("warnings array")
        .iter()
        .filter(|w| {
            w["message"]
                .as_str()
                .unwrap_or_default()
                .contains("no earlier task writes")
        })
        .collect();
    assert!(
        unwritten.is_empty(),
        "an http_call `output` is a write: {unwritten:?}"
    );
}
