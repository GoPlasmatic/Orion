//! #279: `server.data_mounts` — serving REST channels at additional paths.
//!
//! The data plane is hard-mounted at `/api/v1/data`. `route_pattern`s are
//! already multi-segment and unrestricted, so `"/zoom/meetings/user"` was a
//! legal pattern before this change — only the mount point was missing, which
//! forced a reverse proxy whose only job was to prepend the prefix.
//!
//! **Two of the tests here are security regressions, not features.** Under a
//! root mount `MatchedPath` becomes the literal `/{*path}`, which does not
//! start with `/api/v1/admin` — so admin auth would wave the request through
//! to the anonymous data plane, and the rate limiter would meter it against
//! the wrong budget.

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use crate::common::{self, body_json, json_request};

fn config_with_mounts(mounts: &[&str]) -> orion::config::AppConfig {
    let mut config = orion::config::AppConfig::default();
    config.server.data_mounts = mounts.iter().map(|m| m.to_string()).collect();
    config
}

/// A REST channel serving `route_pattern` — the legacy path a deployed client
/// calls.
async fn rest_channel(app: &axum::Router, name: &str, route_pattern: &str) {
    let wf_id = common::create_and_activate_workflow(app, common::echo_workflow("Mounted")).await;
    common::create_rest_channel(app, name, route_pattern, vec!["POST"], &wf_id).await;
}

/// The headline: the channel answers at the mounted path **and** still at the
/// canonical one. Additive, never a moved prefix.
#[tokio::test]
async fn a_mounted_channel_serves_both_paths() {
    let app = common::test_app_with_config(config_with_mounts(&["/zoom"])).await;
    rest_channel(&app, "zoom-meetings", "/zoom/meetings/user").await;

    for path in ["/zoom/meetings/user", "/api/v1/data/zoom/meetings/user"] {
        let resp = app
            .clone()
            .oneshot(json_request("POST", path, Some(json!({"data": {"a": 1}}))))
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
    }
}

/// `/async` is mount-independent — the suffix is stripped before routing.
#[tokio::test]
async fn a_mounted_channel_serves_async_too() {
    let app = common::test_app_with_config(config_with_mounts(&["/zoom"])).await;
    rest_channel(&app, "zoom-async", "/zoom/meetings/user").await;

    let resp = app
        .oneshot(json_request(
            "POST",
            "/zoom/meetings/user/async",
            Some(json!({"data": {"a": 1}})),
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

/// **Security regression 1.** With a root mount, `GET /api/v1/admin/nonexistent`
/// matches the catch-all, so `MatchedPath` is `/{*path}` — which is not an
/// admin path. Without the fix the request reaches the unauthenticated data
/// plane, and a channel could claim any unregistered path under
/// `/api/v1/admin/` and be served anonymously.
#[tokio::test]
async fn a_root_mount_does_not_disable_admin_auth() {
    let mut config = config_with_mounts(&["/"]);
    config.admin_auth.enabled = true;
    config.admin_auth.api_keys = vec!["a-sufficiently-long-test-secret-key-000".to_string()];
    let app = common::test_app_with_config(config).await;

    for path in [
        "/api/v1/admin/nonexistent",
        "/api/v1/admin/workflows/x/steal",
        "/api/v1/admin/workflows",
    ] {
        let resp = app
            .clone()
            .oneshot(json_request("GET", path, None))
            .await
            .expect("request");
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{path} must still be guarded under a root mount"
        );
    }
}

/// The carve-out survives: the single-trace GET authenticates itself with the
/// per-submission token, so it must **not** start demanding an admin key just
/// because the raw URI is consulted under a mount.
#[tokio::test]
async fn the_self_authenticating_trace_read_is_unaffected() {
    let mut config = config_with_mounts(&["/"]);
    config.admin_auth.enabled = true;
    config.admin_auth.api_keys = vec!["a-sufficiently-long-test-secret-key-000".to_string()];
    let app = common::test_app_with_config(config).await;

    let resp = app
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/traces/00000000-0000-0000-0000-000000000000",
            None,
        ))
        .await
        .expect("request");
    assert_ne!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "the single-trace read enforces its own two-lane rule, not the middleware's"
    );
}

/// **Security regression 2.** Root-mounted data traffic must classify as
/// `Data`, or it is metered against the default limiter instead of
/// `rate_limit.endpoints.data_rps`.
#[tokio::test]
async fn root_mounted_data_traffic_is_metered_as_data() {
    let mut config = config_with_mounts(&["/"]);
    config.rate_limit.enabled = true;
    // A data budget so small that a second call must be refused if — and only
    // if — the request classified as Data.
    config.rate_limit.endpoints.data_rps = Some(1);
    config.rate_limit.default_rps = 10_000;
    config.rate_limit.default_burst = 10_000;
    let app = common::test_app_with_config(config).await;
    rest_channel(&app, "legacy", "/Legacy-App/api/public/ping").await;

    let mut refused = false;
    for _ in 0..8 {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/Legacy-App/api/public/ping",
                Some(json!({"data": {}})),
            ))
            .await
            .expect("request");
        if resp.status() == StatusCode::TOO_MANY_REQUESTS {
            refused = true;
            break;
        }
    }
    assert!(
        refused,
        "root-mounted data traffic must be metered against the data budget"
    );
}

/// Platform routes keep their own handlers under a root mount — matchit
/// prefers a static route over a catch-all regardless of registration order.
#[tokio::test]
async fn platform_routes_win_over_a_root_mount() {
    let app = common::test_app_with_config(config_with_mounts(&["/"])).await;

    for path in ["/health", "/healthz", "/readyz"] {
        let resp = app
            .clone()
            .oneshot(json_request("GET", path, None))
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
        // A channel lookup would answer the error envelope, not this shape.
        let body = body_json(resp).await;
        assert!(
            body.get("status").is_some(),
            "{path} must be answered by its own handler: {body}"
        );
    }

    // `/{*path}` does not match the root path itself.
    let resp = app
        .oneshot(json_request("GET", "/", None))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// A channel whose served path would land under a platform route is refused at
/// activation — the R7 rule that the gate and the table must not drift.
#[tokio::test]
async fn a_channel_shadowed_by_a_platform_route_is_refused_at_activation() {
    let app = common::test_app_with_config(config_with_mounts(&["/"])).await;
    let wf_id = common::create_and_activate_workflow(&app, common::echo_workflow("Shadow")).await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "shadowed",
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["POST"],
                "route_pattern": "/api/v1/admin/anything",
                "workflow_id": wf_id,
            })),
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let channel_id = body_json(resp).await["data"]["channel_id"]
        .as_str()
        .expect("id")
        .to_string();

    let resp = app
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{channel_id}/status"),
            Some(json!({ "status": "active" })),
        ))
        .await
        .expect("request");
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "a channel the platform would shadow must not activate"
    );
    let body = body_json(resp).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("platform route")),
        "{body}"
    );
}

/// With no mounts configured — the default — nothing changes.
#[tokio::test]
async fn no_mounts_is_todays_behaviour() {
    let app = common::test_app().await;
    rest_channel(&app, "plain", "/orders/{id}").await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/orders/7",
            Some(json!({"data": {}})),
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);

    // The unmounted path is a plain 404 from the fallback.
    let resp = app
        .oneshot(json_request("POST", "/orders/7", Some(json!({"data": {}}))))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
