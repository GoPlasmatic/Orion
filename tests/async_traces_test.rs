mod common;

use axum::http::StatusCode;
use common::{body_json, json_request};
use serde_json::json;
use std::time::Duration;
use tower::ServiceExt;

/// Helper: poll a trace until it reaches a terminal status or max iterations.
async fn poll_trace_until_done(
    app: &axum::Router,
    trace_id: &str,
    max_polls: usize,
) -> serde_json::Value {
    let mut body = json!(null);
    for _ in 0..max_polls {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let resp = app
            .clone()
            .oneshot(json_request(
                "GET",
                &format!("/api/v1/data/traces/{}", trace_id),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        body = body_json(resp).await;
        let status = body["status"].as_str().unwrap_or("");
        if status == "completed" || status == "failed" {
            break;
        }
    }
    body
}

// ============================================================
// Basic async submission
// ============================================================

#[tokio::test]
async fn test_async_submit_returns_202_with_trace_id() {
    let app = common::test_app().await;

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

    // Poll until completion
    let trace = poll_trace_until_done(&app, &trace_id, 30).await;
    assert_eq!(trace["status"], "completed");
    assert!(trace.get("message").is_some());
}

#[tokio::test]
async fn test_async_trace_with_no_matching_rules() {
    let app = common::test_app().await;

    // Submit to a channel with no rules configured
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/no-rules-channel/async",
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    let trace_id = body["trace_id"].as_str().unwrap().to_string();

    // Trace should still complete (no-op)
    let trace = poll_trace_until_done(&app, &trace_id, 30).await;
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
        trace_ids.push(body["trace_id"].as_str().unwrap().to_string());
    }

    // Wait for all traces to complete
    for trace_id in &trace_ids {
        let trace = poll_trace_until_done(&app, trace_id, 40).await;
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

    // Wait for completion
    poll_trace_until_done(&app, &trace_id, 30).await;

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
