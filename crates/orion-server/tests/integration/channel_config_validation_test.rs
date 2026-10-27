//! B2 + R3: strict per-channel validation, one table-driven rejection matrix
//! run against channel definition upload (`POST /api/v2/admin/definitions`).
//!
//!   - malformed config (wrong shape) → field-pathed details entry
//!   - bad JSONLogic in validation_logic / rate_limit.key_logic → refused at
//!     upload (instead of silently warning at load)
//!   - emptied route_pattern / methods → refused for the REST protocol
//!
//! Well-formed configs still upload.

use crate::common::{body_json, deploy, json_request, test_app};
use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

/// A valid REST channel definition named `name`, with `payload`'s keys laid
/// over it.
fn channel_with(name: &str, payload: &Value) -> Value {
    let mut definition = json!({
        "name": name,
        "channel_type": "sync",
        "protocol": "rest",
        "methods": ["POST"],
        "route_pattern": format!("/{name}"),
    });
    for (key, value) in payload.as_object().unwrap() {
        definition[key] = value.clone();
    }
    json!({"kind": "channel", "definition": definition})
}

/// Assert that `payload` (the channel fields under test) is refused at
/// upload with a details entry at `expected_path` (and `expected_code`, when
/// given).
async fn assert_channel_rejected(
    app: &axum::Router,
    name: &str,
    payload: &Value,
    expected_path: &str,
    expected_code: Option<&str>,
) {
    let (status, body) = deploy::try_upload(app, channel_with(name, payload)).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "{name}: expected 400: {body}"
    );
    let details = body["error"]["details"].as_array().unwrap();
    assert!(
        details
            .iter()
            .any(|d| d["path"] == expected_path
                && expected_code.is_none_or(|code| d["code"] == code)),
        "{name}: expected {expected_path} (code {expected_code:?}) in details, got {body:?}"
    );
}

#[tokio::test]
async fn rejection_matrix_on_upload() {
    // (case name, payload under test, expected details path, expected code)
    let cases: [(&str, Value, &str, Option<&str>); 14] = [
        // `rate_limit` must be an object with requests_per_second; a
        // number-shaped value fails at deserialize.
        (
            "bad-config-shape",
            json!({ "config": { "rate_limit": 42 } }),
            "definition.config",
            None,
        ),
        // datalogic-rs::compile rejects multi-key objects where one key is a
        // recognized op and the others aren't — a common shape mistake.
        // (Single unknown operators pass compile and only fail at eval; we
        // accept that limitation — see B2 scope notes.)
        (
            "bad-jsonlogic",
            json!({ "config": { "validation_logic": { "var": [], "extra_key": 1 } } }),
            "definition.config.validation_logic",
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
            "definition.config.rate_limit.key_logic",
            None,
        ),
        // G6: the quota block needs a principal, and a key naming it. Both
        // are refused at upload rather than shipping a channel that answers
        // 429 to everything because the key could never be computed.
        (
            "principal-rate-limit-without-key-logic",
            json!({ "config": {
                "auth": { "mode": "jwt", "jwt_keys": [{ "algorithm": "HS256", "key": "a-test-signing-secret-value" }],
                    "algorithms": ["HS256"] },
                "principal_rate_limit": { "requests_per_second": 10 }
            } }),
            "definition.config.principal_rate_limit.key_logic",
            Some("REQUIRED"),
        ),
        (
            "principal-rate-limit-without-jwt-auth",
            json!({ "config": {
                "auth": { "mode": "api_key", "keys": ["k"] },
                "principal_rate_limit": {
                    "requests_per_second": 10,
                    "key_logic": { "var": "auth.sub" }
                }
            } }),
            "definition.config.principal_rate_limit",
            None,
        ),
        (
            "empty-route",
            json!({ "route_pattern": "" }),
            "definition.route_pattern",
            Some("REQUIRED_FOR_PROTOCOL"),
        ),
        (
            "empty-methods",
            json!({ "methods": [] }),
            "definition.methods",
            None,
        ),
        // #275: `requests_per_second: 0` is refused, because the limiter
        // floors it to 1: asking for "admit nothing" would quietly get one per
        // second — a limit that reads as far stricter than it is.
        (
            "zero-rps",
            json!({ "config": {
                "rate_limit": { "requests_per_second": 0 }
            } }),
            "definition.config.rate_limit.requests_per_second",
            Some("INVALID"),
        ),
        // The remaining four are all the same failure: a `key_headers` entry
        // that cannot name a real header never populates the key context, so
        // `key_logic` resolves to `null` and every request is refused. The
        // channel would load clean and reject all its traffic.
        (
            "empty-key-headers",
            json!({ "config": {
                "rate_limit": { "requests_per_second": 10, "key_headers": [] }
            } }),
            "definition.config.rate_limit.key_headers",
            Some("INVALID"),
        ),
        (
            "blank-key-header",
            json!({ "config": {
                "rate_limit": { "requests_per_second": 10, "key_headers": ["  "] }
            } }),
            "definition.config.rate_limit.key_headers",
            Some("INVALID"),
        ),
        (
            "non-token-key-header",
            json!({ "config": {
                "rate_limit": { "requests_per_second": 10, "key_headers": ["device id"] }
            } }),
            "definition.config.rate_limit.key_headers",
            Some("INVALID"),
        ),
        // Header names are case-insensitive, so these are one name twice.
        // #278: `body_mode` needs no validation function — `deny_unknown_fields`
        // plus the enum reject both classes at deserialize, on all five admin
        // entry points at once. Unlike a `key_headers` typo, every wrong value
        // here is a hard parse error rather than an invisibly dead feature.
        (
            "bad-body-mode",
            json!({ "config": { "request": { "body_mode": "envelope" } } }),
            "definition.config",
            None,
        ),
        (
            "typod-request-key",
            json!({ "config": { "request": { "bodymode": "payload" } } }),
            "definition.config",
            None,
        ),
        (
            "duplicate-key-header",
            json!({ "config": {
                "rate_limit": {
                    "requests_per_second": 10,
                    "key_headers": ["deviceid", "DeviceId"]
                }
            } }),
            "definition.config.rate_limit.key_headers",
            Some("INVALID"),
        ),
    ];

    let app = test_app().await;
    for (name, payload, expected_path, expected_code) in &cases {
        assert_channel_rejected(&app, name, payload, expected_path, *expected_code).await;
    }
}

#[tokio::test]
async fn well_formed_config_still_accepts() {
    let app = test_app().await;
    deploy::upload(
        &app,
        channel_with(
            "happy",
            &json!({
                "route_pattern": "/h",
                "config": {
                    "timeout_ms": 5000,
                    "validation_logic": { "!!": { "var": "data.id" } },
                    "rate_limit": {
                        "requests_per_second": 100,
                        "key_logic": { "var": "client_ip" }
                    }
                }
            }),
        ),
    )
    .await;
}

/// #275: the shape the rejection matrix exists to make expressible — a
/// declared header, keyed on. The complement to those four rejection rows:
/// without this, over-strict validation would refuse the one config the
/// feature was added for and no test would notice.
#[tokio::test]
async fn a_declared_key_header_is_accepted_and_round_trips() {
    let app = test_app().await;
    let config = json!({
        "rate_limit": {
            "requests_per_second": 5,
            "key_headers": ["deviceid"],
            "key_logic": { "var": "headers.deviceid" }
        }
    });
    let digest = deploy::upload(
        &app,
        channel_with("per-device", &json!({ "config": config })),
    )
    .await;

    // The stored definition must come back with it: `key_headers` is not
    // dropped by a serde round-trip, and not masked.
    let resp = app
        .oneshot(json_request(
            "GET",
            &format!("/api/v2/admin/definitions/{digest}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["definition"]["config"]["rate_limit"]["key_headers"],
        json!(["deviceid"]),
        "key_headers must survive upload → read: {body}"
    );
}

#[tokio::test]
async fn empty_config_object_is_accepted() {
    // Documented default — `config: {}` must remain valid.
    let app = test_app().await;
    deploy::upload(
        &app,
        channel_with(
            "empty-config",
            &json!({"route_pattern": "/e", "config": {}}),
        ),
    )
    .await;
}
