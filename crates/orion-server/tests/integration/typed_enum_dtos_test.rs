//! A4 (Tier-1 ergonomics): typed-enum DTOs for channel_type / protocol / connector_type.
//!
//! Verifies that:
//!   - v0.1.1 lowercase wire values ("sync", "rest", "kafka", "http") still create
//!   - the deserializer is case-insensitive (e.g. "SYNC", "Rest", "Kafka")
//!   - bad enum values fail with 400 (v0.1 status preserved by OrionJson extractor)
//!   - the response field shape is unchanged (still lowercase strings)

use crate::common::{body_json, json_request, test_app};
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn v01_lowercase_wire_values_still_accepted() {
    let app = test_app().await;
    // sync REST channel — the exact shape v0.1.1 clients send.
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "v01-style",
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["POST"],
                "route_pattern": "/v01-style",
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    // Response keeps the v0.1 lowercase shape.
    assert_eq!(body["data"]["channel_type"], "sync");
    assert_eq!(body["data"]["protocol"], "rest");
}

#[tokio::test]
async fn channel_type_and_protocol_are_case_insensitive() {
    let app = test_app().await;
    // Mixed casing — would have failed under v0.1's strict lowercase string
    // validation. Now succeeds because A4's deserializer lowercases before
    // matching variants.
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "mixed-case",
                "channel_type": "SYNC",
                "protocol": "Rest",
                "methods": ["POST"],
                "route_pattern": "/mixed-case",
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    // Canonical lowercase is preserved on read so existing clients that
    // compare `==` to "sync"/"rest" keep working.
    assert_eq!(body["data"]["channel_type"], "sync");
    assert_eq!(body["data"]["protocol"], "rest");
}

#[tokio::test]
async fn connector_type_is_case_insensitive() {
    let app = test_app().await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "name": "uppercase-http",
                "connector_type": "HTTP",
                "config": { "url": "https://example.com" }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["connector_type"], "http");
}

#[tokio::test]
async fn invalid_channel_protocol_is_rejected_at_deserialization_with_400() {
    let app = test_app().await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "bad-protocol",
                "channel_type": "sync",
                "protocol": "grpc",
                "methods": ["POST"],
                "route_pattern": "/x",
            })),
        ))
        .await
        .unwrap();
    // v0.1 returned 400 for bad protocol strings; we preserve that status via
    // the OrionJson extractor even though serde's default would be 422.
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    let msg = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("rest") || msg.contains("kafka"),
        "message should list allowed protocols, got {msg}"
    );
}

#[tokio::test]
async fn invalid_channel_type_is_rejected_with_400() {
    let app = test_app().await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "bad-channel-type",
                "channel_type": "stream",
                "protocol": "rest",
                "methods": ["POST"],
                "route_pattern": "/x",
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    let msg = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("sync") && msg.contains("async"),
        "message should list allowed channel types, got {msg}"
    );
}
