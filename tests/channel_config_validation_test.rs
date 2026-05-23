//! B2 (Tier-2 ergonomics): strict per-channel config_json validation.
//!
//! Verifies that:
//!   - malformed channel.config (wrong shape) is rejected at CREATE with
//!     a field-pathed details entry
//!   - bad JSONLogic in validation_logic / rate_limit.key_logic is
//!     rejected at CREATE (instead of silently warning at engine reload)
//!   - well-formed channel.config still accepts

mod common;

use axum::http::StatusCode;
use common::{body_json, json_request, test_app};
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn malformed_config_shape_rejected_at_create() {
    let app = test_app().await;
    // `rate_limit` is supposed to be an object with requests_per_second; giving
    // it a number-shaped value should fail at deserialize.
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "bad-config-shape",
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["POST"],
                "route_pattern": "/x",
                "config": { "rate_limit": 42 }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    let details = body["error"]["details"].as_array().unwrap();
    assert!(
        details.iter().any(|d| d["path"] == "channel.config"),
        "expected channel.config path on details, got {body:?}"
    );
}

#[tokio::test]
async fn bad_jsonlogic_in_validation_logic_rejected() {
    // datalogic-rs::compile rejects multi-key objects where one key is
    // recognized as an op and the others aren't — a common shape mistake.
    // (Single unknown operators pass compile and only fail at eval; we
    // accept that limitation — see B2 scope notes in the commit.)
    let app = test_app().await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "bad-jsonlogic",
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["POST"],
                "route_pattern": "/y",
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
        "expected channel.config.validation_logic on details, got {body:?}"
    );
}

#[tokio::test]
async fn bad_jsonlogic_in_rate_limit_key_logic_rejected() {
    let app = test_app().await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "bad-key-logic",
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["POST"],
                "route_pattern": "/z",
                "config": {
                    "rate_limit": {
                        "requests_per_second": 10,
                        "key_logic": { "var": [], "extra_key": 1 }
                    }
                }
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
            .any(|d| d["path"] == "channel.config.rate_limit.key_logic"),
        "expected channel.config.rate_limit.key_logic on details, got {body:?}"
    );
}

#[tokio::test]
async fn well_formed_config_still_accepts() {
    let app = test_app().await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "happy",
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["POST"],
                "route_pattern": "/h",
                "config": {
                    "timeout_ms": 5000,
                    "validation_logic": { "!!": { "var": "data.id" } },
                    "rate_limit": {
                        "requests_per_second": 100,
                        "key_logic": { "var": "client_ip" }
                    }
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn empty_config_object_is_accepted() {
    // Documented default — `config: {}` must remain valid.
    let app = test_app().await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "empty-config",
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["POST"],
                "route_pattern": "/e",
                "config": {}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}
