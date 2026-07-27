//! R3: PUT /api/v1/admin/channels/{id} validation.
//!
//! Updates previously ran no validation at all — a draft could be updated
//! with a malformed config, an uncompilable validation_logic, or an emptied
//! route_pattern, surfacing only as a warning at engine reload. These tests
//! pin the create-time checks to the update path, validated against the
//! merged (stored draft ⊕ request) view.

use crate::common::{body_json, json_request, test_app};
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

/// Create a draft REST channel and return its channel_id.
async fn create_draft_channel(app: &axum::Router, name: &str) -> String {
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": name,
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["POST"],
                "route_pattern": format!("/{}", name),
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    body["data"]["channel_id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn put_malformed_config_rejected() {
    let app = test_app().await;
    let id = create_draft_channel(&app, "upd-bad-config").await;

    let resp = app
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/channels/{id}"),
            Some(json!({ "config": { "rate_limit": 42 } })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    let details = body["error"]["details"].as_array().unwrap();
    assert!(
        details.iter().any(|d| d["path"] == "channel.config"),
        "expected channel.config path in details, got {body:?}"
    );
}

#[tokio::test]
async fn put_bad_jsonlogic_in_config_rejected() {
    let app = test_app().await;
    let id = create_draft_channel(&app, "upd-bad-logic").await;

    let resp = app
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/channels/{id}"),
            Some(json!({
                "config": { "validation_logic": { "var": [], "extra_key": 1 } }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    let details = body["error"]["details"].as_array().unwrap();
    assert!(
        details
            .iter()
            .any(|d| d["path"] == "channel.config.validation_logic"),
        "expected channel.config.validation_logic in details, got {body:?}"
    );
}

#[tokio::test]
async fn put_emptying_route_pattern_rejected() {
    let app = test_app().await;
    let id = create_draft_channel(&app, "upd-empty-route").await;

    let resp = app
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/channels/{id}"),
            Some(json!({ "route_pattern": "" })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    let details = body["error"]["details"].as_array().unwrap();
    assert!(
        details
            .iter()
            .any(|d| d["path"] == "channel.route_pattern" && d["code"] == "REQUIRED_FOR_PROTOCOL"),
        "expected channel.route_pattern REQUIRED_FOR_PROTOCOL, got {body:?}"
    );
}

#[tokio::test]
async fn put_emptying_methods_rejected() {
    let app = test_app().await;
    let id = create_draft_channel(&app, "upd-empty-methods").await;

    let resp = app
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/channels/{id}"),
            Some(json!({ "methods": [] })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    let details = body["error"]["details"].as_array().unwrap();
    assert!(
        details.iter().any(|d| d["path"] == "channel.methods"),
        "expected channel.methods in details, got {body:?}"
    );
}

#[tokio::test]
async fn put_valid_update_passes() {
    let app = test_app().await;
    let id = create_draft_channel(&app, "upd-valid").await;

    // Omitting route_pattern/methods keeps the stored values (merged view)
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/channels/{id}"),
            Some(json!({
                "name": "Updated Name",
                "config": {
                    "timeout_ms": 5000,
                    "validation_logic": { "!!": { "var": "data.id" } }
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["name"], "Updated Name");

    // Replacing the protocol fields with valid values also passes
    let resp = app
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/channels/{id}"),
            Some(json!({
                "methods": ["GET", "POST"],
                "route_pattern": "/upd-valid/{item}"
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn put_missing_channel_returns_404() {
    let app = test_app().await;
    let resp = app
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/channels/does-not-exist",
            Some(json!({ "name": "whatever" })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
