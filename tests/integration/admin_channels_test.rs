use crate::common;

use crate::common::{body_json, json_request};
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

// ============================================================
// Channel CRUD Lifecycle
// ============================================================

#[tokio::test]
async fn test_channels_crud_lifecycle() {
    let app = common::test_app().await;

    // First create a workflow to link to
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "ch-test-wf",
                "name": "Channel Test Workflow",
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Activate the workflow
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/workflows/ch-test-wf/status",
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Create a channel (starts as draft)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "test-channel",
                "channel_type": "sync",
                "protocol": "http",
                "methods": ["POST"],
                "route_pattern": "/test",
                "workflow_id": "ch-test-wf",
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    let channel_id = body["data"]["channel_id"].as_str().unwrap().to_string();
    assert_eq!(body["data"]["name"], "test-channel");
    assert_eq!(body["data"]["status"], "draft");

    // Get the channel
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/channels/{}", channel_id),
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["name"], "test-channel");

    // List channels
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/channels", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["total"].as_i64().unwrap() >= 1);
    assert!(!body["data"].as_array().unwrap().is_empty());

    // Activate the channel
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{}/status", channel_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["status"], "active");

    // Archive the channel
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{}/status", channel_id),
            Some(json!({"status": "archived"})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["status"], "archived");

    // Delete the channel
    let resp = app
        .clone()
        .oneshot(json_request(
            "DELETE",
            &format!("/api/v1/admin/channels/{}", channel_id),
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
            &format!("/api/v1/admin/channels/{}", channel_id),
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Archiving a channel must stop the data plane serving it — found by the
/// cluster archive-propagation test: the single-segment name fallback in the
/// dynamic data handler accepted ANY name and ran the engine against an
/// empty workflow set, returning 200 "ok" for archived channels.
#[tokio::test]
async fn archived_channel_is_not_served() {
    let app = common::test_app().await;
    let (channel_id, _wf) = common::create_and_activate_channel_full(
        &app,
        "arch-stop-ch",
        common::simple_log_workflow("Arch Stop WF"),
        serde_json::json!({}),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/arch-stop-ch",
            Some(serde_json::json!({"data": {"x": 1}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "active channel must serve");

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{channel_id}/status"),
            Some(serde_json::json!({"status": "archived"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "archive must succeed");

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/arch-stop-ch",
            Some(serde_json::json!({"data": {"x": 1}})),
        ))
        .await
        .unwrap();
    let status = resp.status();
    let body = common::body_json(resp).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "archived channel must not serve, got {status}: {body}"
    );
}

/// A channel name that never existed must 404 on both the sync and async
/// data planes — not silently execute an empty workflow set.
#[tokio::test]
async fn unknown_channel_name_returns_404() {
    let app = common::test_app().await;
    for uri in [
        "/api/v1/data/never-created-ch",
        "/api/v1/data/never-created-ch/async",
    ] {
        let resp = app
            .clone()
            .oneshot(common::json_request(
                "POST",
                uri,
                Some(serde_json::json!({"data": {"x": 1}})),
            ))
            .await
            .unwrap();
        let status = resp.status();
        let body = common::body_json(resp).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{uri}: unknown channel must 404, got {status}: {body}"
        );
        assert_eq!(body["error"]["code"], "NOT_FOUND", "{uri}: {body}");
    }
}
