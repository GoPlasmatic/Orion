mod common;

use axum::http::StatusCode;
use common::{body_json, json_request};
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn test_connectors_crud_lifecycle() {
    let app = common::test_app().await;

    // Create a connector
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "name": "test-http",
                "connector_type": "http",
                "config": {
                    "url": "https://example.com/api",
                    "method": "POST",
                    "headers": { "Authorization": "Bearer secret-token-123" }
                }
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    let connector_id = body["data"]["id"].as_str().unwrap().to_string();
    assert_eq!(body["data"]["name"], "test-http");
    assert_eq!(body["data"]["connector_type"], "http");

    // Get the connector
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/connectors/{}", connector_id),
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["name"], "test-http");

    // List connectors (should include the internal __data_api__ + our new one)
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/connectors", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let connectors = body["data"].as_array().unwrap();
    assert!(!connectors.is_empty());

    // Update the connector
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/connectors/{}", connector_id),
            Some(json!({
                "config": {
                    "url": "https://example.com/v2/api",
                    "method": "PUT"
                }
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["name"], "test-http");

    // Delete the connector
    let resp = app
        .clone()
        .oneshot(json_request(
            "DELETE",
            &format!("/api/v1/admin/connectors/{}", connector_id),
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
            &format!("/api/v1/admin/connectors/{}", connector_id),
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_data_sync_processing() {
    let app = common::test_app().await;

    // Create a rule that matches the "orders" channel
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "name": "Order Logger",
                "channel": "orders",
                "condition": true,
                "tasks": [{
                    "id": "t1",
                    "name": "Log",
                    "function": { "name": "log", "input": { "message": "order received" } }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Send data to the orders channel
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/orders",
            Some(json!({
                "data": { "order_id": "ORD-001", "amount": 150 },
                "metadata": { "source": "test" }
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body.get("id").is_some());
    assert!(body.get("data").is_some());
}

#[tokio::test]
async fn test_data_async_processing() {
    let app = common::test_app().await;

    // Submit async job
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/events/async",
            Some(json!({
                "data": { "event": "click", "user_id": "u1" }
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    let job_id = body["job_id"].as_str().unwrap().to_string();

    // Poll job status with retry until completed or timeout
    let mut final_status = String::new();
    for _ in 0..20 {
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let resp = app
            .clone()
            .oneshot(json_request(
                "GET",
                &format!("/api/v1/data/jobs/{}", job_id),
                None,
            ))
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        final_status = body["status"].as_str().unwrap().to_string();
        if final_status == "completed" || final_status == "failed" {
            break;
        }
    }
    assert_eq!(final_status, "completed");
}

#[tokio::test]
async fn test_data_batch_processing() {
    let app = common::test_app().await;

    // Batch process
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/batch",
            Some(json!({
                "messages": [
                    { "channel": "orders", "data": { "id": 1 } },
                    { "channel": "events", "data": { "id": 2 } }
                ]
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
}

#[tokio::test]
async fn test_health_endpoint() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/health", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert!(body.get("uptime_seconds").is_some());
    assert!(body.get("version").is_some());
    assert!(body.get("components").is_some());
    assert_eq!(body["components"]["database"], "ok");
    assert_eq!(body["components"]["engine"], "ok");
}

#[tokio::test]
async fn test_metrics_endpoint() {
    let app = common::test_app().await;

    // Process a message first to generate some metrics
    let _ = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/test-channel",
            Some(json!({
                "data": { "key": "value" },
                "metadata": {}
            })),
        ))
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/metrics", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    // Metrics should contain Prometheus format text (may be empty if no metrics recorded yet)
    assert!(!body.contains("error"));
}
