mod common;

use axum::http::StatusCode;
use common::{body_json, json_request};
use serde_json::json;
use tower::ServiceExt;

// ============================================================
// Per-channel timeout enforcement
// ============================================================

#[tokio::test]
async fn test_channel_timeout_returns_504() {
    // Start a slow mock server that delays 200ms
    let mock_app = axum::Router::new().route(
        "/slow",
        axum::routing::post(|| async {
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            axum::Json(json!({"result": "done"}))
        }),
    );
    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = mock_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(mock_listener, mock_app).await.unwrap();
    });

    let app = common::test_app().await;

    // Create connector pointing to slow mock server
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "slow-api",
                "name": "slow-api",
                "connector_type": "http",
                "config": {
                    "type": "http",
                    "url": format!("http://{}", mock_addr),
                    "retry": {"max_retries": 0, "retry_delay_ms": 10}
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Create workflow that calls the slow endpoint
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "Timeout Test Workflow",
                "tasks": [{
                    "id": "slow-call",
                    "name": "Slow HTTP Call",
                    "function": {
                        "name": "http_call",
                        "input": {
                            "connector": "slow-api",
                            "method": "POST",
                            "path": "/slow",
                            "body": {"test": true},
                            "response_path": "response",
                            "timeout_ms": 5000
                        }
                    }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    let wf_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    // Activate workflow
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{}/status", wf_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Create channel with 50ms timeout (mock server takes 200ms -> will timeout)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "timeout-ch",
                "channel_type": "sync",
                "protocol": "http",
                "methods": ["POST"],
                "route_pattern": "/timeout-ch",
                "workflow_id": wf_id,
                "config": {
                    "timeout_ms": 50
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    let ch_id = body["data"]["channel_id"].as_str().unwrap().to_string();

    // Activate channel
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{}/status", ch_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Send request — expect 504 Gateway Timeout
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/timeout-ch",
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "TIMEOUT");
}

#[tokio::test]
async fn test_channel_without_timeout_succeeds() {
    let app = common::test_app().await;

    common::create_and_activate_channel(
        &app,
        "no-timeout-ch",
        json!({
            "name": "No Timeout Workflow",
            "condition": true,
            "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "fast"}}}]
        }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/no-timeout-ch",
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ============================================================
// Per-channel input validation via validation_logic
// ============================================================

#[tokio::test]
async fn test_channel_validation_logic_rejects_invalid_input() {
    let app = common::test_app().await;

    // Create workflow
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "Validation Test WF",
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
            })),
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let wf_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    let _ = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{}/status", wf_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();

    // Create channel with validation_logic: order_id must exist AND quantity > 0
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "validated-ch",
                "channel_type": "sync",
                "protocol": "http",
                "methods": ["POST"],
                "route_pattern": "/validated-ch",
                "workflow_id": wf_id,
                "config": {
                    "validation_logic": {
                        "and": [
                            { "!!": { "var": "data.order_id" } },
                            { ">": [{ "var": "data.quantity" }, 0] }
                        ]
                    }
                }
            })),
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let ch_id = body["data"]["channel_id"].as_str().unwrap().to_string();

    let _ = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{}/status", ch_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();

    // Valid input — should pass
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/validated-ch",
            Some(json!({"data": {"order_id": "ORD-001", "quantity": 5}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Missing order_id — should be rejected 400
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/validated-ch",
            Some(json!({"data": {"quantity": 5}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("validation failed")
    );

    // quantity = 0 — should be rejected 400
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/validated-ch",
            Some(json!({"data": {"order_id": "ORD-002", "quantity": 0}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_channel_without_validation_logic_passes_any_input() {
    let app = common::test_app().await;

    common::create_and_activate_channel(
        &app,
        "no-validation-ch",
        json!({
            "name": "No Validation WF",
            "condition": true,
            "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
        }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/no-validation-ch",
            Some(json!({"data": "just a string"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ============================================================
// Per-channel CORS enforcement
// ============================================================

#[tokio::test]
async fn test_channel_cors_rejects_disallowed_origin() {
    let app = common::test_app().await;

    // Create workflow
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "CORS Test WF",
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
            })),
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let wf_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    let _ = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{}/status", wf_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();

    // Create channel with CORS restricted to https://allowed.example.com
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "cors-ch",
                "channel_type": "sync",
                "protocol": "http",
                "methods": ["POST"],
                "route_pattern": "/cors-ch",
                "workflow_id": wf_id,
                "config": {
                    "cors": {
                        "allowed_origins": ["https://allowed.example.com"]
                    }
                }
            })),
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let ch_id = body["data"]["channel_id"].as_str().unwrap().to_string();

    let _ = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{}/status", ch_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();

    // Request from allowed origin — should succeed
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v1/data/cors-ch")
        .header("content-type", "application/json")
        .header("origin", "https://allowed.example.com")
        .body(axum::body::Body::from(r#"{"data":{"key":"value"}}"#))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_ne!(resp.status(), StatusCode::FORBIDDEN);

    // Request from disallowed origin — should be rejected 403
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v1/data/cors-ch")
        .header("content-type", "application/json")
        .header("origin", "https://evil.example.com")
        .body(axum::body::Body::from(r#"{"data":{"key":"value"}}"#))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "FORBIDDEN");

    // Request with no Origin header — should pass (non-browser request)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/cors-ch",
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();
    assert_ne!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_channel_without_cors_allows_any_origin() {
    let app = common::test_app().await;

    common::create_and_activate_channel(
        &app,
        "no-cors-ch",
        json!({
            "name": "No CORS WF",
            "condition": true,
            "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
        }),
    )
    .await;

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v1/data/no-cors-ch")
        .header("content-type", "application/json")
        .header("origin", "https://any-origin.example.com")
        .body(axum::body::Body::from(r#"{"data":{"key":"value"}}"#))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ============================================================
// Per-channel backpressure (concurrency limiting)
// ============================================================

#[tokio::test]
async fn test_channel_backpressure_rejects_when_at_capacity() {
    let app = common::test_app().await;

    // Create workflow
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "Backpressure Test WF",
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
            })),
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let wf_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    let _ = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{}/status", wf_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();

    // Create channel with max_concurrent = 0 (immediately at capacity)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "backpressure-ch",
                "channel_type": "sync",
                "protocol": "http",
                "methods": ["POST"],
                "route_pattern": "/backpressure-ch",
                "workflow_id": wf_id,
                "config": {
                    "backpressure": {
                        "max_concurrent": 0
                    }
                }
            })),
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let ch_id = body["data"]["channel_id"].as_str().unwrap().to_string();

    let _ = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{}/status", ch_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();

    // Request should be rejected with 503 — channel at capacity
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/backpressure-ch",
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "SERVICE_UNAVAILABLE");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("at capacity")
    );
}

#[tokio::test]
async fn test_channel_without_backpressure_allows_requests() {
    let app = common::test_app().await;

    common::create_and_activate_channel(
        &app,
        "no-bp-ch",
        json!({
            "name": "No Backpressure WF",
            "condition": true,
            "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
        }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/no-bp-ch",
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
