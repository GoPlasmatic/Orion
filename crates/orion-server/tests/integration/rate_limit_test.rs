use crate::common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use orion::config::{AppConfig, EndpointRateLimits, RateLimitConfig};

/// A one-task workflow that logs `message`.
fn log_workflow(name: &str, message: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "condition": true,
        "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": message}}}]
    })
}

/// Create a test app with rate limiting enabled (very low limits for testing).
async fn rate_limited_app(rps: u32, burst: u32) -> Router {
    common::test_app_with_config(AppConfig {
        rate_limit: RateLimitConfig {
            enabled: true,
            default_rps: rps,
            default_burst: burst,
            ..Default::default()
        },
        ..Default::default()
    })
    .await
}

#[tokio::test]
async fn test_rate_limit_allows_within_limit() {
    let app = rate_limited_app(100, 50).await;

    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .header("x-forwarded-for", "10.0.0.1")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_rate_limit_returns_429_when_exceeded() {
    // Use burst=1 so second request is rejected
    let app = rate_limited_app(1, 1).await;

    // First request should succeed
    let req1 = Request::builder()
        .method("GET")
        .uri("/health")
        .header("x-forwarded-for", "10.0.0.99")
        .body(Body::empty())
        .unwrap();
    let resp1 = app.clone().oneshot(req1).await.unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);

    // Second request from same IP should be rate limited
    let req2 = Request::builder()
        .method("GET")
        .uri("/health")
        .header("x-forwarded-for", "10.0.0.99")
        .body(Body::empty())
        .unwrap();
    let resp2 = app.oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn test_rate_limit_429_response_body() {
    let app = rate_limited_app(1, 1).await;

    // Exhaust the limit
    let req1 = Request::builder()
        .method("GET")
        .uri("/health")
        .header("x-forwarded-for", "10.0.0.100")
        .body(Body::empty())
        .unwrap();
    let _ = app.clone().oneshot(req1).await.unwrap();

    // Get the 429 response
    let req2 = Request::builder()
        .method("GET")
        .uri("/health")
        .header("x-forwarded-for", "10.0.0.100")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req2).await.unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(resp.headers().get("retry-after").unwrap(), "1");

    let body = common::body_json(resp).await;
    assert_eq!(body["error"]["code"], "RATE_LIMITED");
    assert_eq!(body["error"]["message"], "Too many requests");
}

#[tokio::test]
async fn test_rate_limit_different_ips_independent() {
    let app = rate_limited_app(1, 1).await;

    // Request from IP A
    let req1 = Request::builder()
        .method("GET")
        .uri("/health")
        .header("x-forwarded-for", "192.168.1.1")
        .body(Body::empty())
        .unwrap();
    let resp1 = app.clone().oneshot(req1).await.unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);

    // Request from IP B should also succeed (independent limit)
    let req2 = Request::builder()
        .method("GET")
        .uri("/health")
        .header("x-forwarded-for", "192.168.1.2")
        .body(Body::empty())
        .unwrap();
    let resp2 = app.oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_rate_limit_admin_endpoint() {
    // Admin routes are governed by `endpoints.admin_rps`, which now defaults to
    // a real value (S12) instead of falling back to `default_rps`. Set it
    // explicitly so this exercises the admin limiter rather than the default one.
    let app = common::test_app_with_config(AppConfig {
        rate_limit: RateLimitConfig {
            enabled: true,
            default_rps: 100,
            default_burst: 50,
            endpoints: orion::config::EndpointRateLimits {
                admin_rps: Some(1),
                data_rps: None,
            },
            ..Default::default()
        },
        ..Default::default()
    })
    .await;

    let req = Request::builder()
        .method("GET")
        .uri("/api/v2/admin/deployments")
        .header("x-forwarded-for", "10.0.0.200")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Second should be rate limited
    let req2 = Request::builder()
        .method("GET")
        .uri("/api/v2/admin/deployments")
        .header("x-forwarded-for", "10.0.0.200")
        .body(Body::empty())
        .unwrap();
    let resp2 = app.oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn test_rate_limit_channel_specific_from_db() {
    // Use the standard test_app which has rate limiting disabled at platform level,
    // but we need rate limiting enabled. Build a custom app with rate limiting ON
    // and high default limits, then create a channel with tight DB-driven limits.
    let app = common::test_app_with_config(AppConfig {
        rate_limit: RateLimitConfig {
            enabled: true,
            default_rps: 1000,
            default_burst: 500,
            endpoints: EndpointRateLimits {
                admin_rps: Some(1000),
                data_rps: Some(1000),
            },
            ..Default::default()
        },
        ..Default::default()
    })
    .await;

    // A channel with a per-channel rate limit in its config (1 rps, burst 1).
    // Deploying it rebuilds the channel estate with the limiter.
    common::create_and_activate_channel_with_config(
        &app,
        "rate-limited-orders",
        log_workflow("rate-limit-test-wf", "test"),
        serde_json::json!({
            "rate_limit": {
                "requests_per_second": 1,
                "burst": 1,
                "key_logic": { "var": "client_ip" }
            }
        }),
    )
    .await;

    // First request to channel — should succeed (not 429)
    let req1 = Request::builder()
        .method("POST")
        .uri("/api/v2/data/rate-limited-orders")
        .header("x-forwarded-for", "10.0.0.50")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"data":{"test":true}}"#))
        .unwrap();
    let resp1 = app.clone().oneshot(req1).await.unwrap();
    assert_ne!(resp1.status(), StatusCode::TOO_MANY_REQUESTS);

    // Second request from same IP to same channel — should be rate limited
    let req2 = Request::builder()
        .method("POST")
        .uri("/api/v2/data/rate-limited-orders")
        .header("x-forwarded-for", "10.0.0.50")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"data":{"test":true}}"#))
        .unwrap();
    let resp2 = app.clone().oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::TOO_MANY_REQUESTS);
}

/// N6: a redeploy must not reset per-channel limiter state. Every deployment
/// rebuilds the serving generation, so a fresh limiter per rebuild would let a
/// caller bypass a channel's limit by causing (or waiting for) one — here, a
/// deployment that adds something else and leaves the channel unchanged.
#[tokio::test]
async fn test_channel_rate_limit_survives_a_redeploy() {
    let app = common::test_app_with_config(AppConfig {
        rate_limit: RateLimitConfig {
            enabled: true,
            default_rps: 1000,
            default_burst: 500,
            endpoints: EndpointRateLimits {
                admin_rps: Some(1000),
                data_rps: Some(1000),
            },
            ..Default::default()
        },
        ..Default::default()
    })
    .await;

    common::create_and_activate_channel_with_config(
        &app,
        "reload-rl-ch",
        log_workflow("reload-rl-wf", "test"),
        serde_json::json!({
            "rate_limit": { "requests_per_second": 1, "burst": 1 }
        }),
    )
    .await;

    let data_req = || {
        Request::builder()
            .method("POST")
            .uri("/api/v2/data/reload-rl-ch")
            .header("x-forwarded-for", "10.0.0.60")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"data":{"test":true}}"#))
            .unwrap()
    };

    // Consume the burst: first passes, second is limited.
    let resp = app.clone().oneshot(data_req()).await.unwrap();
    assert_ne!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    let resp = app.clone().oneshot(data_req()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

    // Redeploy with the channel unchanged: a new workflow joins the
    // manifest, and the node rebuilds its generation.
    common::create_and_activate_workflow(&app, log_workflow("reload-rl-other", "other")).await;

    // The consumed burst must still be consumed.
    let resp = app.clone().oneshot(data_req()).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "a redeploy must not refill a channel's rate-limit burst"
    );
}

#[tokio::test]
async fn test_no_rate_limit_when_disabled() {
    // Use the standard test_app which has rate_limit_state: None
    let app = common::test_app().await;

    for _ in 0..5 {
        let req = common::json_request("GET", "/health", None);
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn test_rate_limit_custom_key_logic_by_header() {
    // Rate limit keyed by x-api-key header — different keys get independent limits
    let app = common::test_app_with_config(AppConfig {
        rate_limit: RateLimitConfig {
            enabled: true,
            default_rps: 1000,
            default_burst: 500,
            endpoints: EndpointRateLimits {
                admin_rps: Some(1000),
                data_rps: Some(1000),
            },
            ..Default::default()
        },
        ..Default::default()
    })
    .await;

    // A channel with key_logic keyed by the x-api-key header.
    common::create_and_activate_channel_with_config(
        &app,
        "api-key-limited",
        log_workflow("key-logic-test-wf", "test"),
        serde_json::json!({
            "rate_limit": {
                "requests_per_second": 1,
                "burst": 1,
                "key_logic": { "var": "headers.x-api-key" }
            }
        }),
    )
    .await;

    // Request with API key "key-A" — should succeed
    let req1 = Request::builder()
        .method("POST")
        .uri("/api/v2/data/api-key-limited")
        .header("x-forwarded-for", "10.0.0.1")
        .header("x-api-key", "key-A")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"data":{"test":true}}"#))
        .unwrap();
    let resp1 = app.clone().oneshot(req1).await.unwrap();
    assert_ne!(resp1.status(), StatusCode::TOO_MANY_REQUESTS);

    // Second request with same API key "key-A" — should be rate limited
    let req2 = Request::builder()
        .method("POST")
        .uri("/api/v2/data/api-key-limited")
        .header("x-forwarded-for", "10.0.0.1")
        .header("x-api-key", "key-A")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"data":{"test":true}}"#))
        .unwrap();
    let resp2 = app.clone().oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::TOO_MANY_REQUESTS);

    // Request with different API key "key-B" from same IP — should succeed (independent limit)
    let req3 = Request::builder()
        .method("POST")
        .uri("/api/v2/data/api-key-limited")
        .header("x-forwarded-for", "10.0.0.1")
        .header("x-api-key", "key-B")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"data":{"test":true}}"#))
        .unwrap();
    let resp3 = app.clone().oneshot(req3).await.unwrap();
    assert_ne!(resp3.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn test_rate_limit_uses_x_real_ip_fallback() {
    let app = rate_limited_app(1, 1).await;

    let req1 = Request::builder()
        .method("GET")
        .uri("/health")
        .header("x-real-ip", "172.16.0.1")
        .body(Body::empty())
        .unwrap();
    let resp1 = app.clone().oneshot(req1).await.unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);

    let req2 = Request::builder()
        .method("GET")
        .uri("/health")
        .header("x-real-ip", "172.16.0.1")
        .body(Body::empty())
        .unwrap();
    let resp2 = app.oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::TOO_MANY_REQUESTS);
}

/// S9: a REST-routed channel's own rate limit must actually apply.
///
/// Looking the first path segment up as a channel name would, for
/// `GET /orders/{id}` on a channel named `s9-orders-get`, ask for a channel
/// called `orders`, find nothing, and fall through to the platform limiter —
/// the configured limit silently never applying. The platform limits here are
/// deliberately huge, so the only thing
/// that can produce a 429 is the channel's own limiter.
#[tokio::test]
async fn test_rate_limit_applies_to_rest_routed_channel() {
    let app = common::test_app_with_config(AppConfig {
        rate_limit: RateLimitConfig {
            enabled: true,
            default_rps: 1000,
            default_burst: 500,
            endpoints: EndpointRateLimits {
                admin_rps: Some(1000),
                data_rps: Some(1000),
            },
            ..Default::default()
        },
        ..Default::default()
    })
    .await;

    let wf_id = common::create_and_activate_workflow(&app, log_workflow("s9-wf", "s9")).await;

    // The channel name and its route prefix are deliberately different — that
    // difference is the whole bug.
    common::deploy_channel(
        &app,
        serde_json::json!({
            "name": "s9-orders-get",
            "channel_type": "sync",
            "protocol": "rest",
            "methods": ["GET"],
            "route_pattern": "/orders/{id}",
            "workflow_id": wf_id,
            "config": {
                "rate_limit": {
                    "requests_per_second": 1,
                    "burst": 1,
                    "key_logic": { "var": "client_ip" }
                }
            }
        }),
    )
    .await;

    let get_order = || {
        Request::builder()
            .method("GET")
            .uri("/api/v2/data/orders/ORD-1")
            .header("x-forwarded-for", "10.0.0.77")
            .body(Body::empty())
            .unwrap()
    };

    let resp1 = app.clone().oneshot(get_order()).await.unwrap();
    assert_ne!(
        resp1.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "the first request must be allowed through"
    );

    let resp2 = app.clone().oneshot(get_order()).await.unwrap();
    assert_eq!(
        resp2.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "the channel's own limit must apply to a REST-routed path"
    );
}

/// The same channel reached through its `/async` submission path is still the
/// same channel, so the limit follows it there too.
#[tokio::test]
async fn test_rate_limit_applies_to_rest_routed_async_path() {
    let app = common::test_app_with_config(AppConfig {
        rate_limit: RateLimitConfig {
            enabled: true,
            default_rps: 1000,
            default_burst: 500,
            endpoints: EndpointRateLimits {
                admin_rps: Some(1000),
                data_rps: Some(1000),
            },
            ..Default::default()
        },
        ..Default::default()
    })
    .await;

    let wf_id = common::create_and_activate_workflow(&app, log_workflow("s9-async-wf", "s9")).await;

    common::deploy_channel(
        &app,
        serde_json::json!({
            "name": "s9-events-post",
            "channel_type": "sync",
            "protocol": "rest",
            "methods": ["POST"],
            "route_pattern": "/events/{kind}",
            "workflow_id": wf_id,
            "config": {
                "rate_limit": {
                    "requests_per_second": 1,
                    "burst": 1,
                    "key_logic": { "var": "client_ip" }
                }
            }
        }),
    )
    .await;

    let post_event = || {
        Request::builder()
            .method("POST")
            .uri("/api/v2/data/events/click/async")
            .header("x-forwarded-for", "10.0.0.88")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"data":{"a":1}}"#))
            .unwrap()
    };

    let resp1 = app.clone().oneshot(post_event()).await.unwrap();
    assert_ne!(resp1.status(), StatusCode::TOO_MANY_REQUESTS);

    let resp2 = app.clone().oneshot(post_event()).await.unwrap();
    assert_eq!(
        resp2.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "the /async submission path resolves to the same channel"
    );
}

/// T16: every other test in this file sets `data_rps` high deliberately, to
/// get the platform limiter out of the way of whatever it is testing — so no
/// request had ever proven a data-plane 429 actually comes from
/// `endpoints.data_rps`. The channel below carries no `rate_limit` config at
/// all, so the platform limiter is the only thing that can refuse.
#[tokio::test]
async fn test_data_plane_429_comes_from_endpoints_data_rps() {
    let app = common::test_app_with_config(AppConfig {
        rate_limit: RateLimitConfig {
            enabled: true,
            default_rps: 1000,
            default_burst: 500,
            endpoints: EndpointRateLimits {
                admin_rps: Some(1000),
                data_rps: Some(1),
            },
            ..Default::default()
        },
        ..Default::default()
    })
    .await;

    // Deliberately no `config.rate_limit`: the platform limiter must be the
    // only 429 producer on this channel.
    common::create_and_activate_channel(&app, "data-rps-channel", log_workflow("data-rps-wf", "x"))
        .await;

    let data_req = |ip: &str| {
        Request::builder()
            .method("POST")
            .uri("/api/v2/data/data-rps-channel")
            .header("x-forwarded-for", ip.to_string())
            .header("content-type", "application/json")
            .body(Body::from(r#"{"data":{"n":1}}"#))
            .unwrap()
    };

    let resp1 = app.clone().oneshot(data_req("10.0.0.77")).await.unwrap();
    assert_ne!(
        resp1.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "first request must pass"
    );

    let resp2 = app.clone().oneshot(data_req("10.0.0.77")).await.unwrap();
    assert_eq!(
        resp2.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "second request must trip endpoints.data_rps"
    );
    let body = common::body_json(resp2).await;
    assert_eq!(body["error"]["code"], "RATE_LIMITED");

    // And it is genuinely the *platform* limiter, keyed per client: a
    // different client IP passes while the first is still over.
    let resp3 = app.clone().oneshot(data_req("10.0.0.78")).await.unwrap();
    assert_ne!(
        resp3.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "a different client must not share the first client's bucket"
    );
}
