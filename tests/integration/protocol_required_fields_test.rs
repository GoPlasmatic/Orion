//! B1 (Tier-2 ergonomics): protocol-conditional required fields are
//! collected in one response with `REQUIRED_FOR_PROTOCOL` codes, so
//! authors fix everything in one round-trip instead of failing on the
//! first issue.


use axum::http::StatusCode;
use crate::common::{body_json, json_request, test_app};
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn rest_channel_missing_both_methods_and_route_pattern_collects_both_errors() {
    let app = test_app().await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "rest-missing-both",
                "channel_type": "sync",
                "protocol": "rest"
                // intentionally omit methods AND route_pattern
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    let details = body["error"]["details"].as_array().unwrap();
    let paths: Vec<&str> = details.iter().filter_map(|d| d["path"].as_str()).collect();
    assert!(
        paths.contains(&"channel.methods"),
        "expected channel.methods in details, got {paths:?}"
    );
    assert!(
        paths.contains(&"channel.route_pattern"),
        "expected channel.route_pattern in details, got {paths:?}"
    );
    // Both should carry the REQUIRED_FOR_PROTOCOL code.
    for d in details {
        if d["path"] == "channel.methods" || d["path"] == "channel.route_pattern" {
            assert_eq!(d["code"], "REQUIRED_FOR_PROTOCOL");
        }
    }
}

#[tokio::test]
async fn http_channel_missing_required_fields_reports_protocol_in_message() {
    let app = test_app().await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "http-missing",
                "channel_type": "sync",
                "protocol": "http"
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    let message = body["error"]["message"].as_str().unwrap_or("");
    // Top-level message names the protocol so the cause is obvious
    // without parsing details[].
    assert!(
        message.contains("http"),
        "message should name the protocol, got: {message}"
    );
}

#[tokio::test]
async fn field_error_carries_expected_hint_for_protocol_required_fields() {
    let app = test_app().await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "rest-hint",
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["POST"]
                // omit route_pattern
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    let details = body["error"]["details"].as_array().unwrap();
    let rp = details
        .iter()
        .find(|d| d["path"] == "channel.route_pattern")
        .expect("route_pattern detail missing");
    let expected = rp["expected"].as_str().unwrap_or("");
    assert!(
        expected.contains("URL path pattern"),
        "expected hint should describe the shape, got: {expected}"
    );
}
