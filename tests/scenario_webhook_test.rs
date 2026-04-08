mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use common::{
    body_json, create_and_activate_channel, create_and_activate_channel_with_config, json_request,
    poll_trace_until_done, workflow_with_tasks,
};

// ============================================================
// 1. Webhook normalization with channel_call forwarding
// ============================================================

/// Real-world scenario: A webhook receiver normalizes vendor-specific payloads
/// (e.g. Stripe) into a canonical format and forwards them to an internal
/// processing channel via `channel_call`.
#[tokio::test]
async fn test_webhook_normalization() {
    let app = common::test_app().await;

    // ---- Step 1: Create the "processor" channel first ----
    // The processor parses JSON and maps a simple "processed" flag.
    let processor_workflow = workflow_with_tasks(
        "Processor Workflow",
        json!([
            {
                "id": "p1",
                "name": "Parse",
                "function": {
                    "name": "parse_json",
                    "input": { "source": "payload", "target": "input" }
                }
            },
            {
                "id": "p2",
                "name": "Mark processed",
                "function": {
                    "name": "map",
                    "input": {
                        "mappings": [
                            { "path": "data.processed", "logic": true }
                        ]
                    }
                }
            }
        ]),
    );

    create_and_activate_channel(&app, "processor", processor_workflow).await;

    // ---- Step 2: Create the "webhook-receiver" channel ----
    // It parses the raw webhook payload, normalizes it (extract event_type,
    // amount, and tag source), then forwards to "processor" via channel_call.
    let receiver_workflow = workflow_with_tasks(
        "Webhook Receiver Workflow",
        json!([
            {
                "id": "t1",
                "name": "Parse payload",
                "function": {
                    "name": "parse_json",
                    "input": { "source": "payload", "target": "input" }
                }
            },
            {
                "id": "t2",
                "name": "Normalize",
                "function": {
                    "name": "map",
                    "input": {
                        "mappings": [
                            {
                                "path": "data.event_type",
                                "logic": {
                                    "if": [
                                        { "==": [{ "var": "data.input.type" }, "charge.succeeded"] },
                                        "payment_received",
                                        "unknown"
                                    ]
                                }
                            },
                            {
                                "path": "data.amount",
                                "logic": { "var": "data.input.data.amount" }
                            },
                            {
                                "path": "data.source",
                                "logic": "stripe"
                            }
                        ]
                    }
                }
            },
            {
                "id": "t3",
                "name": "Forward to processor",
                "function": {
                    "name": "channel_call",
                    "input": {
                        "channel": "processor",
                        "response_path": "data.processor_result"
                    }
                }
            }
        ]),
    );

    create_and_activate_channel(&app, "webhook-receiver", receiver_workflow).await;

    // ---- Step 3: Send a Stripe-like webhook payload ----
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/webhook-receiver",
            Some(json!({
                "data": {
                    "type": "charge.succeeded",
                    "data": {
                        "amount": 5000,
                        "currency": "usd"
                    }
                }
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;

    // The engine should report success
    assert_eq!(body["status"], "ok");

    // Normalization assertions
    assert_eq!(
        body["data"]["event_type"], "payment_received",
        "event_type should be normalized from charge.succeeded"
    );
    assert_eq!(
        body["data"]["source"], "stripe",
        "source should be tagged as stripe"
    );
    assert_eq!(
        body["data"]["amount"], 5000,
        "amount should be extracted from nested payload"
    );

    // channel_call forwarding assertion: the processor result should be present
    assert!(
        body["data"]["processor_result"].is_object(),
        "processor_result should be populated by channel_call; got: {}",
        body["data"]["processor_result"]
    );
    assert_eq!(
        body["data"]["processor_result"]["processed"], true,
        "processor should have marked the data as processed"
    );
}

// ============================================================
// 2. Async webhook processing with trace polling
// ============================================================

/// Real-world scenario: A webhook is submitted for async background processing.
/// The client receives a 202 with a trace ID and polls until the trace completes.
#[tokio::test]
async fn test_webhook_async_processing() {
    let app = common::test_app().await;

    // Create a simple channel with a log workflow for async processing
    create_and_activate_channel(
        &app,
        "async-webhook",
        common::simple_log_workflow("Async Webhook Workflow"),
    )
    .await;

    // ---- Step 1: POST to the async endpoint ----
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/async-webhook/async",
            Some(json!({
                "data": {
                    "event": "invoice.paid",
                    "invoice_id": "inv_12345",
                    "amount": 9900
                }
            })),
        ))
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "Async submission should return 202 Accepted"
    );

    // ---- Step 2: Extract the trace ID ----
    let body = body_json(resp).await;
    let trace_id = body["trace_id"]
        .as_str()
        .expect("Response should contain a trace_id");
    assert!(!trace_id.is_empty(), "trace_id should not be empty");

    // ---- Step 3: Poll until the trace reaches a terminal state ----
    let trace = poll_trace_until_done(&app, trace_id, 30).await;

    assert_eq!(
        trace["status"], "completed",
        "Async trace should complete successfully"
    );
}

// ============================================================
// 3. Webhook deduplication via idempotency header
// ============================================================

/// Real-world scenario: Webhook providers often send duplicate delivery attempts.
/// A channel configured with deduplication rejects repeated requests bearing the
/// same idempotency key within the configured time window.
#[tokio::test]
async fn test_webhook_dedup_via_idempotency() {
    let app = common::test_app().await;

    // Create a channel with deduplication enabled, keyed on "X-Webhook-Id" header
    let dedup_config = json!({
        "deduplication": {
            "header": "X-Webhook-Id",
            "window_secs": 300
        }
    });

    create_and_activate_channel_with_config(
        &app,
        "dedup-webhook",
        common::simple_log_workflow("Dedup Webhook Workflow"),
        dedup_config,
    )
    .await;

    let payload = json!({
        "data": {
            "event": "payment.completed",
            "payment_id": "pay_abc123"
        }
    });
    let payload_str = serde_json::to_string(&payload).unwrap();

    // ---- Step 1: First request with X-Webhook-Id: wh-unique-001 -> 200 OK ----
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/data/dedup-webhook")
        .header("content-type", "application/json")
        .header("X-Webhook-Id", "wh-unique-001")
        .body(Body::from(payload_str.clone()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "First request should succeed"
    );
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");

    // ---- Step 2: Duplicate request with same X-Webhook-Id -> 409 Conflict ----
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/data/dedup-webhook")
        .header("content-type", "application/json")
        .header("X-Webhook-Id", "wh-unique-001")
        .body(Body::from(payload_str.clone()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "Duplicate request with same idempotency key should be rejected with 409"
    );
    let body = body_json(resp).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("Duplicate"),
        "Error message should mention duplicate: {}",
        body
    );

    // ---- Step 3: Different X-Webhook-Id -> 200 OK ----
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/data/dedup-webhook")
        .header("content-type", "application/json")
        .header("X-Webhook-Id", "wh-unique-002")
        .body(Body::from(payload_str.clone()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "Request with a different idempotency key should succeed"
    );
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
}
