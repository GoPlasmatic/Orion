//! #269: `config.response.error_bodies` — per-channel shaping of ingress
//! guard-rejection bodies.
//!
//! The platform decides the **status**; the channel decides the **bytes**.
//! Two properties matter more than the feature itself and are asserted here:
//! error-owned headers survive the swap, and a channel without the block is
//! byte-identical to before.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use crate::common::{self, body_json, json_request};

/// A channel with an `api_key` auth guard and a shaped 401.
async fn shaped_auth_channel(app: &axum::Router, name: &str, error_bodies: serde_json::Value) {
    common::create_and_activate_channel_with_config(
        app,
        name,
        common::simple_log_workflow(&format!("{name} WF")),
        json!({
            "auth": { "mode": "api_key", "keys": ["s3cret-key-value-long-enough"] },
            "response": { "error_bodies": error_bodies }
        }),
    )
    .await;
}

async fn post_unauthenticated(app: &axum::Router, channel: &str) -> axum::http::Response<Body> {
    app.clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/data/{channel}"),
            Some(json!({"data": {}})),
        ))
        .await
        .expect("request")
}

/// The motivating case: deployed clients that parse a legacy 401 shape.
#[tokio::test]
async fn a_shaped_401_replaces_the_platform_envelope() {
    let app = common::test_app().await;
    shaped_auth_channel(
        &app,
        "legacy-401",
        json!({
            "401": { "body": r#"{"status":{status},"error":"SESSION_EXPIRED","message":"{message}"}"# }
        }),
    )
    .await;

    let resp = post_unauthenticated(&app, "legacy-401").await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "the platform still owns the status"
    );
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    let body = body_json(resp).await;
    assert_eq!(body["status"], 401);
    assert_eq!(body["error"], "SESSION_EXPIRED");
    assert!(
        body.get("error").is_some() && body["error"].is_string(),
        "the platform envelope's nested error object is gone: {body}"
    );
}

/// **The regression that matters most.** A channel without `error_bodies` must
/// answer exactly as it did before — same status, same envelope shape, same
/// keys.
#[tokio::test]
async fn a_channel_without_error_bodies_is_unchanged() {
    let app = common::test_app().await;
    common::create_and_activate_channel_with_config(
        &app,
        "plain-401",
        common::simple_log_workflow("Plain WF"),
        json!({ "auth": { "mode": "api_key", "keys": ["s3cret-key-value-long-enough"] } }),
    )
    .await;

    let resp = post_unauthenticated(&app, "plain-401").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "UNAUTHORIZED");
    assert!(
        body["error"]["message"].is_string(),
        "the platform envelope is intact: {body}"
    );
}

/// **`WWW-Authenticate` must survive the body swap.** The response is built by
/// the error and then edited; rendering a fresh one would drop the RFC 6750
/// challenge silently.
#[tokio::test]
async fn error_owned_headers_survive_shaping() {
    let app = common::test_app().await;
    common::create_and_activate_channel_with_config(
        &app,
        "jwt-shaped",
        common::simple_log_workflow("JWT WF"),
        json!({
            "auth": {
                "mode": "jwt",
                "algorithms": ["HS256"],
                "jwt_keys": [{
                    "algorithm": "HS256",
                    "key": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                }]
            },
            "response": { "error_bodies": { "401": { "body": r#"{"e":"{code}"}"# } } }
        }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/data/jwt-shaped")
                .header("content-type", "application/json")
                .header("authorization", "Bearer not-a-real-token")
                .body(Body::from(r#"{"data":{}}"#))
                .unwrap(),
        )
        .await
        .expect("request");

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(
        resp.headers().contains_key("www-authenticate"),
        "a refused bearer token must still carry its challenge: {:?}",
        resp.headers()
    );
    let body = body_json(resp).await;
    assert_eq!(body["e"], "UNAUTHORIZED", "and the body is still shaped");
}

/// `retry-after` is the other error-owned header, on the 429 path.
#[tokio::test]
async fn a_shaped_429_keeps_retry_after() {
    let app = common::test_app().await;
    common::create_and_activate_channel_with_config(
        &app,
        "throttled",
        common::simple_log_workflow("Throttle WF"),
        json!({
            "rate_limit": { "requests_per_second": 1, "burst": 1 },
            "response": { "error_bodies": {
                "429": { "body": r#"{"errorCode":"{status}","message":"{message}"}"# }
            } }
        }),
    )
    .await;

    // Spend the bucket, then trip it.
    let mut refused = None;
    for _ in 0..6 {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/data/throttled",
                Some(json!({"data": {}})),
            ))
            .await
            .expect("request");
        if resp.status() == StatusCode::TOO_MANY_REQUESTS {
            refused = Some(resp);
            break;
        }
    }
    let resp = refused.expect("the limiter must refuse within six calls");
    assert!(
        resp.headers().contains_key("retry-after"),
        "a shaped 429 must still carry retry-after: {:?}",
        resp.headers()
    );
    let body = body_json(resp).await;
    assert_eq!(body["errorCode"], "429");
}

/// `"default"` catches every status the map does not name explicitly.
#[tokio::test]
async fn the_default_key_catches_unnamed_statuses() {
    let app = common::test_app().await;
    shaped_auth_channel(
        &app,
        "defaulted",
        json!({ "default": { "body": r#"{"code":"{code}","st":{status}}"# } }),
    )
    .await;

    let resp = post_unauthenticated(&app, "defaulted").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert_eq!(body["st"], 401);
    assert_eq!(body["code"], "UNAUTHORIZED");
}

/// A non-JSON content type is honoured, for clients that parse something else.
#[tokio::test]
async fn a_text_content_type_is_applied() {
    let app = common::test_app().await;
    shaped_auth_channel(
        &app,
        "text-401",
        json!({
            "401": { "body": "denied: {code}", "content_type": "text/plain" }
        }),
    )
    .await;

    let resp = post_unauthenticated(&app, "text-401").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/plain")
    );
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    assert_eq!(String::from_utf8_lossy(&bytes), "denied: UNAUTHORIZED");
}

/// Authoring-time refusals. The templates that cannot work are refused at the
/// door rather than falling back silently at request time.
#[tokio::test]
async fn unusable_templates_are_refused_at_create() {
    let app = common::test_app().await;
    for (name, bodies) in [
        // Unknown placeholder — a misspelling must not ship as a literal.
        ("unknown-ph", json!({ "401": { "body": "{mesage}" } })),
        // `details` is deliberately outside the closed set.
        ("details-ph", json!({ "401": { "body": "{details}" } })),
        // Not a status, and not an error status.
        ("bad-key", json!({ "nope": { "body": "{code}" } })),
        ("ok-status", json!({ "200": { "body": "{code}" } })),
        // JSON content type that does not render as JSON.
        ("not-json", json!({ "401": { "body": "plain text" } })),
    ] {
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
                    "route_pattern": format!("/{name}"),
                    "config": { "response": { "error_bodies": bodies } }
                })),
            ))
            .await
            .expect("request");
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "{name} must be refused"
        );
        let body = body_json(resp).await;
        let details = body["error"]["details"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(
            details.iter().any(|d| d["path"]
                .as_str()
                .is_some_and(|p| p.starts_with("channel.config.response.error_bodies."))),
            "{name}: expected a field-pathed detail, got {body}"
        );
    }
}

/// `error_bodies` is independent of `mode` — an envelope channel may use it,
/// which is the point: the two settings answer different questions.
#[tokio::test]
async fn error_bodies_work_under_both_response_modes() {
    let app = common::test_app().await;
    for (name, mode) in [("mode-env", "envelope"), ("mode-shaped", "shaped")] {
        common::create_and_activate_channel_with_config(
            &app,
            name,
            common::simple_log_workflow(&format!("{name} WF")),
            json!({
                "auth": { "mode": "api_key", "keys": ["s3cret-key-value-long-enough"] },
                "response": {
                    "mode": mode,
                    "error_bodies": { "401": { "body": r#"{"m":"{code}"}"# } }
                }
            }),
        )
        .await;

        let resp = post_unauthenticated(&app, name).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = body_json(resp).await;
        assert_eq!(body["m"], "UNAUTHORIZED", "{name} ({mode}): {body}");
    }
}
