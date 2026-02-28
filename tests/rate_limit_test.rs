mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::sync::Arc;
use tokio::sync::RwLock;
use tower::ServiceExt;

use orion::config::{AppConfig, ChannelRateLimit, EndpointRateLimits, RateLimitConfig};
use orion::connector::ConnectorRegistry;
use orion::server::rate_limit::RateLimitState;
use orion::server::state::AppState;
use orion::storage::repositories::connectors::SqliteConnectorRepository;
use orion::storage::repositories::rules::SqliteRuleRepository;
use orion::storage::repositories::traces::SqliteTraceRepository;

/// Create a test app with rate limiting configured via the given config.
async fn rate_limited_app_with_config(rate_limit_config: RateLimitConfig) -> Router {
    let storage_config = orion::config::StorageConfig {
        path: ":memory:".to_string(),
        max_connections: 5,
        ..Default::default()
    };
    let pool = orion::storage::init_pool(&storage_config).await.unwrap();

    let rule_repo = Arc::new(SqliteRuleRepository::new(pool.clone()));
    let connector_repo = Arc::new(SqliteConnectorRepository::new(pool.clone()));
    let trace_repo = Arc::new(SqliteTraceRepository::new(pool.clone()));
    let connector_registry = Arc::new(ConnectorRegistry::new(Default::default()));

    let http_client = reqwest::Client::new();
    let custom_functions =
        orion::engine::build_custom_functions(connector_registry.clone(), http_client.clone());
    let engine = dataflow_rs::Engine::new(vec![], Some(custom_functions));
    let engine = Arc::new(RwLock::new(Arc::new(engine)));

    let (trace_queue, _worker_handle) = orion::queue::start_workers(
        2,
        100,
        30,
        engine.clone(),
        trace_repo.clone() as Arc<dyn orion::storage::repositories::traces::TraceRepository>,
    );

    let metrics_handle = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .unwrap_or_else(|_| {
            metrics_exporter_prometheus::PrometheusBuilder::new()
                .build_recorder()
                .handle()
        });

    let rate_limit_state = Some(Arc::new(RateLimitState::from_config(&rate_limit_config)));

    let config = AppConfig {
        rate_limit: rate_limit_config,
        ..Default::default()
    };

    let state = AppState {
        engine,
        rule_repo,
        connector_repo,
        trace_repo,
        connector_registry,
        trace_queue,
        config: Arc::new(config),
        start_time: chrono::Utc::now(),
        metrics_handle,
        http_client,
        rate_limit_state,
    };

    orion::server::build_router(state)
}

/// Create a test app with rate limiting enabled (very low limits for testing).
async fn rate_limited_app(rps: u32, burst: u32) -> Router {
    rate_limited_app_with_config(RateLimitConfig {
        enabled: true,
        default_rps: rps,
        default_burst: burst,
        ..Default::default()
    })
    .await
}

/// Create a rate-limited app with channel-specific limits.
async fn rate_limited_app_with_channels() -> Router {
    let mut channels = std::collections::HashMap::new();
    channels.insert(
        "orders".to_string(),
        ChannelRateLimit {
            rps: 1,
            burst: Some(1),
        },
    );

    rate_limited_app_with_config(RateLimitConfig {
        enabled: true,
        default_rps: 100,
        default_burst: 50,
        endpoints: EndpointRateLimits {
            admin_rps: Some(50),
            data_rps: Some(100),
        },
        channels,
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
        .uri("/api/v1/admin/rules")
        .header("x-forwarded-for", "10.0.0.200")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Second should be rate limited
    let req2 = Request::builder()
        .method("GET")
        .uri("/api/v1/admin/rules")
        .header("x-forwarded-for", "10.0.0.200")
        .body(Body::empty())
        .unwrap();
    let resp2 = app.oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn test_rate_limit_channel_specific() {
    let app = rate_limited_app_with_channels().await;

    // First request to orders channel
    let req1 = Request::builder()
        .method("POST")
        .uri("/api/v1/data/orders")
        .header("x-forwarded-for", "10.0.0.50")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"data":{"test":true}}"#))
        .unwrap();
    let resp1 = app.clone().oneshot(req1).await.unwrap();
    // Should succeed (may be 200 or other status based on data processing, but not 429)
    assert_ne!(resp1.status(), StatusCode::TOO_MANY_REQUESTS);

    // Second request to same channel from same IP — should be rate limited by channel limiter
    let req2 = Request::builder()
        .method("POST")
        .uri("/api/v1/data/orders")
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
