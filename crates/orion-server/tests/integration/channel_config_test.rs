use crate::common;

use crate::common::{body_json, json_request};
use axum::http::StatusCode;
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

    // Connector pointing to the slow mock server.
    common::create_http_connector(&app, "slow-api", mock_addr).await;

    // A workflow that calls the slow endpoint, behind a channel with a 50ms
    // timeout (the mock server takes 200ms, so the request times out).
    common::create_and_activate_channel_with_config(
        &app,
        "timeout-ch",
        common::workflow_with_tasks(
            "Timeout Test Workflow",
            json!([{
                "id": "slow-call",
                "name": "Slow HTTP Call",
                "function": {
                    "name": "http_call",
                    "input": {
                        "connector": "slow-api",
                        "method": "POST",
                        "path": "/slow",
                        "body": {"test": true},
                        // A path: `"response"` names no context root and
                        // is refused at upload (O3's named break, pinned by
                        // `channel_load_refusal_test::
                        // an_output_that_is_no_path_quarantines_its_channel`).
                        "output": "data.response",
                        "timeout_ms": 5000
                    }
                }
            }]),
        ),
        json!({"timeout_ms": 50}),
    )
    .await;

    // Send request — expect 504 Gateway Timeout
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/timeout-ch",
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
        common::simple_log_workflow("No Timeout Workflow"),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/no-timeout-ch",
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_response_body_carries_no_internal_rollout_key() {
    // The rollout bucket is a message field and never enters `data`, so no
    // success body carries a `_rollout_bucket` the caller never sent (nor does
    // traces.result_json, proposal F31). The assertion pins that observable
    // API contract.
    let app = common::test_app().await;

    common::create_and_activate_channel(
        &app,
        "no-leak-ch",
        common::simple_log_workflow("No Leak Workflow"),
    )
    .await;

    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v2/data/no-leak-ch",
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = common::body_json(resp).await;
    let data = body["data"].as_object().expect("data object");
    assert!(
        !data.contains_key("_rollout_bucket"),
        "internal routing key leaked into the response: {}",
        body["data"]
    );
}

// ============================================================
// Per-channel input validation via validation_logic
// ============================================================

#[tokio::test]
async fn test_channel_validation_logic_rejects_invalid_input() {
    let app = common::test_app().await;

    // A channel with validation_logic: order_id must exist AND quantity > 0.
    common::create_and_activate_channel_with_config(
        &app,
        "validated-ch",
        common::simple_log_workflow("Validation Test WF"),
        json!({
            "validation_logic": {
                "and": [
                    { "!!": { "var": "data.order_id" } },
                    { ">": [{ "var": "data.quantity" }, 0] }
                ]
            }
        }),
    )
    .await;

    // Valid input — should pass
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/validated-ch",
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
            "/api/v2/data/validated-ch",
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
            "/api/v2/data/validated-ch",
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
        common::simple_log_workflow("No Validation WF"),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/no-validation-ch",
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

    // A channel with CORS restricted to https://allowed.example.com.
    common::create_and_activate_channel_with_config(
        &app,
        "cors-ch",
        common::simple_log_workflow("CORS Test WF"),
        json!({"origin_allow_list": ["https://allowed.example.com"]}),
    )
    .await;

    // Request from allowed origin — should succeed
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v2/data/cors-ch")
        .header("content-type", "application/json")
        .header("origin", "https://allowed.example.com")
        .body(axum::body::Body::from(r#"{"data":{"key":"value"}}"#))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_ne!(resp.status(), StatusCode::FORBIDDEN);

    // Request from disallowed origin — should be rejected 403
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v2/data/cors-ch")
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
            "/api/v2/data/cors-ch",
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
        common::simple_log_workflow("No CORS WF"),
    )
    .await;

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v2/data/no-cors-ch")
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

    // A channel with max_concurrent_per_node = 0: immediately at capacity.
    common::create_and_activate_channel_with_config(
        &app,
        "backpressure-ch",
        common::simple_log_workflow("Backpressure Test WF"),
        json!({"backpressure": {"max_concurrent_per_node": 0}}),
    )
    .await;

    // Request should be rejected with 503 — channel at capacity
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/backpressure-ch",
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
        common::simple_log_workflow("No Backpressure WF"),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/no-bp-ch",
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
