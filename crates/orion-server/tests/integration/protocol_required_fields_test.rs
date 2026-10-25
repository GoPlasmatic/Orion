//! B1 (Tier-2 ergonomics): protocol-conditional required fields are
//! collected in one response with `REQUIRED_FOR_PROTOCOL` codes, so
//! authors fix everything in one round-trip instead of failing on the
//! first issue. A channel definition upload refuses them at their paths
//! under `definition`.

use crate::common::deploy;
use crate::common::test_app;
use axum::http::StatusCode;
use serde_json::json;

#[tokio::test]
async fn rest_channel_missing_both_methods_and_route_pattern_collects_both_errors() {
    let app = test_app().await;
    let (status, body) = deploy::try_upload(
        &app,
        json!({"kind": "channel", "definition": {
            "name": "rest-missing-both",
            "channel_type": "sync",
            "protocol": "rest"
            // intentionally omit methods AND route_pattern
        }}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let details = body["error"]["details"].as_array().unwrap();
    let paths: Vec<&str> = details.iter().filter_map(|d| d["path"].as_str()).collect();
    assert!(
        paths.contains(&"definition.methods"),
        "expected definition.methods in details, got {paths:?}"
    );
    assert!(
        paths.contains(&"definition.route_pattern"),
        "expected definition.route_pattern in details, got {paths:?}"
    );
    // Both should carry the REQUIRED_FOR_PROTOCOL code.
    for d in details {
        if d["path"] == "definition.methods" || d["path"] == "definition.route_pattern" {
            assert_eq!(d["code"], "REQUIRED_FOR_PROTOCOL");
        }
    }
}

#[tokio::test]
async fn http_channel_missing_required_fields_reports_protocol_in_message() {
    let app = test_app().await;
    let (status, body) = deploy::try_upload(
        &app,
        json!({"kind": "channel", "definition": {
            "name": "http-missing",
            "channel_type": "sync",
            "protocol": "http"
        }}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Each detail names the protocol so the cause is obvious without
    // reading the request back.
    let details = body["error"]["details"].as_array().unwrap();
    assert!(!details.is_empty(), "{body}");
    for d in details {
        let message = d["message"].as_str().unwrap_or("");
        assert!(
            message.contains("http"),
            "the detail should name the protocol, got: {message}"
        );
    }
}

#[tokio::test]
async fn field_error_carries_expected_hint_for_protocol_required_fields() {
    let app = test_app().await;
    let (status, body) = deploy::try_upload(
        &app,
        json!({"kind": "channel", "definition": {
            "name": "rest-hint",
            "channel_type": "sync",
            "protocol": "rest",
            "methods": ["POST"]
            // omit route_pattern
        }}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let details = body["error"]["details"].as_array().unwrap();
    let rp = details
        .iter()
        .find(|d| d["path"] == "definition.route_pattern")
        .expect("route_pattern detail missing");
    let expected = rp["expected"].as_str().unwrap_or("");
    assert!(
        expected.contains("URL path pattern"),
        "expected hint should describe the shape, got: {expected}"
    );
}
