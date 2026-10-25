//! A4 (Tier-1 ergonomics): typed enums for a channel's `channel_type` and
//! `protocol`, as a channel definition takes them.
//!
//! Verifies that:
//!   - v0.1.1 lowercase wire values ("sync", "rest") upload
//!   - the deserializer is case-insensitive (e.g. "SYNC", "Rest")
//!   - bad enum values are refused, naming the allowed values
//!   - the stored definition reads back in the canonical lowercase

use crate::common::deploy;
use crate::common::{body_json, json_request, test_app};
use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

/// The stored definition a digest names.
async fn read_definition(app: &axum::Router, digest: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v2/admin/definitions/{digest}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

/// Upload a channel definition, answering the status and the body as text —
/// a refused enum is refused while the body is deserialized, before any
/// JSON envelope is built.
async fn upload_text(app: &axum::Router, definition: Value) -> (StatusCode, String) {
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/admin/definitions",
            Some(json!({"kind": "channel", "definition": definition})),
        ))
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn v01_lowercase_wire_values_still_accepted() {
    let app = test_app().await;
    // sync REST channel — the exact shape v0.1.1 clients send.
    let digest = deploy::upload(
        &app,
        json!({"kind": "channel", "definition": {
            "name": "v01-style",
            "channel_type": "sync",
            "protocol": "rest",
            "methods": ["POST"],
            "route_pattern": "/v01-style",
        }}),
    )
    .await;
    let stored = read_definition(&app, &digest).await;
    // The stored form keeps the v0.1 lowercase shape.
    assert_eq!(stored["definition"]["channel_type"], "sync", "{stored}");
    assert_eq!(stored["definition"]["protocol"], "rest", "{stored}");
}

#[tokio::test]
async fn channel_type_and_protocol_are_case_insensitive() {
    let app = test_app().await;
    // Mixed casing — would have failed under v0.1's strict lowercase string
    // validation. Succeeds because A4's deserializer lowercases before
    // matching variants.
    let digest = deploy::upload(
        &app,
        json!({"kind": "channel", "definition": {
            "name": "mixed-case",
            "channel_type": "SYNC",
            "protocol": "Rest",
            "methods": ["POST"],
            "route_pattern": "/mixed-case",
        }}),
    )
    .await;
    // Canonical lowercase is what is stored, so clients that compare `==`
    // to "sync"/"rest" keep working.
    let stored = read_definition(&app, &digest).await;
    assert_eq!(stored["definition"]["channel_type"], "sync", "{stored}");
    assert_eq!(stored["definition"]["protocol"], "rest", "{stored}");
}

#[tokio::test]
async fn invalid_channel_protocol_is_rejected_at_deserialization() {
    let app = test_app().await;
    let (status, text) = upload_text(
        &app,
        json!({
            "name": "bad-protocol",
            "channel_type": "sync",
            "protocol": "grpc",
            "methods": ["POST"],
            "route_pattern": "/x",
        }),
    )
    .await;
    assert!(status.is_client_error(), "{status}: {text}");
    assert!(
        text.contains("rest") && text.contains("cron"),
        "the refusal should list allowed protocols, got {text}"
    );
}

#[tokio::test]
async fn invalid_channel_type_is_rejected_at_deserialization() {
    let app = test_app().await;
    let (status, text) = upload_text(
        &app,
        json!({
            "name": "bad-channel-type",
            "channel_type": "stream",
            "protocol": "rest",
            "methods": ["POST"],
            "route_pattern": "/x",
        }),
    )
    .await;
    assert!(status.is_client_error(), "{status}: {text}");
    assert!(
        text.contains("sync") && text.contains("async"),
        "the refusal should list allowed channel types, got {text}"
    );
}
