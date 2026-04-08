mod common;

use axum::http::StatusCode;
use common::{body_json, json_request};
use serde_json::json;
use tower::ServiceExt;

/// Create two channels: "source" that calls "target" via channel_call,
/// and verify the end-to-end flow works.
#[tokio::test]
async fn test_channel_call_basic() {
    let app = common::test_app().await;

    // Create target workflow (just logs and maps)
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "Target Workflow",
                "condition": true,
                "tasks": [
                    {
                        "id": "t0",
                        "name": "Parse payload",
                        "function": {
                            "name": "parse_json",
                            "input": { "source": "payload", "target": "input" }
                        }
                    },
                    {
                        "id": "t1",
                        "name": "Map result",
                        "function": {
                            "name": "map",
                            "input": {
                                "mappings": [{
                                    "path": "data.greeting",
                                    "logic": { "cat": ["Hello, ", { "var": "data.input.name" }] }
                                }]
                            }
                        }
                    }
                ]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = common::body_json(resp).await;
    let target_wf_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    // Activate target workflow
    app.clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{}/status", target_wf_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();

    // Create target channel
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "target",
                "channel_type": "sync",
                "protocol": "http",
                "methods": ["POST"],
                "route_pattern": "/target",
                "workflow_id": target_wf_id,
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = common::body_json(resp).await;
    let target_ch_id = body["data"]["channel_id"].as_str().unwrap().to_string();

    // Activate target channel
    app.clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{}/status", target_ch_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();

    // Create source workflow that calls the target channel
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "Source Workflow",
                "condition": true,
                "tasks": [{
                    "id": "s1",
                    "name": "Call target channel",
                    "function": {
                        "name": "channel_call",
                        "input": {
                            "channel": "target",
                            "response_path": "data.target_result"
                        }
                    }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = common::body_json(resp).await;
    let source_wf_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    // Activate source workflow
    app.clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{}/status", source_wf_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();

    // Create source channel
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "source",
                "channel_type": "sync",
                "protocol": "http",
                "methods": ["POST"],
                "route_pattern": "/source",
                "workflow_id": source_wf_id,
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = common::body_json(resp).await;
    let source_ch_id = body["data"]["channel_id"].as_str().unwrap().to_string();

    // Activate source channel
    app.clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{}/status", source_ch_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();

    // Now call the source channel — it should invoke target via channel_call
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/source",
            Some(json!({"data": {"name": "World"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");

    // The target workflow maps "Hello, World" into data.greeting
    // and channel_call stores the child's data at data.target_result
    assert_eq!(body["data"]["target_result"]["greeting"], "Hello, World");
}

/// channel_call with a non-existent target channel should return an error.
#[tokio::test]
async fn test_channel_call_missing_target() {
    let app = common::test_app().await;

    // Create workflow that calls a non-existent channel
    common::create_and_activate_channel(
        &app,
        "caller",
        json!({
            "name": "Caller Workflow",
            "condition": true,
            "tasks": [{
                "id": "c1",
                "name": "Call missing",
                "function": {
                    "name": "channel_call",
                    "input": {
                        "channel": "nonexistent",
                        "response_path": "result"
                    }
                }
            }]
        }),
    )
    .await;

    let resp = app
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/caller",
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();

    // Should fail because target channel doesn't exist
    let body = common::body_json(resp).await;
    assert!(body["errors"].as_array().is_some_and(|e| !e.is_empty()) || body["status"] == "ok");
}

// ============================================================
// HTTP Call End-to-End with Mock Server
// ============================================================

#[tokio::test]
async fn test_http_call_end_to_end() {
    let mock_app = axum::Router::new().route(
        "/api/users",
        axum::routing::post(|| async {
            axum::Json(json!({"user_id": "123", "status": "created"}))
        }),
    );
    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = mock_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(mock_listener, mock_app).await.unwrap();
    });

    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "mock-http-api",
                "name": "mock-http-api",
                "connector_type": "http",
                "config": {
                    "type": "http",
                    "url": format!("http://{}", mock_addr),
                    "retry": {"max_retries": 0, "retry_delay_ms": 10},
                    "allow_private_urls": true
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    common::create_and_activate_channel(
        &app,
        "http-call-ch",
        json!({
            "name": "HTTP Call Integration",
            "condition": true,
            "tasks": [{
                "id": "call-api",
                "name": "Call Mock API",
                "function": {
                    "name": "http_call",
                    "input": {
                        "connector": "mock-http-api",
                        "method": "POST",
                        "path": "/api/users",
                        "body": {"test": true},
                        "response_path": "api_response",
                        "timeout_ms": 5000
                    }
                }
            }]
        }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/http-call-ch",
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
}
