mod common;

use axum::http::StatusCode;
use common::{body_json, json_request};
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
            Some(json!({
                "name": "Test Workflow",
                "priority": 10,
                "condition": true,
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
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
async fn test_workflow_version_history() {
    let app = common::test_app().await;

    // Create and activate a workflow
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "ver-test",
                "name": "Version Test",
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"v1"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Activate v1
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/workflows/ver-test/status",
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Create new draft version
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/ver-test/versions",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["version"], 2);
    assert_eq!(body["data"]["status"], "draft");

    // Update the draft
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/workflows/ver-test",
            Some(json!({"name": "Version Test v2"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["name"], "Version Test v2");

    // Activate v2
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/workflows/ver-test/status",
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // List versions
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/workflows/ver-test/versions",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["total"].as_i64().unwrap() >= 2);
    // Versions should have workflow_id field
    let versions = body["data"].as_array().unwrap();
    assert!(versions.iter().all(|v| v.get("workflow_id").is_some()));
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
async fn test_workflow_import_export() {
    let app = common::test_app().await;

    // Import
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/import",
            Some(json!([
                {
                    "workflow_id": "import-1",
                    "name": "Import Workflow 1",
                    "condition": true,
                    "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"imported"}}}]
                },
                {
                    "workflow_id": "import-2",
                    "name": "Import Workflow 2",
                    "condition": true,
                    "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"imported"}}}]
                }
            ])),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["imported"], 2);
    assert_eq!(body["failed"], 0);

    // Export
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/workflows/export", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let data = body["data"].as_array().unwrap();
    assert!(data.len() >= 2);
}

#[tokio::test]
async fn test_workflow_test_endpoint() {
    let app = common::test_app().await;

    // Create a workflow
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "test-dry-run",
                "name": "Dry Run Workflow",
                "condition": true,
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"dry run"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Test the workflow
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/test-dry-run/test",
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body.get("matched").is_some());
    assert!(body.get("trace").is_some());
    assert!(body.get("output").is_some());
}

#[tokio::test]
async fn test_workflow_validate_endpoint() {
    let app = common::test_app().await;

    // Valid workflow
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/validate",
            Some(json!({
                "name": "Valid Workflow",
                "condition": true,
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["valid"], true);

    // Invalid workflow (empty name)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/validate",
            Some(json!({
                "name": "",
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["valid"], false);
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
// Channel CRUD Lifecycle
// ============================================================

#[tokio::test]
async fn test_channels_crud_lifecycle() {
    let app = common::test_app().await;

    // First create a workflow to link to
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "ch-test-wf",
                "name": "Channel Test Workflow",
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
            "/api/v1/admin/workflows/ch-test-wf/status",
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Create a channel (starts as draft)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "test-channel",
                "channel_type": "sync",
                "protocol": "http",
                "methods": ["POST"],
                "route_pattern": "/test",
                "workflow_id": "ch-test-wf",
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    let channel_id = body["data"]["channel_id"].as_str().unwrap().to_string();
    assert_eq!(body["data"]["name"], "test-channel");
    assert_eq!(body["data"]["status"], "draft");

    // Get the channel
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/channels/{}", channel_id),
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["name"], "test-channel");

    // List channels
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/channels", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["total"].as_i64().unwrap() >= 1);
    assert!(!body["data"].as_array().unwrap().is_empty());

    // Activate the channel
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{}/status", channel_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["status"], "active");

    // Archive the channel
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{}/status", channel_id),
            Some(json!({"status": "archived"})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["status"], "archived");

    // Delete the channel
    let resp = app
        .clone()
        .oneshot(json_request(
            "DELETE",
            &format!("/api/v1/admin/channels/{}", channel_id),
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
            &format!("/api/v1/admin/channels/{}", channel_id),
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
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
        json!({
            "name": "Status Check Workflow",
            "condition": true,
            "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
        }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/engine/status", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["workflows_count"].as_i64().unwrap() >= 1);
    assert!(body["active_workflows"].as_i64().unwrap() >= 1);
    assert!(body.get("channels").is_some());
    assert!(body.get("version").is_some());
    assert!(body.get("uptime_seconds").is_some());
    let channels = body["channels"].as_array().unwrap();
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
    assert_eq!(body["valid"], true);
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
    let warnings = body["warnings"].as_array().unwrap();
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
    assert_eq!(body["valid"], false);
    let errors = body["errors"].as_array().unwrap();
    let has_name_error = errors
        .iter()
        .any(|e| e["field"].as_str().unwrap_or("") == "name");
    assert!(has_name_error);
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
    assert!(body.get("matched").is_some());
    assert!(body.get("trace").is_some());
    assert!(body.get("output").is_some());
    assert!(body.get("errors").is_some());
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
// Import workflows - all success case
// ============================================================

#[tokio::test]
async fn test_import_all_success() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/import",
            Some(json!([
                {
                    "workflow_id": "import-1",
                    "name": "Import Workflow 1",
                    "condition": true,
                    "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "imported"}}}]
                },
                {
                    "workflow_id": "import-2",
                    "name": "Import Workflow 2",
                    "condition": true,
                    "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "imported"}}}]
                }
            ])),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["imported"], 2);
    assert_eq!(body["failed"], 0);
    assert!(body["errors"].as_array().unwrap().is_empty());
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
