#![allow(clippy::unwrap_used, clippy::needless_update)]

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use orion::config::{AppConfig, EndpointRateLimits, RateLimitConfig};

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
    let app = rate_limited_app(1, 1).await;

    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/admin/workflows")
        .header("x-forwarded-for", "10.0.0.200")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Second should be rate limited
    let req2 = Request::builder()
        .method("GET")
        .uri("/api/v1/admin/workflows")
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

    // Create a workflow
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(serde_json::json!({
                "name": "rate-limit-test-wf",
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = common::body_json(resp).await;
    let wf_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    // Activate workflow
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{}/status", wf_id),
            Some(serde_json::json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Create a channel with per-channel rate limit in config_json (1 rps, burst 1)
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(serde_json::json!({
                "name": "rate-limited-orders",
                "channel_type": "sync",
                "protocol": "http",
                "methods": ["POST"],
                "route_pattern": "/rate-limited-orders",
                "workflow_id": wf_id,
                "config": {
                    "rate_limit": {
                        "requests_per_second": 1,
                        "burst": 1,
                        "key_logic": { "var": "client_ip" }
                    }
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = common::body_json(resp).await;
    let ch_id = body["data"]["channel_id"].as_str().unwrap().to_string();

    // Activate channel (triggers engine reload → ChannelRegistry rebuilt with rate limiter)
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{}/status", ch_id),
            Some(serde_json::json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // First request to channel — should succeed (not 429)
    let req1 = Request::builder()
        .method("POST")
        .uri("/api/v1/data/rate-limited-orders")
        .header("x-forwarded-for", "10.0.0.50")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"data":{"test":true}}"#))
        .unwrap();
    let resp1 = app.clone().oneshot(req1).await.unwrap();
    assert_ne!(resp1.status(), StatusCode::TOO_MANY_REQUESTS);

    // Second request from same IP to same channel — should be rate limited
    let req2 = Request::builder()
        .method("POST")
        .uri("/api/v1/data/rate-limited-orders")
        .header("x-forwarded-for", "10.0.0.50")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"data":{"test":true}}"#))
        .unwrap();
    let resp2 = app.clone().oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::TOO_MANY_REQUESTS);
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

    // Create workflow
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(serde_json::json!({
                "name": "key-logic-test-wf",
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
            })),
        ))
        .await
        .unwrap();
    let body = common::body_json(resp).await;
    let wf_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    let _ = app
        .clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{}/status", wf_id),
            Some(serde_json::json!({"status": "active"})),
        ))
        .await
        .unwrap();

    // Create channel with key_logic keyed by x-api-key header
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(serde_json::json!({
                "name": "api-key-limited",
                "channel_type": "sync",
                "protocol": "http",
                "methods": ["POST"],
                "route_pattern": "/api-key-limited",
                "workflow_id": wf_id,
                "config": {
                    "rate_limit": {
                        "requests_per_second": 1,
                        "burst": 1,
                        "key_logic": { "var": "headers.x-api-key" }
                    }
                }
            })),
        ))
        .await
        .unwrap();
    let body = common::body_json(resp).await;
    let ch_id = body["data"]["channel_id"].as_str().unwrap().to_string();

    let _ = app
        .clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{}/status", ch_id),
            Some(serde_json::json!({"status": "active"})),
        ))
        .await
        .unwrap();

    // Request with API key "key-A" — should succeed
    let req1 = Request::builder()
        .method("POST")
        .uri("/api/v1/data/api-key-limited")
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
        .uri("/api/v1/data/api-key-limited")
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
        .uri("/api/v1/data/api-key-limited")
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
