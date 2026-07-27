//! B2 + R3: strict per-channel validation, one table-driven rejection matrix
//! run against both verbs.
//!
//! CREATE (`POST /api/v1/admin/channels`) and UPDATE (`PUT .../{id}`) share
//! the same checks — updates are validated against the merged (stored draft ⊕
//! request) view, so every rejection case below must hold on both paths:
//!
//!   - malformed channel.config (wrong shape) → field-pathed details entry
//!   - bad JSONLogic in validation_logic / rate_limit.key_logic → rejected at
//!     write time (instead of silently warning at engine reload)
//!   - emptied route_pattern / methods → rejected for REST protocol
//!
//! Well-formed configs still accept on both verbs.

use crate::common::{body_json, json_request, test_app};
use axum::http::StatusCode;
use serde_json::{Value, json};
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

/// Assert that `payload` (the channel fields under test) is rejected with a
/// details entry at `expected_path` (and `expected_code`, when given).
///
/// `POST` merges the payload over a valid create body; `PUT` first creates a
/// valid draft and then applies the payload as the update.
async fn assert_channel_rejected(
    app: &axum::Router,
    method: &str,
    name: &str,
    payload: &Value,
    expected_path: &str,
    expected_code: Option<&str>,
) {
    let (uri, body) = match method {
        "POST" => {
            let mut body = json!({
                "name": name,
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["POST"],
                "route_pattern": format!("/{name}"),
            });
            for (key, value) in payload.as_object().unwrap() {
                body[key] = value.clone();
            }
            ("/api/v1/admin/channels".to_string(), body)
        }
        "PUT" => {
            let id = create_draft_channel(app, name).await;
            (format!("/api/v1/admin/channels/{id}"), payload.clone())
        }
        other => panic!("unsupported method {other}"),
    };

    let resp = app
        .clone()
        .oneshot(json_request(method, &uri, Some(body)))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "{method} {name}: expected 400"
    );
    let body = body_json(resp).await;
    let details = body["error"]["details"].as_array().unwrap();
    assert!(
        details
            .iter()
            .any(|d| d["path"] == expected_path
                && expected_code.is_none_or(|code| d["code"] == code)),
        "{method} {name}: expected {expected_path} (code {expected_code:?}) in details, got {body:?}"
    );
}

#[tokio::test]
async fn rejection_matrix_on_create_and_update() {
    // (case name, payload under test, expected details path, expected code)
    let cases: [(&str, Value, &str, Option<&str>); 5] = [
        // `rate_limit` must be an object with requests_per_second; a
        // number-shaped value fails at deserialize.
        (
            "bad-config-shape",
            json!({ "config": { "rate_limit": 42 } }),
            "channel.config",
            None,
        ),
        // datalogic-rs::compile rejects multi-key objects where one key is a
        // recognized op and the others aren't — a common shape mistake.
        // (Single unknown operators pass compile and only fail at eval; we
        // accept that limitation — see B2 scope notes.)
        (
            "bad-jsonlogic",
            json!({ "config": { "validation_logic": { "var": [], "extra_key": 1 } } }),
            "channel.config.validation_logic",
            None,
        ),
        (
            "bad-key-logic",
            json!({ "config": {
                "rate_limit": {
                    "requests_per_second": 10,
                    "key_logic": { "var": [], "extra_key": 1 }
                }
            } }),
            "channel.config.rate_limit.key_logic",
            None,
        ),
        (
            "empty-route",
            json!({ "route_pattern": "" }),
            "channel.route_pattern",
            Some("REQUIRED_FOR_PROTOCOL"),
        ),
        (
            "empty-methods",
            json!({ "methods": [] }),
            "channel.methods",
            None,
        ),
    ];

    let app = test_app().await;
    for (name, payload, expected_path, expected_code) in &cases {
        for method in ["POST", "PUT"] {
            let unique = format!("{name}-{}", method.to_lowercase());
            assert_channel_rejected(
                &app,
                method,
                &unique,
                payload,
                expected_path,
                *expected_code,
            )
            .await;
        }
    }
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
