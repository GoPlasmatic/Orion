use crate::common;

use crate::common::{body_json, json_request};
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

// ============================================================
// Workflow CRUD Lifecycle
// ============================================================

#[tokio::test]
async fn test_workflows_crud_lifecycle() {
    let app = common::test_app().await;

    // Create a workflow (starts as draft)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(common::workflow_with_priority("Test Workflow", 10)),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    let workflow_id = body["data"]["workflow_id"].as_str().unwrap().to_string();
    assert_eq!(body["data"]["name"], "Test Workflow");
    assert_eq!(body["data"]["version"], 1);
    assert_eq!(body["data"]["status"], "draft");
    assert_eq!(body["data"]["rollout_percentage"], 100);

    // Get the workflow
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/workflows/{}", workflow_id),
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["name"], "Test Workflow");

    // List workflows
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/workflows", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["total"].as_i64().unwrap() >= 1);
    assert!(!body["data"].as_array().unwrap().is_empty());

    // Update the draft workflow
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/workflows/{}", workflow_id),
            Some(json!({"name": "Updated Workflow", "priority": 20})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["name"], "Updated Workflow");
    assert_eq!(body["data"]["priority"], 20);

    // Activate the workflow
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{}/status", workflow_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["status"], "active");

    // Archive the workflow
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{}/status", workflow_id),
            Some(json!({"status": "archived"})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["status"], "archived");

    // Delete the workflow
    let resp = app
        .clone()
        .oneshot(json_request(
            "DELETE",
            &format!("/api/v1/admin/workflows/{}", workflow_id),
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Verify 404
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/workflows/{}", workflow_id),
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_workflow_status_transitions() {
    let app = common::test_app().await;

    // Create a workflow (draft)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "status-test",
                "name": "Status Test",
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["status"], "draft");

    // Activate
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/workflows/status-test/status",
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["status"], "active");

    // Archive
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/workflows/status-test/status",
            Some(json!({"status": "archived"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["status"], "archived");
}

#[tokio::test]
async fn test_workflow_list_with_filters() {
    let app = common::test_app().await;

    // Create two workflows with different tags
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "Filter Workflow A",
                "tags": ["production"],
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    let wf_a_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    // Activate workflow A
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{}/status", wf_a_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "Filter Workflow B",
                "tags": ["staging"],
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Filter by status
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/workflows?status=active",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let data = body["data"].as_array().unwrap();
    for workflow in data {
        assert_eq!(workflow["status"], "active");
    }

    // Filter by tag
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/workflows?tag=production",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let data = body["data"].as_array().unwrap();
    assert!(!data.is_empty());
}

#[tokio::test]
async fn test_workflow_pagination() {
    let app = common::test_app().await;

    // Create 3 workflows
    for i in 0..3 {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/admin/workflows",
                Some(json!({
                    "name": format!("Pagination Workflow {}", i),
                    "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
                })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    // Get page 1
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/workflows?limit=2&offset=0",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"].as_array().unwrap().len(), 2);
    assert!(body["total"].as_i64().unwrap() >= 3);
    assert_eq!(body["limit"], 2);
    assert_eq!(body["offset"], 0);
}

#[tokio::test]
async fn test_workflow_with_custom_id() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "my-custom-workflow",
                "name": "Custom ID Workflow",
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["workflow_id"], "my-custom-workflow");
}

#[tokio::test]
async fn test_cannot_update_active_workflow() {
    let app = common::test_app().await;

    // Create a workflow
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "no-update-active",
                "name": "No Update Active",
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Activate the workflow
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/workflows/no-update-active/status",
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Try to update the active workflow -- should fail (no draft exists)
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/workflows/no-update-active",
            Some(json!({"name": "Should Fail"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_create_new_version_and_edit() {
    let app = common::test_app().await;

    // Create a workflow
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "new-ver-test",
                "name": "New Version Test",
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"v1"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Activate the workflow
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/workflows/new-ver-test/status",
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Create new version (draft v2)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/new-ver-test/versions",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["version"], 2);
    assert_eq!(body["data"]["status"], "draft");

    // Edit the draft
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/workflows/new-ver-test",
            Some(json!({"name": "New Version Test v2"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Cannot create another draft when one exists
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/new-ver-test/versions",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

// ============================================================
// Engine Status
// ============================================================

#[tokio::test]
async fn test_engine_status_with_loaded_workflows() {
    let app = common::test_app().await;

    common::create_and_activate_channel(
        &app,
        "status-ch",
        common::simple_log_workflow("Status Check Workflow"),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/engine/status", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["data"]["workflows_count"].as_i64().unwrap() >= 1);
    assert!(body["data"]["active_workflows"].as_i64().unwrap() >= 1);
    assert!(body["data"].get("channels").is_some());
    assert!(body["data"].get("version").is_some());
    assert!(body["data"].get("uptime_seconds").is_some());
    let channels = body["data"]["channels"].as_array().unwrap();
    assert!(channels.iter().any(|c| c == "status-ch"));
}

// ============================================================
// Version History with Pagination
// ============================================================

#[tokio::test]
async fn test_version_list_with_pagination_params() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "ver-page-test",
                "name": "Version Pagination",
                "condition": true,
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "v1"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/workflows/ver-page-test/status",
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/ver-page-test/versions",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/workflows/ver-page-test/status",
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/ver-page-test/versions",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/workflows/ver-page-test/versions?limit=1&offset=0",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["limit"], 1);
    assert_eq!(body["offset"], 0);
    assert!(body["total"].as_i64().unwrap() >= 2);
    assert_eq!(body["data"].as_array().unwrap().len(), 1);

    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/workflows/ver-page-test/versions?limit=10&offset=1",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["offset"], 1);
    assert!(!body["data"].as_array().unwrap().is_empty());
}

// ============================================================
// Export with Filters
// ============================================================

#[tokio::test]
async fn test_export_workflows_with_status_filter() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "export-a",
                "name": "Export Workflow A",
                "condition": true,
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "a"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/workflows/export-a/status",
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "export-b",
                "name": "Export Workflow B",
                "condition": true,
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "b"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/workflows/export?status=active",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let data = body["data"].as_array().unwrap();
    assert!(!data.is_empty());
    for workflow in data {
        assert_eq!(workflow["status"], "active");
    }
}

// ============================================================
// Validate workflow - task condition JSONLogic
// ============================================================

#[tokio::test]
async fn test_validate_workflow_with_task_condition() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/validate",
            Some(json!({
                "name": "Condition Test",
                "condition": true,
                "tasks": [{
                    "id": "t1",
                    "name": "Task with condition",
                    "condition": {">": [{"var": "data.amount"}, 100]},
                    "function": {"name": "log", "input": {"message": "test"}}
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["valid"], true);
}

#[tokio::test]
async fn test_validate_workflow_with_connector_warning() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/validate",
            Some(json!({
                "name": "Connector Warning Test",
                "condition": true,
                "tasks": [{
                    "id": "t1",
                    "name": "HTTP Call",
                    "function": {
                        "name": "http_call",
                        "input": {
                            "connector": "nonexistent-connector",
                            "method": "GET",
                            "path": "/api",
                            "timeout_ms": 5000
                        }
                    }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let warnings = body["data"]["warnings"].as_array().unwrap();
    let has_connector_warning = warnings
        .iter()
        .any(|w| w["message"].as_str().unwrap_or("").contains("not found"));
    assert!(has_connector_warning);
}

#[tokio::test]
async fn test_validate_workflow_with_empty_name() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/validate",
            Some(json!({
                "name": "",
                "condition": true,
                "tasks": [{
                    "id": "t1",
                    "name": "Log",
                    "function": {"name": "log", "input": {"message": "test"}}
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["valid"], false);
    let errors = body["data"]["errors"].as_array().unwrap();
    // R20: field paths come from the create-path validator now, so they read
    // the same in both places.
    let has_name_error = errors
        .iter()
        .any(|e| e["field"].as_str().unwrap_or("") == "workflow.name");
    assert!(has_name_error, "{errors:?}");
}

// ============================================================
// Test workflow with test endpoint (dry-run with data)
// ============================================================

#[tokio::test]
async fn test_workflow_test_with_metadata() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "test-dry-run-meta",
                "name": "Dry Run Workflow",
                "condition": true,
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "dry run"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/test-dry-run-meta/test",
            Some(json!({
                "data": {"key": "value"},
                "metadata": {"source": "test-suite"}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["data"].get("matched").is_some());
    assert!(body["data"].get("trace").is_some());
    assert!(body["data"].get("output").is_some());
    assert!(body["data"].get("errors").is_some());
}

#[tokio::test]
async fn test_workflow_test_with_non_object_data() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "test-non-obj",
                "name": "Non Object Data",
                "condition": true,
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/test-non-obj/test",
            Some(json!({
                "data": "just a string"
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ============================================================
// Create workflow with description and custom ID
// ============================================================

#[tokio::test]
async fn test_create_workflow_with_description_and_tags() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "desc-workflow",
                "name": "Described Workflow",
                "description": "This is a test workflow with a description",
                "priority": 5,
                "condition": {"==": [1, 1]},
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}],
                "tags": ["production", "important"],
                "continue_on_error": true
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["workflow_id"], "desc-workflow");
    assert_eq!(
        body["data"]["description"],
        "This is a test workflow with a description"
    );
    assert_eq!(body["data"]["priority"], 5);
    assert!(body["data"]["continue_on_error"].as_bool().unwrap());
}

// ============================================================
// Update workflow with description
// ============================================================

#[tokio::test]
async fn test_update_workflow_with_description() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "upd-desc-workflow",
                "name": "Update Desc Workflow",
                "condition": true,
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/workflows/upd-desc-workflow",
            Some(json!({
                "description": "Updated description"
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ============================================================
// R5: workflows that cannot run are rejected, not deferred
// ============================================================

#[tokio::test]
async fn test_create_rejects_unknown_function() {
    let app = common::test_app().await;

    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "unknown-fn-wf",
                "tasks": [{
                    "id": "t1",
                    "name": "typo",
                    "function": { "name": "http_calll", "input": { "connector": "c", "output": "data.x" } }
                }]
            })),
        ))
        .await
        .unwrap();
    // Previously 201 with only a lint warning — the workflow then failed at
    // its first request (R5).
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert!(
        body.to_string().contains("http_calll"),
        "error must name the unknown function, got {body}"
    );
}

#[tokio::test]
async fn test_activate_rejects_missing_connector() {
    let app = common::test_app().await;

    // Create is allowed (connectors and workflows may be authored in either
    // order) …
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "needs-conn-wf",
                "tasks": [{
                    "id": "t1",
                    "name": "read",
                    "function": { "name": "db_read", "input": {
                        "connector": "not-yet-created-db",
                        "query": "SELECT 1",
                        "output": "data.r"
                    }}
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let wf_id = body_json(resp).await["data"]["workflow_id"]
        .as_str()
        .unwrap()
        .to_string();

    // …activation is the gate.
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{wf_id}/status"),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert!(
        body.to_string().contains("not-yet-created-db"),
        "error must name the missing connector, got {body}"
    );

    // With the connector present, activation succeeds.
    common::create_connector(&app, common::db_connector("not-yet-created-db")).await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{wf_id}/status"),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// Create a workflow with the given tasks and try to activate it, returning
/// the activation response body and status.
async fn create_then_activate(
    app: &axum::Router,
    name: &str,
    tasks: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({ "name": name, "tasks": tasks })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "create must succeed");
    let wf_id = body_json(resp).await["data"]["workflow_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{wf_id}/status"),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

/// F52: the connector must be of a type the referencing function can use.
/// `ensure_workflow_connectors_exist` checked existence only, so pointing
/// `cache_read` at a `db` connector activated cleanly and then 500'd on the
/// first request — a runtime discovery for something fully determined at
/// authoring time.
#[tokio::test]
async fn test_activate_rejects_a_connector_of_the_wrong_type() {
    let app = common::test_app().await;
    common::create_connector(&app, common::db_connector("a-sql-db")).await;

    let (status, body) = create_then_activate(
        &app,
        "wrong-type-wf",
        json!([{
            "id": "t1",
            "name": "lookup",
            "function": { "name": "cache_read", "input": {
                "connector": "a-sql-db",
                "key": "k",
                "output": "data.v"
            }}
        }]),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let msg = body.to_string();
    assert!(msg.contains("a-sql-db"), "must name the connector: {msg}");
    assert!(msg.contains("cache_read"), "must name the function: {msg}");
    assert!(
        msg.contains("'db'") && msg.contains("'cache'"),
        "must say what it is and what was needed: {msg}"
    );

    // The same workflow against a cache connector activates.
    common::create_connector(&app, common::cache_connector_memory("a-cache")).await;
    let (status, body) = create_then_activate(
        &app,
        "right-type-wf",
        json!([{
            "id": "t1",
            "name": "lookup",
            "function": { "name": "cache_read", "input": {
                "connector": "a-cache",
                "key": "k",
                "output": "data.v"
            }}
        }]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// F52: `data_query` against a MongoDB connector needs a `database` — Mongo
/// connection strings carry no default one. The function schema cannot mark
/// the field required (the same task shape is valid against SQL and
/// Elasticsearch, which need no database), so the rule is conditional on the
/// connector, and the connector is known at activation.
#[tokio::test]
async fn test_activate_requires_a_database_for_mongo_connectors() {
    let app = common::test_app().await;
    common::create_connector(
        &app,
        json!({
            "name": "docs-mongo",
            "connector_type": "db",
            "config": { "connection_string": "mongodb://localhost:27017" }
        }),
    )
    .await;

    let query = json!({"source": "orders", "filter": {"eq": ["status", "new"]}});
    let (status, body) = create_then_activate(
        &app,
        "mongo-no-db-wf",
        json!([{
            "id": "t1",
            "name": "find",
            "function": { "name": "data_query", "input": {
                "connector": "docs-mongo",
                "query": query,
                "output": "data.rows"
            }}
        }]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let msg = body.to_string();
    assert!(msg.contains("database"), "must name the missing key: {msg}");
    assert!(msg.contains("docs-mongo"), "must name the connector: {msg}");

    // With `database` set it activates …
    let (status, body) = create_then_activate(
        &app,
        "mongo-with-db-wf",
        json!([{
            "id": "t1",
            "name": "find",
            "function": { "name": "data_query", "input": {
                "connector": "docs-mongo",
                "database": "shop",
                "query": query,
                "output": "data.rows"
            }}
        }]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // … and the same task against SQL needs no `database` at all.
    common::create_connector(&app, common::db_connector("orders-sql")).await;
    let (status, body) = create_then_activate(
        &app,
        "sql-no-db-wf",
        json!([{
            "id": "t1",
            "name": "find",
            "function": { "name": "data_query", "input": {
                "connector": "orders-sql",
                "query": query,
                "output": "data.rows"
            }}
        }]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

// ============================================================
// Single-draft invariant (proposal D9)
// ============================================================

#[tokio::test]
async fn a_second_draft_cannot_be_created_for_one_workflow() {
    // Postgres enforces this with a partial unique index covering INSERT and
    // UPDATE; SQLite and MySQL had BEFORE INSERT triggers only, so an UPDATE
    // that set status='draft' produced a second draft on two of three
    // backends — after which draft lookups silently pick an arbitrary row.
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "Draft One",
                "tasks": [{"id": "t1", "name": "Log",
                           "function": {"name": "log", "input": {"message": "x"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = common::body_json(resp).await;
    let workflow_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    // A draft already exists for this id, so asking for another new version
    // must be refused rather than yielding two drafts.
    let resp = app
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/admin/workflows/{workflow_id}/versions"),
            Some(json!({
                "name": "Draft Two",
                "tasks": [{"id": "t1", "name": "Log",
                           "function": {"name": "log", "input": {"message": "y"}}}]
            })),
        ))
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "a second draft must be refused, got {}",
        resp.status()
    );
}

/// `enrich` is refused at create, because Orion registers no handler for it.
///
/// It is a dataflow-rs built-in *name* but not a self-contained one: it
/// deserializes into a typed built-in variant — so `Engine::new` accepts it and
/// the custom-input check skips it, since it never becomes
/// `FunctionConfig::Custom` — and then dispatches to a handler registered under
/// the same name. Nothing registers one.
///
/// The gate used to be membership in a hand-copied name list, and `enrich` was
/// added to it (F54) on the reasoning that "the engine runs it". The engine
/// does not: such a workflow activated cleanly and then failed every single
/// request with `FunctionNotFound`, forever. dataflow-rs 3.1 publishes the
/// distinction as `BuiltinKind`, so the gate now asks whether this engine can
/// run the task rather than whether the name is spelled correctly.
#[tokio::test]
async fn a_workflow_using_the_handlerless_enrich_builtin_is_refused() {
    let app = common::test_app().await;
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "enrich-wf",
                "channel": "enrich-ch",
                "tasks": [{
                    "id": "t1",
                    "name": "Enrich",
                    "function": { "name": "enrich", "input": {} }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = common::body_json(resp).await;
    assert!(
        body.to_string().contains("enrich"),
        "the refusal must name the function, got: {body}"
    );
}

/// …while a self-contained built-in with no Orion handler is still accepted.
/// `filter` needs no registration, so refusing it would turn the gate into a
/// blanket "Orion handlers only" rule.
#[tokio::test]
async fn a_workflow_using_a_self_contained_builtin_is_accepted() {
    let app = common::test_app().await;
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "filter-wf",
                "channel": "filter-ch",
                "tasks": [{
                    "id": "t1",
                    "name": "Filter",
                    "function": { "name": "filter", "input": {"condition": true} }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "body = {}",
        common::body_json(resp).await
    );
}

// ============================================================
// R20: /validate must agree with POST /workflows
// ============================================================

/// R20: `validate_workflow_tasks_schema` carried the doc comment *"Public so
/// the `/validate` endpoint can reuse it"* and had **zero external callers**;
/// the endpoint re-implemented the same walk and the two disagreed by design —
/// an unknown `function.name` was a hard error at create and a *warning* here.
/// So `/validate` reported `valid: true` for a workflow create rejects, which is
/// worse than no linter: it lies in the one direction that matters.
#[tokio::test]
async fn validate_never_green_lights_what_create_rejects() {
    let app = common::test_app().await;

    // Every payload here is one `POST /workflows` refuses.
    let rejected = [
        // Unknown function — the exact case that used to be a warning.
        json!({
            "name": "Unknown Function",
            "tasks": [{"id": "t1", "name": "T",
                       "function": {"name": "not_a_function", "input": {}}}]
        }),
        // Missing a required input field for a known function.
        json!({
            "name": "Missing Input",
            "tasks": [{"id": "t1", "name": "T",
                       "function": {"name": "cache_read", "input": {"connector": "c"}}}]
        }),
        // Wrongly-typed input field.
        json!({
            "name": "Wrong Type",
            "tasks": [{"id": "t1", "name": "T",
                       "function": {"name": "db_read",
                                    "input": {"connector": "c", "query": 42}}}]
        }),
        // Empty name.
        json!({
            "name": "",
            "tasks": [{"id": "t1", "name": "T",
                       "function": {"name": "log", "input": {"message": "x"}}}]
        }),
    ];

    for payload in rejected {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/admin/workflows/validate",
                Some(payload.clone()),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "/validate always answers 200"
        );
        let validated = body_json(resp).await;

        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/admin/workflows",
                Some(payload.clone()),
            ))
            .await
            .unwrap();
        let created = resp.status();

        assert!(
            created.is_client_error(),
            "fixture must be one create rejects: {payload}"
        );
        assert_eq!(
            validated["data"]["valid"], false,
            "/validate said valid for a payload create rejected with {created}: \
             {payload} -> {validated}"
        );
    }
}

/// The other direction still holds: a payload create accepts validates clean.
#[tokio::test]
async fn validate_accepts_what_create_accepts() {
    let app = common::test_app().await;
    let payload = json!({
        "name": "Perfectly Fine",
        "condition": true,
        "tasks": [{"id": "t1", "name": "Log",
                   "function": {"name": "log", "input": {"message": "x"}}}]
    });

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/validate",
            Some(payload.clone()),
        ))
        .await
        .unwrap();
    let validated = body_json(resp).await;
    assert_eq!(validated["data"]["valid"], true, "{validated}");

    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(payload),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

/// R24: the version endpoints' page defaults were written out twice in the
/// route layer (`unwrap_or(50)` / `unwrap_or(0)`) and a third time in the
/// repository's `clamp_pagination`. They now come from one place, and both
/// entities behave identically — including for the out-of-range values the
/// route layer previously passed through untouched.
#[tokio::test]
async fn version_pagination_bounds_are_shared_by_both_entities() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "page-bounds",
                "name": "Page Bounds",
                "tasks": [{"id":"t1","name":"Log",
                           "function":{"name":"log","input":{"message":"v1"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "channel_id": "page-bounds-ch",
                "name": "page-bounds-ch",
                "channel_type": "sync",
                "protocol": "http",
                "methods": ["POST"],
                "route_pattern": "/page-bounds"
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    for (entity, id) in [("workflows", "page-bounds"), ("channels", "page-bounds-ch")] {
        // Default page size.
        let resp = app
            .clone()
            .oneshot(json_request(
                "GET",
                &format!("/api/v1/admin/{entity}/{id}/versions"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{entity}");
        assert_eq!(body_json(resp).await["limit"], 50, "{entity} default limit");

        // Over the cap and below the floor: clamped identically for both.
        let resp = app
            .clone()
            .oneshot(json_request(
                "GET",
                &format!("/api/v1/admin/{entity}/{id}/versions?limit=999999&offset=-5"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{entity}");
        let body = body_json(resp).await;
        assert_eq!(body["limit"], 1000, "{entity} limit must be capped");
        assert_eq!(body["offset"], 0, "{entity} offset must not go negative");
    }
}
