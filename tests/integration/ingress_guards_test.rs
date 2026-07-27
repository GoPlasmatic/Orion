//! Ingress guard unification tests (proposal S1/F14/F4/G1):
//! every entry point enforces the target channel's declared contract.

use crate::common;
use crate::common::{body_json, json_request, post_with_idempotency_key};
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

/// Channel config requiring `data.order_id` to be present.
fn require_order_id_config() -> serde_json::Value {
    json!({
        "validation_logic": { "!!": { "var": "data.order_id" } }
    })
}

// ============================================================
// S1: async submissions cannot bypass per-channel guards
// ============================================================

#[tokio::test]
async fn test_async_path_rejects_invalid_input_per_validation_logic() {
    let app = common::test_app().await;

    common::create_and_activate_channel_with_config(
        &app,
        "async-validated-ch",
        common::simple_log_workflow("Async Validated WF"),
        require_order_id_config(),
    )
    .await;

    // Regression for the `/async` bypass: invalid input must be rejected
    // with 400 before a trace is created, exactly like the sync path.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/async-validated-ch/async",
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

    // Valid input still gets accepted asynchronously.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/async-validated-ch/async",
            Some(json!({"data": {"order_id": "ORD-1"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn test_async_path_honors_deduplication() {
    let app = common::test_app().await;

    common::create_and_activate_channel_with_config(
        &app,
        "async-dedup-ch",
        common::simple_log_workflow("Async Dedup WF"),
        json!({
            "deduplication": { "header": "Idempotency-Key", "window_secs": 300 }
        }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(post_with_idempotency_key(
            "/api/v1/data/async-dedup-ch/async",
            "async-key-1",
            json!({"data": {"n": 1}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Same idempotency key within the window — must be rejected 409.
    let resp = app
        .clone()
        .oneshot(post_with_idempotency_key(
            "/api/v1/data/async-dedup-ch/async",
            "async-key-1",
            json!({"data": {"n": 2}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // A different key is accepted.
    let resp = app
        .clone()
        .oneshot(post_with_idempotency_key(
            "/api/v1/data/async-dedup-ch/async",
            "async-key-2",
            json!({"data": {"n": 3}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn test_async_path_honors_cors_allowlist() {
    let app = common::test_app().await;

    common::create_and_activate_channel_with_config(
        &app,
        "async-cors-ch",
        common::simple_log_workflow("Async CORS WF"),
        json!({
            "cors": { "allowed_origins": ["https://allowed.example"] }
        }),
    )
    .await;

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v1/data/async-cors-ch/async")
        .header("content-type", "application/json")
        .header("origin", "https://evil.example")
        .body(axum::body::Body::from(
            serde_json::to_string(&json!({"data": {}})).unwrap(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
