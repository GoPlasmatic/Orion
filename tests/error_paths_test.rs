mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{body_json, json_request};
use serde_json::json;
use tower::ServiceExt;

// ============================================================
// Content type & body validation
// ============================================================

#[tokio::test]
async fn test_any_content_type_accepted_if_body_is_valid_json() {
    let app = common::test_app().await;

    // Content-type validation is handled by per-channel validation_logic,
    // not hardcoded. Valid JSON with any content-type should parse fine.
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/data/orders")
        .header("content-type", "text/plain")
        .body(Body::from(r#"{"data": {"key": "value"}}"#))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert!(resp.status().is_success());
}

#[tokio::test]
async fn test_completely_invalid_json_body() {
    let app = common::test_app().await;

    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/data/orders")
        .header("content-type", "application/json")
        .body(Body::from("this is not json {{{"))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert!(resp.status().is_client_error());
}

#[tokio::test]
async fn test_empty_body_accepted() {
    let app = common::test_app().await;

    // Empty body is treated as {data: {}, metadata: {}} — valid for GET/DELETE
    // or any request without payload.
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/data/orders")
        .header("content-type", "application/json")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert!(resp.status().is_success());
}

#[tokio::test]
async fn test_missing_data_field_in_process_request() {
    let app = common::test_app().await;

    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/data/orders",
            Some(json!({"metadata": {"source": "test"}})),
        ))
        .await
        .unwrap();

    // `data` is required in ProcessRequest
    assert!(resp.status().is_client_error());
}

// ============================================================
// 404 on nonexistent resources
// ============================================================

#[tokio::test]
async fn test_nonexistent_trace_returns_404() {
    let app = common::test_app().await;

    let resp = app
        .oneshot(json_request(
            "GET",
            "/api/v1/data/traces/nonexistent-trace-id",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_nonexistent_workflow_returns_404() {
    let app = common::test_app().await;

    let resp = app
        .oneshot(json_request(
            "DELETE",
            "/api/v1/admin/workflows/nonexistent-workflow",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_nonexistent_connector_returns_404() {
    let app = common::test_app().await;

    let resp = app
        .oneshot(json_request(
            "DELETE",
            "/api/v1/admin/connectors/nonexistent-connector",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_update_nonexistent_workflow_returns_400() {
    let app = common::test_app().await;

    // update_draft returns BadRequest "No draft version found"
    let resp = app
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/workflows/nonexistent-workflow",
            Some(json!({"name": "Updated Name"})),
        ))
        .await
        .unwrap();

    assert!(resp.status().is_client_error());
}

// ============================================================
// Invalid status transition
// ============================================================

#[tokio::test]
async fn test_invalid_status_transition() {
    let app = common::test_app().await;

    // Create a workflow (draft)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "Status Test Workflow",
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    let workflow_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    // Try to set an invalid status
    let resp = app
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{}/status", workflow_id),
            Some(json!({"status": "invalid_status"})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Invalid status")
    );
}

// ============================================================
// Duplicate resource handling
// ============================================================

#[tokio::test]
async fn test_duplicate_workflow_id_rejected() {
    let app = common::test_app().await;

    let workflow = json!({
        "workflow_id": "duplicate-workflow",
        "name": "First Workflow",
        "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
    });

    // First creation succeeds
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(workflow.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Second creation with same ID must fail
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(workflow),
        ))
        .await
        .unwrap();
    assert!(!resp.status().is_success());
}

#[tokio::test]
async fn test_duplicate_connector_id_returns_conflict() {
    let app = common::test_app().await;

    let connector = json!({
        "id": "dup-conn",
        "name": "First Connector",
        "connector_type": "http",
        "config": {"url": "https://example.com/api", "method": "POST"}
    });

    // First creation succeeds
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(connector),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Second creation with same ID triggers UNIQUE constraint
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "dup-conn",
                "name": "Different Name",
                "connector_type": "http",
                "config": {"url": "https://example.com/api2", "method": "GET"}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}
