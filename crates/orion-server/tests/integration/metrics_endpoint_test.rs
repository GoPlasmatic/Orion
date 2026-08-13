//! Where `/metrics` is served, and where it is not (proposal O12).
//!
//! Two defects, both about registration rather than rendering:
//!
//! * `metrics.enabled = false` still registered the route, so `/metrics`
//!   answered `200` with an empty body rendered from an orphan recorder — a
//!   scrape target that looked healthy and reported nothing, forever.
//! * There was no way to serve it anywhere but the main listener, so with
//!   `admin_auth` on, every scraper had to hold a credential that can also
//!   rewrite workflows and read trace payloads.

use std::time::Duration;

use axum::http::StatusCode;
use tower::ServiceExt;

use crate::common;
use crate::common::{body_json, json_request};

fn metrics_config(bind_addr: Option<&str>) -> orion::config::AppConfig {
    let mut config = orion::config::AppConfig::default();
    config.metrics.enabled = true;
    config.metrics.bind_addr = bind_addr.map(str::to_string);
    config
}

// ============================================================
// Registration is conditional
// ============================================================

/// With collection off the path must 404 like any other unknown route —
/// including the JSON error envelope, so a misconfigured scrape is legible.
#[tokio::test]
async fn metrics_route_is_absent_when_disabled() {
    let config = orion::config::AppConfig::default();
    assert!(!config.metrics.enabled, "the default is off");
    let app = common::test_app_with_config(config).await;

    let resp = app
        .oneshot(json_request("GET", "/metrics", None))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "metrics.enabled = false must not leave a 200-with-empty-body endpoint behind"
    );
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "NOT_FOUND");
}

/// Enabled and with no dedicated listener: the endpoint stays where it always
/// was.
#[tokio::test]
async fn metrics_route_is_present_on_the_main_listener_by_default() {
    let app = common::test_app_with_config(metrics_config(None)).await;
    let resp = app
        .oneshot(json_request("GET", "/metrics", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// A dedicated listener *moves* the endpoint — it does not duplicate it. The
/// whole point is that the main listener stops serving it, so a scraper
/// pointed at the public address cannot keep working by accident.
#[tokio::test]
async fn bind_addr_removes_metrics_from_the_main_listener() {
    let config = metrics_config(Some("127.0.0.1:0"));
    let app = common::test_app_with_config(config).await;
    let resp = app
        .oneshot(json_request("GET", "/metrics", None))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "with metrics.bind_addr set, the main listener must not serve /metrics"
    );
}

// ============================================================
// ...including with admin auth on
// ============================================================

const ADMIN_KEY: &str = "a-very-long-admin-key-for-testing-0123";

fn with_admin_auth(mut config: orion::config::AppConfig) -> orion::config::AppConfig {
    config.admin_auth.enabled = true;
    config.admin_auth.api_keys = vec![ADMIN_KEY.to_string()];
    config
}

fn authed(uri: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .body(axum::body::Body::empty())
        .expect("request")
}

/// The guard is keyed on the path, and the path is unregistered — so it has
/// to answer `404`, not `401`.
///
/// This is not pedantry about a status code. `admin_auth_middleware` is
/// applied with `Router::layer`, so it wraps the 404 fallback too, and an
/// unregistered route produces no `MatchedPath` — the middleware falls back to
/// the raw URI. A guard that names `/metrics` unconditionally therefore
/// challenges for a credential on a path that does not exist, contradicting
/// every artifact that documents this (`404s` in the OpenAPI description, the
/// observability page and the configuration reference) and advertising the
/// endpoint's existence to an anonymous caller. `/docs` (S17) already gets
/// this right; `/metrics` must match.
#[tokio::test]
async fn disabled_metrics_404s_rather_than_401s_with_admin_auth_on() {
    let config = with_admin_auth(orion::config::AppConfig::default());
    assert!(!config.metrics.enabled, "the default is off");
    let app = common::test_app_with_config(config).await;

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/metrics", None))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "an unregistered /metrics must not answer 401 and advertise itself"
    );
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "NOT_FOUND");

    // A valid credential does not conjure the route either.
    let resp = app.oneshot(authed("/metrics")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Same rule when `metrics.bind_addr` has moved the endpoint: it is not on
/// this listener, so this listener 404s it.
#[tokio::test]
async fn bind_addr_404s_on_the_main_listener_with_admin_auth_on() {
    let app =
        common::test_app_with_config(with_admin_auth(metrics_config(Some("127.0.0.1:0")))).await;

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/metrics", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "NOT_FOUND");

    let resp = app.oneshot(authed("/metrics")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// The other direction, so narrowing the guard cannot quietly open the
/// endpoint: registered on the main listener, it is behind the admin
/// credential exactly as before.
#[tokio::test]
async fn metrics_on_the_main_listener_stays_behind_the_admin_credential() {
    let app = common::test_app_with_config(with_admin_auth(metrics_config(None))).await;

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/metrics", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = app.oneshot(authed("/metrics")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// Narrowing the `/metrics` arm must not have touched the admin plane: it is
/// guarded whether or not this listener serves metrics, including on paths
/// that do not exist (an unknown admin path is a 401, not a 404 — the admin
/// surface is guarded by prefix, so probing it reveals nothing).
#[tokio::test]
async fn the_admin_plane_is_guarded_independently_of_the_metrics_listener() {
    for bind_addr in [None, Some("127.0.0.1:0")] {
        let app = common::test_app_with_config(with_admin_auth(metrics_config(bind_addr))).await;
        let resp = app
            .clone()
            .oneshot(json_request("GET", "/api/v1/admin/workflows", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{bind_addr:?}");

        let resp = app
            .oneshot(authed("/api/v1/admin/workflows"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{bind_addr:?}");
    }
}

// ============================================================
// The dedicated listener
// ============================================================

/// The credential story O12 is about: admin auth is on, the scraper holds
/// nothing, and the scrape still succeeds — because it is talking to the
/// metrics listener, not the admin plane.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dedicated_listener_serves_metrics_without_an_admin_key() {
    let mut config = metrics_config(Some("127.0.0.1:0"));
    config.admin_auth.enabled = true;
    config.admin_auth.api_keys = vec!["a-very-long-admin-key-for-testing-0123".to_string()];
    config.server.shutdown_drain_secs = 0;
    let state = common::test_state_with_config(config).await;
    let cfg = state.config.clone();

    let listener = orion::server::serve::create_tcp_listener("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(orion::server::serve::serve_metrics(
        listener,
        cfg,
        orion::server::metrics_router(state.clone()),
        async move {
            let _ = shutdown_rx.await;
        },
    ));

    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .expect("client");

    let resp = client
        .get(format!("http://{addr}/metrics"))
        .send()
        .await
        .expect("scrape without a credential");
    assert_eq!(
        resp.status(),
        200,
        "the metrics listener must not require an admin key"
    );
    assert!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/plain")),
        "Prometheus exposition is text/plain"
    );

    // ...and it serves nothing else. The admin plane is not reachable here,
    // credential or no credential.
    let resp = client
        .get(format!("http://{addr}/api/v1/admin/workflows"))
        .send()
        .await
        .expect("admin probe");
    assert_eq!(
        resp.status(),
        404,
        "the metrics listener must expose only GET /metrics"
    );

    // It joins the existing shutdown path rather than running until abort.
    shutdown_tx.send(()).expect("send shutdown");
    let result = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("the metrics listener must stop on the shutdown signal");
    assert!(result.expect("join").is_ok());
}
