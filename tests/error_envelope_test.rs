//! A3 (Tier-1 ergonomics): structured error envelope end-to-end tests.
//!
//! Verifies that:
//!   - field-pathed validation errors produce a `details` array
//!   - the v0.1.1 `code` + `message` envelope is preserved
//!   - `request_id` is embedded in error bodies when middleware is active
//!   - non-validation errors omit the `details` key (no breakage for old clients)

mod common;

use axum::http::StatusCode;
use common::{body_json, json_request, test_app};
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn create_channel_missing_route_pattern_returns_field_pathed_details() {
    let app = test_app().await;
    // REST channel without route_pattern — should fail with field details.
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "broken",
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["POST"],
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
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
    assert_eq!(details[0]["path"], "channel.route_pattern");
    assert_eq!(details[0]["code"], "REQUIRED");
}

#[tokio::test]
async fn create_channel_missing_topic_for_kafka_returns_field_details() {
    let app = test_app().await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "needs-topic",
                "channel_type": "async",
                "protocol": "kafka",
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    let details = body["error"]["details"]
        .as_array()
        .expect("details required");
    assert_eq!(details[0]["path"], "channel.topic");
    assert_eq!(details[0]["code"], "REQUIRED");
}

#[tokio::test]
async fn invalid_connector_type_emits_enum_mismatch_with_expected_got() {
    let app = test_app().await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "name": "bad",
                "connector_type": "grpc",
                "config": {}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    let detail = &body["error"]["details"][0];
    assert_eq!(detail["path"], "connector.connector_type");
    assert_eq!(detail["code"], "ENUM_MISMATCH");
    assert!(
        detail["expected"].is_array(),
        "expected[] must list allowed values"
    );
    assert_eq!(detail["got"], "grpc");
}

#[tokio::test]
async fn v01_envelope_preserved_for_non_validation_404() {
    let app = test_app().await;
    // GET an unknown workflow — produces NotFound, not Validation.
    // Must keep the v0.1 shape: code + message, no details key.
    let resp = app
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/workflows/does-not-exist",
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
        .uri("/api/v1/admin/channels")
        .header("content-type", "application/json")
        .header("x-request-id", "test-req-id-42")
        .body(axum::body::Body::from(
            serde_json::to_string(&json!({
                "name": "broken",
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["POST"],
            }))
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
