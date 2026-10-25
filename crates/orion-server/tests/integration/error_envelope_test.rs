//! A3 (Tier-1 ergonomics): structured error envelope end-to-end tests.
//!
//! Verifies that:
//!   - field-pathed validation errors produce a `details` array
//!   - the v0.1.1 `code` + `message` envelope is preserved
//!   - `request_id` is embedded in error bodies when middleware is active
//!   - non-validation errors omit the `details` key (no breakage for old clients)

use crate::common::deploy;
use crate::common::{body_json, json_request, test_app};
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn upload_channel_missing_route_pattern_returns_field_pathed_details() {
    let app = test_app().await;
    // REST channel without route_pattern — refused with field details.
    let (status, body) = deploy::try_upload(
        &app,
        json!({"kind": "channel", "definition": {
            "name": "broken",
            "channel_type": "sync",
            "protocol": "rest",
            "methods": ["POST"],
        }}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let error = &body["error"];

    // v0.1.1 envelope preserved.
    assert!(error["code"].is_string(), "code field must be present");
    assert!(
        error["message"].is_string(),
        "message field must be present"
    );

    // New: structured details with field path.
    let details = error["details"]
        .as_array()
        .expect("details must be a JSON array on a field-validation error");
    assert!(!details.is_empty());
    // B1: protocol-conditional required fields use REQUIRED_FOR_PROTOCOL
    // so clients can distinguish them from generic missing-field errors.
    assert!(
        details.iter().any(
            |d| d["path"] == "definition.route_pattern" && d["code"] == "REQUIRED_FOR_PROTOCOL"
        ),
        "expected definition.route_pattern with REQUIRED_FOR_PROTOCOL, got {body:?}"
    );
}

/// Kafka left Community: `protocol: "kafka"` is refused like any protocol no
/// channel driver on this node serves — by field, naming the ones it does.
#[tokio::test]
async fn upload_channel_with_the_kafka_protocol_is_refused_as_unknown() {
    let app = test_app().await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v2/admin/definitions",
            Some(json!({"kind": "channel", "definition": {
                "name": "needs-topic",
                "channel_type": "async",
                "protocol": "kafka",
            }})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    let details = body["error"]["details"].as_array().expect("details");
    assert_eq!(details.len(), 1, "{body}");
    assert_eq!(details[0]["path"], "definition.protocol", "{body}");
    assert_eq!(details[0]["code"], "INVALID", "{body}");
    assert_eq!(details[0]["got"], "kafka", "{body}");
    assert_eq!(
        details[0]["expected"],
        json!(["cron", "http", "rest"]),
        "{body}"
    );
}

#[tokio::test]
async fn v01_envelope_preserved_for_non_validation_404() {
    let app = test_app().await;
    // GET an unknown definition — produces NotFound, not Validation.
    // Must keep the v0.1 shape: code + message, no details key.
    let resp = app
        .oneshot(json_request(
            "GET",
            "/api/v2/admin/definitions/sha256:0000000000000000000000000000000000000000000000000000000000000000",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    let error = &body["error"];
    assert_eq!(error["code"], "NOT_FOUND");
    assert!(error["message"].is_string());
    assert!(
        error.get("details").is_none(),
        "non-validation errors must omit details (v0.1 compat)"
    );
}

#[tokio::test]
async fn error_response_embeds_request_id_when_header_provided() {
    let app = test_app().await;
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v2/admin/definitions")
        .header("content-type", "application/json")
        .header("x-request-id", "test-req-id-42")
        .body(axum::body::Body::from(
            serde_json::to_string(&json!({"kind": "channel", "definition": {
                "name": "broken",
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["POST"],
            }}))
            .unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // The same id should appear in the response header (Propagate) and the body.
    let header_id = resp
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let body = body_json(resp).await;
    assert_eq!(body["error"]["request_id"], "test-req-id-42");
    assert_eq!(header_id.as_deref(), Some("test-req-id-42"));
}
