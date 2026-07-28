//! One output-field name across every handler (proposal F43).
//!
//! Before 1.0 the destination path was called `output` in eight handlers and
//! `response_path` in `http_call` and `channel_call`. 1.0 standardises on
//! `output` and keeps `response_path` working so 0.3.x workflows load
//! unchanged.
//!
//! `channel_call` takes the new name through a serde alias on an Orion-owned
//! struct. `http_call` cannot: dataflow-rs claims that function name as a
//! built-in and deserializes it into its own `HttpCallConfig`, so Orion
//! rewrites the key at the workflow → dataflow boundary. The two mechanisms
//! are different enough that both need covering, in both directions.

use crate::common;

use crate::common::{body_json, json_request};
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

/// Stand up a mock HTTP origin and register it as a connector.
/// Returns the ready app.
async fn app_with_mock_origin() -> axum::Router {
    let mock_app = axum::Router::new().route(
        "/api/users",
        axum::routing::post(|| async { axum::Json(json!({"user_id": "123"})) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, mock_app).await.unwrap();
    });

    let app = common::test_app().await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "mock-origin",
                "name": "mock-origin",
                "connector_type": "http",
                "config": {
                    "type": "http",
                    "url": format!("http://{}", addr),
                    "retry": {"max_retries": 0, "retry_delay_ms": 10},
                    "allow_private_urls": true
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    app
}

/// An `http_call` workflow whose destination-path key is `key`.
fn http_call_workflow(key: &str, path: &str) -> serde_json::Value {
    json!({
        "name": "HTTP Output Naming",
        "condition": true,
        "tasks": [{
            "id": "call",
            "name": "Call origin",
            "function": {
                "name": "http_call",
                "input": {
                    "connector": "mock-origin",
                    "method": "POST",
                    "path": "/api/users",
                    "body": {"test": true},
                    key: path,
                    "timeout_ms": 5000
                }
            }
        }]
    })
}

async fn run_channel(app: &axum::Router, channel: &str) -> serde_json::Value {
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/data/{channel}"),
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

/// The 1.0 name. `http_call` is the case that needs the boundary rewrite —
/// without it the key is dropped by the upstream struct and the response is
/// silently discarded, which is a wrong answer rather than an error.
#[tokio::test]
async fn http_call_accepts_output() {
    let app = app_with_mock_origin().await;
    common::create_and_activate_channel(
        &app,
        "http-output",
        http_call_workflow("output", "data.api_response"),
    )
    .await;

    let body = run_channel(&app, "http-output").await;
    assert_eq!(
        body["data"]["api_response"]["user_id"], "123",
        "`output` must place the response body at the named path, got: {body}"
    );
}

/// The 0.3.x name still loads and still works.
#[tokio::test]
async fn http_call_still_accepts_response_path() {
    let app = app_with_mock_origin().await;
    common::create_and_activate_channel(
        &app,
        "http-legacy",
        http_call_workflow("response_path", "data.api_response"),
    )
    .await;

    let body = run_channel(&app, "http-legacy").await;
    assert_eq!(body["data"]["api_response"]["user_id"], "123");
}

/// When a workflow carries both keys — as one will mid-migration, if an author
/// adds the new name without deleting the old — `output` must win. Silently
/// honouring the deprecated key would make the migration a no-op that looks
/// like it worked.
#[tokio::test]
async fn http_call_output_wins_over_response_path() {
    let app = app_with_mock_origin().await;
    let mut wf = http_call_workflow("output", "data.new_path");
    wf["tasks"][0]["function"]["input"]["response_path"] = json!("data.old_path");

    common::create_and_activate_channel(&app, "http-both", wf).await;

    let body = run_channel(&app, "http-both").await;
    assert_eq!(
        body["data"]["new_path"]["user_id"], "123",
        "`output` must take precedence, got: {body}"
    );
    assert!(
        body["data"]["old_path"].is_null(),
        "the deprecated key must not also be honoured, got: {body}"
    );
}

/// `channel_call` reaches the same contract through a serde alias rather than
/// the boundary rewrite, so it gets its own coverage.
#[tokio::test]
async fn channel_call_accepts_output() {
    let app = common::test_app().await;

    common::create_and_activate_channel(
        &app,
        "leaf",
        json!({
            "name": "Leaf",
            "condition": true,
            "tasks": [{
                "id": "set",
                "name": "Set a marker",
                "function": {
                    "name": "map",
                    "input": {"mappings": [{"path": "data.marker", "logic": {"cat": ["se", "en"]}}]}
                }
            }]
        }),
    )
    .await;

    common::create_and_activate_channel(
        &app,
        "caller-output",
        json!({
            "name": "Caller",
            "condition": true,
            "tasks": [{
                "id": "call",
                "name": "Call leaf",
                "function": {
                    "name": "channel_call",
                    "input": {"channel": "leaf", "output": "data.child"}
                }
            }]
        }),
    )
    .await;

    let body = run_channel(&app, "caller-output").await;
    assert_eq!(
        body["data"]["child"]["marker"], "seen",
        "`output` must place the child response at the named path, got: {body}"
    );
}
