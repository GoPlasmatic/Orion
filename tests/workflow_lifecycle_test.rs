mod common;

use axum::http::StatusCode;
use common::{body_json, json_request};
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
    assert_eq!(body["matched"], true);

    // Output should contain the computed value
    assert_eq!(body["output"]["computed"], 42);

    // Trace should be present and non-empty
    assert!(body["trace"].is_object());
    let steps = body["trace"]["steps"].as_array().unwrap();
    assert!(!steps.is_empty(), "trace should contain at least one step");

    // No errors
    let errors = body["errors"].as_array().unwrap();
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
    let steps = body["trace"]["steps"].as_array().unwrap();
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
        body["output"].as_object().map_or(true, |o| o.is_empty()),
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

    assert_eq!(body["matched"], true);

    // No errors from the cache_write execution
    let errors = body["errors"].as_array().unwrap();
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
        import_body["imported"].as_i64().unwrap(),
        exported.len() as i64,
        "all exported workflows should import successfully"
    );
    assert_eq!(import_body["failed"], 0);

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
    assert_eq!(body["valid"], true);
    assert!(body["errors"].as_array().unwrap().is_empty());

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
    assert_eq!(body["valid"], false);
    let errors = body["errors"].as_array().unwrap();
    assert!(
        errors.iter().any(|e| e["field"] == "name"),
        "should have error on 'name' field: {errors:?}"
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
    assert_eq!(body["valid"], false);
    let errors = body["errors"].as_array().unwrap();
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
    assert_eq!(body["valid"], false);
    let errors = body["errors"].as_array().unwrap();
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
    assert_eq!(body["valid"], false);
    let errors = body["errors"].as_array().unwrap();
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
