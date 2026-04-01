mod common;

use axum::http::StatusCode;
use common::{body_json, json_request, poll_trace_until_done};
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
        json!({
            "name": "Events Workflow",
            "condition": true,
            "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"event"}}}]
        }),
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
        json!({
            "name": "Orders Workflow",
            "condition": true,
            "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"order"}}}]
        }),
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

    // Poll until completion
    let trace = poll_trace_until_done(&app, &trace_id, 30).await;
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

    // Create and activate a channel
    common::create_and_activate_channel(
        &app,
        "events",
        json!({
            "name": "Events Workflow",
            "condition": true,
            "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"event"}}}]
        }),
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

    // Create and activate a channel
    common::create_and_activate_channel(
        &app,
        "orders",
        json!({
            "name": "Orders Workflow",
            "condition": true,
            "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"order"}}}]
        }),
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
        json!({
            "name": "Events Workflow",
            "condition": true,
            "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"event"}}}]
        }),
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

    // Create and activate two channels
    common::create_and_activate_channel(
        &app,
        "channel-a",
        json!({
            "name": "Channel A Workflow",
            "condition": true,
            "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"a"}}}]
        }),
    )
    .await;

    common::create_and_activate_channel(
        &app,
        "channel-b",
        json!({
            "name": "Channel B Workflow",
            "condition": true,
            "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"b"}}}]
        }),
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
        json!({
            "name": "Test Workflow",
            "condition": true,
            "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
        }),
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

    let body = poll_trace_until_done(&app, &trace_id, 40).await;
    assert_eq!(body["status"], "completed");
    assert!(body.get("message").is_some());
    assert!(body.get("started_at").is_some());
    assert!(body.get("completed_at").is_some());
}
