use crate::common;

use crate::common::{body_json, json_request, poll_trace_until_done};
use axum::http::StatusCode;
use serde_json::json;
use std::time::Duration;
use tower::ServiceExt;

// ============================================================
// Basic async submission
// ============================================================

#[tokio::test]
async fn test_async_submit_returns_202_with_trace_id() {
    let app = common::test_app().await;

    // Create and activate a channel for the async endpoint
    common::create_and_activate_channel(
        &app,
        "events",
        common::simple_log_workflow("Events Workflow"),
    )
    .await;

    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/data/events/async",
            Some(json!({"data": {"event": "click", "user_id": "u1"}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    assert!(body["trace_id"].is_string());
    assert!(!body["trace_id"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn test_async_trace_completes_successfully() {
    let app = common::test_app().await;

    // Create and activate a channel
    common::create_and_activate_channel(
        &app,
        "orders",
        common::simple_log_workflow("Orders Workflow"),
    )
    .await;

    // Submit async trace
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/orders/async",
            Some(json!({"data": {"order_id": 42, "amount": 99.99}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    let trace_id = body["trace_id"].as_str().unwrap().to_string();
    let token = body["trace_token"].as_str().unwrap().to_string();

    // Poll until completion
    let trace = poll_trace_until_done(&app, &trace_id, 30, Some(&token)).await;
    assert_eq!(trace["status"], "completed");
    assert!(trace.get("message").is_some());
}

#[tokio::test]
async fn test_async_trace_with_no_matching_channel() {
    let app = common::test_app().await;

    // Submit to a channel with no channel/workflow configured
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/no-workflows-channel/async",
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    let trace_id = body["trace_id"].as_str().unwrap().to_string();
    let token = body["trace_token"].as_str().unwrap().to_string();

    // Trace should still complete (no-op)
    let trace = poll_trace_until_done(&app, &trace_id, 30, Some(&token)).await;
    assert_eq!(trace["status"], "completed");
}

#[tokio::test]
async fn test_async_trace_empty_channel_rejected() {
    let app = common::test_app().await;

    // Use percent-encoded space as channel, which trims to empty
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/data/%20/async",
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ============================================================
// Multiple concurrent async traces
// ============================================================

#[tokio::test]
async fn test_multiple_concurrent_async_traces() {
    let app = common::test_app().await;

    // Create and activate a channel
    common::create_and_activate_channel(
        &app,
        "events",
        common::simple_log_workflow("Events Workflow"),
    )
    .await;

    // Submit 10 traces concurrently
    let mut trace_ids = Vec::new();
    for i in 0..10 {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/data/events/async",
                Some(json!({"data": {"index": i}})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let body = body_json(resp).await;
        trace_ids.push((
            body["trace_id"].as_str().unwrap().to_string(),
            body["trace_token"].as_str().unwrap().to_string(),
        ));
    }

    // Wait for all traces to complete
    for (trace_id, token) in &trace_ids {
        let trace = poll_trace_until_done(&app, trace_id, 40, Some(token)).await;
        let status = trace["status"].as_str().unwrap();
        assert!(
            status == "completed" || status == "failed",
            "Trace {} should reach terminal status, got: {}",
            trace_id,
            status,
        );
    }
}

// ============================================================
// Trace listing, pagination, and filtering
// ============================================================

#[tokio::test]
async fn test_trace_list_pagination() {
    let app = common::test_app().await;

    // Create and activate a channel
    common::create_and_activate_channel(
        &app,
        "orders",
        common::simple_log_workflow("Orders Workflow"),
    )
    .await;

    // Submit 5 async traces
    for i in 0..5 {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/data/orders/async",
                Some(json!({"data": {"item": i}})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    // Brief pause so traces are visible
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Page 1: limit=2, offset=0
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/data/traces?limit=2&offset=0",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"].as_array().unwrap().len(), 2);
    assert!(body["total"].as_i64().unwrap() >= 5);
    assert_eq!(body["limit"], 2);
    assert_eq!(body["offset"], 0);

    // Page 2: limit=2, offset=2
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/data/traces?limit=2&offset=2",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"].as_array().unwrap().len(), 2);
    assert_eq!(body["offset"], 2);
}

#[tokio::test]
async fn test_trace_list_filter_by_status() {
    let app = common::test_app().await;

    // Create and activate a channel
    common::create_and_activate_channel(
        &app,
        "events",
        common::simple_log_workflow("Events Workflow"),
    )
    .await;

    // Submit a trace and wait for it to complete
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/events/async",
            Some(json!({"data": {"event": "test"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    let trace_id = body["trace_id"].as_str().unwrap().to_string();
    let token = body["trace_token"].as_str().unwrap().to_string();

    // Wait for completion
    poll_trace_until_done(&app, &trace_id, 30, Some(&token)).await;

    // Filter by completed status
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/data/traces?status=completed",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let traces = body["data"].as_array().unwrap();
    assert!(!traces.is_empty());
    for trace in traces {
        assert_eq!(trace["status"], "completed");
    }
}

#[tokio::test]
async fn test_trace_list_filter_by_channel() {
    let app = common::test_app().await;

    // Create and activate two channels
    common::create_and_activate_channel(
        &app,
        "channel-a",
        common::simple_log_workflow("Channel A Workflow"),
    )
    .await;

    common::create_and_activate_channel(
        &app,
        "channel-b",
        common::simple_log_workflow("Channel B Workflow"),
    )
    .await;

    // Submit traces on two different channels
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/channel-a/async",
            Some(json!({"data": {"src": "a"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/channel-b/async",
            Some(json!({"data": {"src": "b"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Brief pause
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Filter by channel-a
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/data/traces?channel=channel-a",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let traces = body["data"].as_array().unwrap();
    assert_eq!(traces.len(), 1);
    assert_eq!(traces[0]["channel"], "channel-a");
}

// ============================================================
// Get Trace - completed with result
// ============================================================

#[tokio::test]
async fn test_get_completed_trace_with_result() {
    let app = common::test_app().await;

    common::create_and_activate_channel(
        &app,
        "test-ch",
        common::simple_log_workflow("Test Workflow"),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/test-ch/async",
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    let trace_id = body["trace_id"].as_str().unwrap().to_string();
    let token = body["trace_token"].as_str().unwrap().to_string();

    let body = poll_trace_until_done(&app, &trace_id, 40, Some(&token)).await;
    assert_eq!(body["status"], "completed");
    assert!(body.get("message").is_some());
    assert!(body.get("started_at").is_some());
    assert!(body.get("completed_at").is_some());
}

// ============================================================
// Credential redaction (S10/S14)
// ============================================================

/// S10: credential-bearing request headers must be masked before the header
/// map enters workflow metadata, because the async path persists the whole
/// engine message into `traces.result_json`. Asserts the at-rest state via
/// the repository, independent of what the HTTP trace projection exposes.
#[tokio::test]
async fn test_credential_headers_masked_at_rest_in_async_trace() {
    use axum::body::Body;
    use axum::http::Request;

    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());

    common::create_and_activate_channel(
        &app,
        "secure-events",
        common::simple_log_workflow("Secure Events"),
    )
    .await;

    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/data/secure-events/async")
        .header("content-type", "application/json")
        .header("authorization", "Bearer SUPER-SECRET-TOKEN-A")
        .header("cookie", "session=SECRET-COOKIE-A")
        .header("proxy-authorization", "Basic PROXY-SECRET-B")
        .header("x-api-key", "APIKEY-SECRET-C")
        .header("x-tenant", "acme")
        .body(Body::from(
            serde_json::to_vec(&json!({"data": {"event": "click"}})).unwrap(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let submit = body_json(resp).await;
    let trace_id = submit["trace_id"].as_str().unwrap().to_string();
    let token = submit["trace_token"].as_str().unwrap().to_string();

    let trace = poll_trace_until_done(&app, &trace_id, 40, Some(&token)).await;
    assert_eq!(trace["status"], "completed");

    let row = state.trace_repo.get_by_id(&trace_id).await.unwrap();
    let result_json = row
        .result_json
        .expect("completed async trace stores result_json");
    for secret in [
        "SUPER-SECRET-TOKEN-A",
        "SECRET-COOKIE-A",
        "PROXY-SECRET-B",
        "APIKEY-SECRET-C",
    ] {
        assert!(
            !result_json.contains(secret),
            "persisted trace leaks credential {secret}: {result_json}"
        );
    }

    let msg: serde_json::Value = serde_json::from_str(&result_json).unwrap();
    let headers = &msg["context"]["metadata"]["headers"];
    assert_eq!(headers["authorization"], "******");
    assert_eq!(headers["cookie"], "******");
    assert_eq!(headers["proxy-authorization"], "******");
    assert_eq!(headers["x-api-key"], "******");
    // Non-credential headers stay readable for validation_logic and debugging.
    assert_eq!(headers["x-tenant"], "acme");
}

/// S14: on a default config (admin auth off), an anonymous caller polling
/// `GET /traces/{id}` must not be able to read another caller's request
/// context. The persisted message's `context.metadata` (which carries the
/// header map) is stripped from the read projection, and the list endpoint
/// serves a payload-free projection.
#[tokio::test]
async fn test_trace_read_does_not_expose_request_context() {
    use axum::body::Body;
    use axum::http::Request;

    let app = common::test_app().await;

    common::create_and_activate_channel(
        &app,
        "s14-channel",
        common::simple_log_workflow("S14 Workflow"),
    )
    .await;

    // Caller A submits with credentials attached.
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/data/s14-channel/async")
        .header("content-type", "application/json")
        .header("authorization", "Bearer S14-SECRET-TOKEN")
        .header("cookie", "session=S14-SECRET-COOKIE")
        .body(Body::from(
            serde_json::to_vec(&json!({"data": {"event": "purchase"}})).unwrap(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let submit = body_json(resp).await;
    let trace_id = submit["trace_id"].as_str().unwrap().to_string();
    let token = submit["trace_token"].as_str().unwrap().to_string();

    // The submitter polls with its capability token (default config: no
    // admin auth) — R12 requires the token even here.
    let trace = poll_trace_until_done(&app, &trace_id, 40, Some(&token)).await;
    assert_eq!(trace["status"], "completed");

    // The message is served, but without the submitter's request context.
    assert!(trace.get("message").is_some());
    assert!(
        trace["message"]["context"].get("metadata").is_none(),
        "trace read must strip context.metadata (S14), got {trace}"
    );
    let serialized = trace.to_string();
    assert!(!serialized.contains("S14-SECRET-TOKEN"));
    assert!(!serialized.contains("S14-SECRET-COOKIE"));

    // The list endpoint serves no payload fields at all.
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/data/traces", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    let row = list["data"]
        .as_array()
        .and_then(|a| a.iter().find(|r| r["id"] == trace_id.as_str()))
        .expect("submitted trace must appear in the list");
    for forbidden in ["input_json", "result_json", "task_trace_json"] {
        assert!(
            row.get(forbidden).is_none() || row[forbidden].is_null(),
            "list row must not carry {forbidden} (S14), row={row}"
        );
    }
}
