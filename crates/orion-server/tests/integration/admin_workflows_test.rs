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

    // Try to update the active workflow -- should fail (no draft exists).
    // 404, not 400: D22 aligned the no-draft miss with every other
    // missing-row lookup.
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/workflows/no-update-active",
            Some(json!({"name": "Should Fail"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
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
async fn test_http_call_format_axes_validated_at_create() {
    let app = common::test_app().await;

    // Known values and a well-shaped static form body — scalars, an array of
    // scalars, a conditionally-null entry — are accepted.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "OAuth Token Call",
                "condition": true,
                "tasks": [{
                    "id": "t1",
                    "name": "Token",
                    "function": {
                        "name": "http_call",
                        "input": {
                            "connector": "oauth",
                            "method": "POST",
                            "path": "/token",
                            "body_format": "form",
                            "body": {
                                "grant_type": "client_credentials",
                                "scope": ["read", "write"],
                                "audience": null
                            },
                            "response_format": "text",
                            "output": "temp_data.token"
                        }
                    }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Unknown format values are refused when the workflow is created — an
    // authoring-time 400 naming both fields, never a request-time surprise.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "Bad Formats",
                "condition": true,
                "tasks": [{
                    "id": "t1",
                    "name": "Call",
                    "function": {
                        "name": "http_call",
                        "input": {
                            "connector": "oauth",
                            "body_format": "multipart",
                            "response_format": "base64"
                        }
                    }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    let details = body["error"]["details"].to_string();
    assert!(details.contains("body_format"), "{details}");
    assert!(details.contains("response_format"), "{details}");

    // A static body whose shape contradicts the format is caught the same way.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "Nested Form Body",
                "condition": true,
                "tasks": [{
                    "id": "t1",
                    "name": "Call",
                    "function": {
                        "name": "http_call",
                        "input": {
                            "connector": "oauth",
                            "body_format": "form",
                            "body": {"metadata": {"nested": true}}
                        }
                    }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    let details = body["error"]["details"].to_string();
    assert!(details.contains("metadata"), "{details}");
}

#[tokio::test]
async fn test_crypto_op_envelope_validated_at_create() {
    let app = common::test_app().await;

    // The Zoom CRC shape from #259 — a valid hmac task is accepted as a draft.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "Zoom CRC",
                "condition": true,
                "tasks": [{
                    "id": "t1",
                    "name": "Sign",
                    "function": {
                        "name": "crypto",
                        "input": {
                            "op": "hmac",
                            "algorithm": "sha256",
                            "key": "env://ZOOM_WEBHOOK_SECRET",
                            "data": {"var": "data.payload.plainToken"},
                            "encoding": "hex",
                            "output": "temp_data.encrypted_token"
                        }
                    }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // The capability table is enforced at create: fast-hash-for-passwords,
    // a MAC without a key, an out-of-bounds cost, and a field that does not
    // apply to the op are each a named 400, never a request-time surprise.
    for (input, expected_field) in [
        (
            json!({"op": "password_hash", "algorithm": "sha256", "password": "x"}),
            "algorithm",
        ),
        (json!({"op": "hmac", "data": "x"}), "key"),
        (
            json!({"op": "password_hash", "password": "x", "params": {"cost": 42}}),
            "params",
        ),
        (json!({"op": "hash", "data": "x", "key": "why"}), "key"),
    ] {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/admin/workflows",
                Some(json!({
                    "name": "Bad Crypto",
                    "condition": true,
                    "tasks": [{
                        "id": "t1",
                        "name": "Bad",
                        "function": {"name": "crypto", "input": input}
                    }]
                })),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "expected 400 for {input}"
        );
        let body = body_json(resp).await;
        let details = body["error"]["details"].to_string();
        assert!(
            details.contains(expected_field),
            "{input} should have reported on `{expected_field}`, got {details}"
        );
    }
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

/// `POST /workflows/{id}/test` must judge a workflow on state an ingress can
/// actually produce.
///
/// It stamped `vars` and nothing else, so the endpoint a human reaches for
/// first accepted caller-supplied `oauth`, `cookies` and `_orion_errors`, and
/// unlowercased, unmasked `headers` — and a workflow could pass its test on
/// state no request can create. The two *offline* surfaces (`dry-run`, the
/// CLI's `test`) already went through `prepare_offline_metadata`, whose stated
/// purpose is that "an offline pass must mean the same thing as a production
/// pass"; this is the third surface joining them.
#[tokio::test]
async fn workflow_test_normalizes_metadata_the_way_an_ingress_does() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "test-meta-norm",
                "name": "Echo Metadata",
                "condition": true,
                "tasks": [{
                    "id": "t1",
                    "name": "Echo",
                    "function": { "name": "map", "input": { "mappings": [
                        { "path": "data.hdr", "logic": { "var": "metadata.headers.deviceid" } },
                        { "path": "data.auth_hdr", "logic": { "var": "metadata.headers.authorization" } },
                        { "path": "data.errs", "logic": { "var": "metadata._orion_errors" } }
                    ] } }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/test-meta-norm/test",
            Some(json!({
                "data": {},
                "metadata": {
                    // axum yields lowercase header names, so a case writing
                    // this would match here and miss in production.
                    "headers": { "DeviceId": "abc", "Authorization": "Bearer sk-live-xyz" },
                    // Engine-owned; the ingress clears it unconditionally, so a
                    // caller cannot pre-seed failures a workflow branches on.
                    "_orion_errors": [{ "message": "pre-seeded" }]
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let out = &body["data"]["output"];

    assert_eq!(out["hdr"], "abc", "header keys are lowercased: {body}");
    assert_ne!(
        out["auth_hdr"], "Bearer sk-live-xyz",
        "a credential header must be masked, as it is at ingress: {body}"
    );
    assert!(
        out["errs"].is_null(),
        "_orion_errors is engine-owned and cleared: {body}"
    );
}

/// The shape checks come with the normalization, so this surface refuses the
/// same impossible metadata the offline ones do rather than running on it.
#[tokio::test]
async fn workflow_test_refuses_metadata_no_ingress_could_produce() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "test-meta-refuse",
                "name": "Refuse",
                "condition": true,
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "x"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    for (label, metadata) in [
        (
            "headers not an object of strings",
            json!({ "headers": { "a": 1 } }),
        ),
        ("vars not an object", json!({ "vars": "nope" })),
        ("channel not a string", json!({ "channel": 7 })),
        // The ingress builds `auth` as `{"claims": …}` and nothing else.
        (
            "auth carrying more than claims",
            json!({ "auth": { "sub": "u1" } }),
        ),
        // #307: stamped by the sign-in guard once the grant is verified.
        ("oauth not an object", json!({ "oauth": "token" })),
    ] {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/admin/workflows/test-meta-refuse/test",
                Some(json!({ "data": {}, "metadata": metadata })),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "{label} must be refused"
        );
    }
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

// ============================================================
// Task identity is mandatory, and refused at authoring time
// ============================================================

/// Build a one-task workflow, letting the caller break the task's identity.
fn workflow_with_tasks(id: &str, tasks: serde_json::Value) -> serde_json::Value {
    json!({"workflow_id": id, "name": "Identity", "condition": true, "tasks": tasks})
}

async fn create_status_and_body(
    app: &axum::Router,
    wf: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(wf),
        ))
        .await
        .unwrap();
    let status = resp.status();
    (status, common::body_json(resp).await)
}

/// A task with no `id` is refused at create, not discovered at first request.
///
/// `dataflow_rs::Task::id` is a required `String`, so such a workflow used to be
/// accepted (201), activate cleanly (200), and then fail to convert at engine
/// load — quarantining its channel and answering 503 to every request with
/// "missing field `id`". Same class as the `enrich` false-accept above.
#[tokio::test]
async fn a_task_without_an_id_is_refused_at_create() {
    let app = common::test_app().await;
    let (status, body) = create_status_and_body(
        &app,
        workflow_with_tasks(
            "no-task-id",
            json!([{"name": "Log", "function": {"name": "log", "input": {"message": "hi"}}}]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body = {body}");
    assert!(
        body.to_string().contains("tasks[0].id"),
        "the error must point at the offending task, got: {body}"
    );
}

/// A *missing* `name` is refused: it is a required `String` upstream, so the
/// document would not deserialize.
#[tokio::test]
async fn a_task_without_a_name_is_refused_at_create() {
    let app = common::test_app().await;
    let (status, body) = create_status_and_body(
        &app,
        workflow_with_tasks(
            "no-task-name",
            json!([{"id": "t1", "function": {"name": "log", "input": {"message": "hi"}}}]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body = {body}");
    assert!(
        body.to_string().contains("tasks[0].name"),
        "the error must point at the offending task, got: {body}"
    );
}

/// …but an *empty* `name` is accepted, because the engine accepts it.
///
/// This is the boundary between the two rules. Orion tracks dataflow-rs's
/// parsing rules rather than tightening them, so that "Orion accepts it" and
/// "the engine can load it" remain the same statement. `""` deserializes into
/// the required `String` and the workflow loads and runs; an empty name is
/// unhelpful in a log, but that is an authoring preference, not a defect, and
/// refusing it would be Orion inventing a rule the engine does not have.
/// `id` is the one deliberate exception — see below.
#[tokio::test]
async fn a_task_with_an_empty_name_is_accepted_and_serves_traffic() {
    let app = common::test_app().await;

    // Not just "create says 201": the point of the parity rule is that what
    // Orion accepts, the engine loads. Drive it all the way to a served
    // request, so this fails if the rule ever drifts in either direction.
    common::create_and_activate_channel(
        &app,
        "empty-name-ch",
        json!({
            "workflow_id": "empty-task-name",
            "name": "Empty Task Name",
            "condition": true,
            "tasks": [{
                "id": "t1",
                "name": "",
                "function": {"name": "map", "input": {"mappings": [{"path": "data.ok", "logic": true}]}}
            }]
        }),
    )
    .await;

    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/data/empty-name-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "an empty task name must load and run, not quarantine the channel"
    );
    let body = body_json(resp).await;
    assert_eq!(body["data"]["ok"], true, "body = {body}");
}

/// Duplicate task ids are the worst of the three: `Workflow::validate()` runs
/// inside `LogicCompiler::compile_workflows`, so a repeated id fails the whole
/// `Engine::new` — a 500 on the activate that triggers the reload, and a boot
/// abort on startup. It is not contained by the per-channel quarantine, which
/// is exactly why it cannot be left to load time.
#[tokio::test]
async fn duplicate_task_ids_are_refused_at_create() {
    let app = common::test_app().await;
    let (status, body) = create_status_and_body(
        &app,
        workflow_with_tasks(
            "dup-task-ids",
            json!([
                {"id": "t1", "name": "A", "function": {"name": "log", "input": {"message": "a"}}},
                {"id": "t1", "name": "B", "function": {"name": "log", "input": {"message": "b"}}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body = {body}");
    let rendered = body.to_string();
    assert!(
        rendered.contains("tasks[1].id") && rendered.contains("DUPLICATE_TASK_ID"),
        "the error must name the second occurrence, got: {body}"
    );
}

/// A blank `id`, unlike a blank `name`, is refused — the one place Orion is
/// deliberately stricter than the parse. `""` deserializes fine, but two of
/// them collide on `Workflow::validate()`'s uniqueness check, and even one
/// writes an empty `task_id` into every trace step, audit entry and metric
/// label, which makes the identifier useless for the thing it exists to do.
#[tokio::test]
async fn a_blank_task_id_is_refused_at_create() {
    let app = common::test_app().await;
    let (status, body) = create_status_and_body(
        &app,
        workflow_with_tasks(
            "blank-task-id",
            json!([{"id": "  ", "name": "Log", "function": {"name": "log", "input": {"message": "hi"}}}]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body = {body}");
    assert!(body.to_string().contains("tasks[0].id"), "body = {body}");
}

/// R20: `/validate` must agree with create rather than green-lighting a payload
/// create refuses.
#[tokio::test]
async fn the_validate_endpoint_agrees_about_task_identity() {
    let app = common::test_app().await;
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/workflows/validate",
            Some(workflow_with_tasks(
                "v",
                json!([{"name": "Log", "function": {"name": "log", "input": {"message": "hi"}}}]),
            )),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(
        body["data"]["valid"], false,
        "/validate must not accept what create rejects, got: {body}"
    );
}

/// The whole point: a well-formed workflow is untouched by any of this.
#[tokio::test]
async fn a_workflow_with_proper_task_identity_is_accepted() {
    let app = common::test_app().await;
    let (status, body) = create_status_and_body(
        &app,
        workflow_with_tasks(
            "good-ids",
            json!([
                {"id": "fetch", "name": "Fetch", "function": {"name": "log", "input": {"message": "a"}}},
                {"id": "emit",  "name": "Emit",  "function": {"name": "log", "input": {"message": "b"}}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body = {body}");
}

// ============================================================
// Workflow loop (dataflow-rs 3.3)
// ============================================================

fn looping_workflow(name: &str, loop_config: serde_json::Value) -> serde_json::Value {
    json!({
        "name": name,
        "condition": { "<": [{ "var": "temp_data.i" }, { "var": "data.count" }] },
        "loop": loop_config,
        "tasks": [{
            "id": "note",
            "name": "Note the sweep",
            "function": { "name": "log", "input": { "message": "sweep" } }
        }]
    })
}

/// A `loop` survives create → read → new version → activate. The version copy
/// matters most: `create_new_version` builds its row field by field, so a
/// column it forgets is silently dropped rather than failing anything.
#[tokio::test]
async fn workflow_loop_round_trips_and_survives_a_new_version() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(looping_workflow(
                "Looping",
                json!({ "counter": "i", "max": 500 }),
            )),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    let id = body["data"]["workflow_id"].as_str().unwrap().to_string();
    assert_eq!(body["data"]["loop"]["counter"], "i");
    assert_eq!(body["data"]["loop"]["max"], 500);
    let hash_with_loop = body["data"]["content_hash"].as_str().unwrap().to_string();

    // Activate, then branch a new draft version off it.
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{id}/status"),
            Some(json!({ "status": "active" })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/admin/workflows/{id}/versions"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["version"], 2);
    assert_eq!(
        body["data"]["loop"]["max"], 500,
        "the new version must carry the loop the old one had"
    );
    assert_eq!(
        body["data"]["content_hash"].as_str().unwrap(),
        hash_with_loop,
        "copying a version must not change its content"
    );
}

/// A workflow with no `loop` must not grow a `loop` key, and its hash must be
/// what it was before the column existed — package receipts are content-
/// immutable, so a shifted hash turns a re-apply into a 409.
#[tokio::test]
async fn a_workflow_without_a_loop_is_unchanged_by_the_feature() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(common::workflow_with_priority("No Loop", 0)),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert!(
        body["data"].get("loop").is_none(),
        "absent must stay absent, not become null: {}",
        body["data"]
    );
}

/// Each rule mirrors `LoopConfig::validate` in dataflow-rs, which otherwise
/// only fires at `Engine::build()` — where the failure is a refused reload
/// across every channel rather than a 400 on one call.
#[tokio::test]
async fn an_invalid_loop_is_refused_at_write_time() {
    let app = common::test_app().await;

    let cases = [
        (json!({ "counter": "i" }), "max"),
        (json!({ "max": 10, "increment": 0 }), "increment"),
        (json!({ "max": 5, "init": 5 }), "max"),
        (json!({ "max": 10, "counter": "" }), "counter"),
        (json!({ "max": 10, "counter": "a..b" }), "counter"),
        // Above engine.max_loop_iterations, which defaults to 10_000.
        (json!({ "max": 10_000_001 }), "max"),
    ];

    for (loop_config, expected_field) in cases {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/admin/workflows",
                Some(looping_workflow("Bad Loop", loop_config.clone())),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "expected 400 for loop {loop_config}"
        );
        let body = body_json(resp).await;
        let details = body["error"]["details"].to_string();
        assert!(
            details.contains(expected_field),
            "loop {loop_config} should have reported on `{expected_field}`, got {details}"
        );
    }
}

/// The one that proves the feature rather than the plumbing: a looping
/// workflow, executed, must run its task list once per sweep and stop where
/// the break says. Storage round-trips and 400s would all still pass if
/// `workflow_to_dataflow` dropped the `loop` key on the floor.
///
/// The break is a `filter` with `on_reject: "halt"` rather than a workflow
/// condition, and that is not a stylistic choice: `data` starts empty, so a
/// workflow-level condition indexing `data.*` is false on sweep 0 and the loop
/// never starts. Inside the body, `parse_json` has already run.
#[tokio::test]
async fn a_loop_executes_one_sweep_per_iteration() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "loop-exec",
                "name": "Loop Exec",
                "condition": true,
                "loop": { "counter": "i", "max": 50 },
                "tasks": [
                    {
                        "id": "parse",
                        "name": "Parse the payload",
                        "function": {
                            "name": "parse_json",
                            "input": { "source": "payload", "target": "req" }
                        }
                    },
                    {
                        "id": "more",
                        "name": "Stop once every item is done",
                        "function": {
                            "name": "filter",
                            "input": {
                                "condition": {
                                    "<": [{ "var": "temp_data.i" }, { "var": "data.req.count" }]
                                },
                                "on_reject": "halt"
                            }
                        }
                    },
                    {
                        "id": "sweep",
                        "name": "One sweep",
                        "function": { "name": "log", "input": { "message": "sweep" } }
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
            "/api/v1/admin/workflows/loop-exec/test",
            // `max` is 50, so the filter is what stops this at 3.
            Some(json!({ "data": { "count": 3 } })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;

    let steps = body["data"]["trace"]["steps"]
        .as_array()
        .unwrap_or_else(|| panic!("trace.steps should be an array: {}", body["data"]));
    let sweeps = steps
        .iter()
        .filter(|s| s["task_id"] == "sweep" && s["result"] != "skipped")
        .count();
    assert_eq!(
        sweeps, 3,
        "expected one executed step per sweep, stopped by the filter, got {sweeps} in {:#}",
        body["data"]["trace"]
    );
}

// ============================================================
// Validate workflow - task groups (dataflow-rs 3.6)
// ============================================================

/// `/validate` must agree with create about what a task is. Before the walk
/// here was flattened, the group itself was read as a task and reported as
/// missing `name` and `function.name` — so a workflow that `POST /workflows`
/// accepts and the engine runs came back `valid: false` from the linting
/// endpoint, which is the one direction R20 says must never happen.
#[tokio::test]
async fn test_validate_accepts_a_workflow_whose_tasks_are_grouped() {
    let app = common::test_app().await;

    let workflow = json!({
        "name": "Grouped",
        "condition": true,
        "tasks": [
            {
                "id": "seed",
                "name": "Seed",
                "function": {"name": "map", "input": {"mappings": [
                    {"path": "data.amount", "logic": 500}
                ]}}
            },
            {
                "id": "guard",
                "condition": {">": [{"var": "data.amount"}, 100]},
                "tasks": [{
                    "id": "inner",
                    "name": "Inner",
                    "function": {"name": "log", "input": {"message": "big"}}
                }]
            }
        ]
    });

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/validate",
            Some(workflow.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["data"]["valid"], true,
        "grouped workflow rejected by /validate: {:?}",
        body["data"]["errors"]
    );

    // The agreement that matters: create accepts exactly what validate blessed.
    let created = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "wf-grouped",
                "name": "Grouped",
                "condition": true,
                "tasks": workflow["tasks"].clone()
            })),
        ))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
}

/// The other half: a broken task *inside* a group is still checked, and its
/// error names the path that addresses it rather than a top-level index.
#[tokio::test]
async fn test_validate_reports_a_broken_task_inside_a_group() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/validate",
            Some(json!({
                "name": "Grouped",
                "condition": true,
                "tasks": [{
                    "id": "guard",
                    "condition": true,
                    "tasks": [{"id": "inner", "name": "Inner"}]
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["valid"], false);

    let fields: Vec<&str> = body["data"]["errors"]
        .as_array()
        .expect("errors array")
        .iter()
        .filter_map(|e| e["field"].as_str())
        .collect();
    assert!(
        fields.contains(&"tasks[0].tasks[0].function.name"),
        "expected the nested task's own path, got {fields:?}"
    );
}
