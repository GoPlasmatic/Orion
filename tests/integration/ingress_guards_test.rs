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

// ============================================================
// F14: channel_call enforces the target channel's guards
// ============================================================

/// A workflow whose single task channel_calls `target` with a fixed payload.
fn channel_call_workflow(name: &str, target: &str, data: serde_json::Value) -> serde_json::Value {
    json!({
        "name": name,
        "condition": true,
        "tasks": [{
            "id": "s1",
            "name": "Call target",
            "function": {
                "name": "channel_call",
                "input": {
                    "channel": target,
                    "data": data,
                    "response_path": "data.target_result"
                }
            }
        }]
    })
}

#[tokio::test]
async fn test_channel_call_enforces_target_validation_logic() {
    let app = common::test_app().await;

    common::create_and_activate_channel_with_config(
        &app,
        "guarded-target",
        common::simple_log_workflow("Guarded Target WF"),
        require_order_id_config(),
    )
    .await;

    // Caller that sends data violating the target's validation_logic.
    common::create_and_activate_channel(
        &app,
        "caller-invalid",
        channel_call_workflow(
            "Caller Invalid WF",
            "guarded-target",
            json!({"quantity": 1}),
        ),
    )
    .await;

    // Caller that satisfies the target's validation_logic.
    common::create_and_activate_channel(
        &app,
        "caller-valid",
        channel_call_workflow(
            "Caller Valid WF",
            "guarded-target",
            json!({"order_id": "ORD-9"}),
        ),
    )
    .await;

    // The nested validation failure propagates as DataflowError::Validation,
    // which the error envelope maps to 400.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/caller-invalid",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("validation"),
        "channel_call violating target validation_logic must surface a validation error, got: {body}"
    );

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/caller-valid",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(
        body["errors"].as_array().is_some_and(|e| e.is_empty()),
        "valid channel_call must pass target validation, got: {body}"
    );
}

// ============================================================
// F4: metadata.channel is stamped at every entry point
// ============================================================

#[tokio::test]
async fn test_metadata_channel_set_on_http_ingress() {
    let app = common::test_app().await;

    // validation_logic passes only when the ingress stamps metadata.channel
    // with the resolved channel name — a caller-supplied value must lose.
    common::create_and_activate_channel_with_config(
        &app,
        "f4-ch",
        common::simple_log_workflow("F4 WF"),
        json!({
            "validation_logic": { "==": [{ "var": "metadata.channel" }, "f4-ch"] }
        }),
    )
    .await;

    // Sync path — caller tries to spoof metadata.channel; the server value wins.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/f4-ch",
            Some(json!({"data": {}, "metadata": {"channel": "spoofed"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Async path.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/f4-ch/async",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn test_metadata_channel_set_on_channel_call() {
    let app = common::test_app().await;

    // Target accepts only when metadata.channel equals its own name,
    // proving channel_call overrides the parent's channel on the child.
    common::create_and_activate_channel_with_config(
        &app,
        "f4-target",
        common::simple_log_workflow("F4 Target WF"),
        json!({
            "validation_logic": { "==": [{ "var": "metadata.channel" }, "f4-target"] }
        }),
    )
    .await;

    common::create_and_activate_channel(
        &app,
        "f4-caller",
        channel_call_workflow("F4 Caller WF", "f4-target", json!({"ok": true})),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/f4-caller",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(
        body["errors"].as_array().is_some_and(|e| e.is_empty()),
        "child message must carry metadata.channel == target, got: {body}"
    );
}

// ============================================================
// G1: data-plane responses carry sanitized errors only
// ============================================================

#[tokio::test]
async fn test_error_body_is_sanitized_but_trace_keeps_detail() {
    let app = common::test_app().await;

    // db_read against a nonexistent connector fails with a message naming
    // the connector — internal detail that must not reach the caller.
    let workflow = json!({
        "name": "G1 Failing WF",
        "condition": true,
        "continue_on_error": true,
        "tasks": [{
            "id": "t1",
            "name": "Failing DB read",
            "continue_on_error": true,
            "function": {
                "name": "db_read",
                "input": {
                    "connector": "ghost-db-g1",
                    "query": "SELECT 1",
                    "output": "data.db_result"
                }
            }
        }]
    });
    // R5 blocks activating against a missing connector — create then delete
    // it so the runtime failure path is still exercised.
    let conn_id = common::create_connector(&app, common::db_connector("ghost-db-g1")).await;
    common::create_and_activate_channel(&app, "g1-errors-ch", workflow).await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "DELETE",
            &format!("/api/v1/admin/connectors/{conn_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/g1-errors-ch",
            Some(json!({"data": {"x": 1}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;

    let errors = body["errors"].as_array().expect("errors array");
    assert!(!errors.is_empty(), "expected task errors, got: {body}");
    let body_str = serde_json::to_string(&body).unwrap();
    assert!(
        !body_str.contains("ghost-db-g1"),
        "connector name must not leak into the data-plane body: {body_str}"
    );
    assert!(errors[0]["code"].is_string(), "code is kept: {body}");
    assert!(
        body.get("request_id").is_some(),
        "sanitized body carries a correlation id: {body}"
    );

    // The persisted trace keeps the full error detail for operators.
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/data/traces?channel=g1-errors-ch&limit=1",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    let trace_id = list["data"][0]["id"].as_str().expect("trace id");

    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/data/traces/{trace_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let trace = body_json(resp).await;
    let trace_str = serde_json::to_string(&trace).unwrap();
    assert!(
        trace_str.contains("ghost-db-g1"),
        "persisted trace must keep full error detail: {trace_str}"
    );
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
