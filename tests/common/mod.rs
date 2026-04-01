use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tokio::sync::RwLock;
use tower::ServiceExt;

use orion::channel::ChannelRegistry;
use orion::config::AppConfig;
use orion::connector::ConnectorRegistry;
use orion::server::rate_limit::RateLimitState;
use orion::server::state::AppState;
use orion::storage::repositories::channels::SqliteChannelRepository;
use orion::storage::repositories::connectors::SqliteConnectorRepository;
use orion::storage::repositories::traces::SqliteTraceRepository;
use orion::storage::repositories::workflows::SqliteWorkflowRepository;

/// Create a test app with an in-memory SQLite database.
pub async fn test_app() -> Router {
    test_app_with_config(AppConfig::default()).await
}

/// Create a test app with a custom config (e.g. for rate limiting tests).
#[allow(dead_code)]
pub async fn test_app_with_config(config: AppConfig) -> Router {
    let storage_config = orion::config::StorageConfig {
        path: ":memory:".to_string(),
        max_connections: 5,
        ..Default::default()
    };
    let pool = orion::storage::init_pool(&storage_config).await.unwrap();

    let channel_repo = Arc::new(SqliteChannelRepository::new(pool.clone()));
    let workflow_repo = Arc::new(SqliteWorkflowRepository::new(pool.clone()));
    let connector_repo = Arc::new(SqliteConnectorRepository::new(pool.clone()));
    let trace_repo = Arc::new(SqliteTraceRepository::new(pool.clone()));
    let connector_registry = Arc::new(ConnectorRegistry::new(Default::default()));
    let channel_registry = Arc::new(ChannelRegistry::new());

    let http_client = reqwest::Client::new();
    let engine = Arc::new(RwLock::new(Arc::new(dataflow_rs::Engine::new(
        vec![],
        None,
    ))));
    let custom_functions = orion::engine::build_custom_functions(
        connector_registry.clone(),
        http_client.clone(),
        engine.clone(),
    );
    let built_engine = dataflow_rs::Engine::new(vec![], Some(custom_functions));
    *engine.write().await = Arc::new(built_engine);

    // Start a small worker pool for async trace tests
    let (trace_queue, _worker_handle) = orion::queue::start_workers(
        2,
        100,
        30,
        engine.clone(),
        trace_repo.clone() as Arc<dyn orion::storage::repositories::traces::TraceRepository>,
    );

    // Init metrics recorder (use try — may already be initialized by another test)
    let metrics_handle = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .unwrap_or_else(|_| {
            // Recorder already installed by another test — create a no-op handle
            metrics_exporter_prometheus::PrometheusBuilder::new()
                .build_recorder()
                .handle()
        });

    let rate_limit_state = if config.rate_limit.enabled {
        Some(Arc::new(RateLimitState::from_config(&config.rate_limit)))
    } else {
        None
    };

    let state = AppState {
        engine,
        channel_repo,
        workflow_repo,
        connector_repo,
        trace_repo,
        connector_registry,
        channel_registry,
        trace_queue,
        config: Arc::new(config),
        start_time: chrono::Utc::now(),
        metrics_handle,
        http_client,
        datalogic: Arc::new(datalogic_rs::DataLogic::new()),
        rate_limit_state,
        #[cfg(feature = "kafka")]
        kafka_consumer_handle: Arc::new(tokio::sync::Mutex::new(None)),
        #[cfg(feature = "kafka")]
        kafka_producer: None,
    };

    orion::server::build_router(state)
}

pub fn json_request(method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let body = match body {
        Some(v) => Body::from(serde_json::to_string(&v).unwrap()),
        None => Body::empty(),
    };
    builder.body(body).unwrap()
}

pub async fn body_json(response: axum::http::Response<Body>) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Create a workflow and a channel, activate both, and return (channel_name, workflow_id).
/// Use this helper in tests that need an active channel for data processing.
#[allow(dead_code)]
pub async fn create_and_activate_channel(
    app: &axum::Router,
    channel_name: &str,
    workflow_json: serde_json::Value,
) -> (String, String) {
    // Create workflow
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(workflow_json),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED);
    let body = body_json(resp).await;
    let workflow_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    // Activate workflow
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{}/status", workflow_id),
            Some(serde_json::json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    // Create channel
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(serde_json::json!({
                "name": channel_name,
                "channel_type": "sync",
                "protocol": "http",
                "methods": ["POST"],
                "route_pattern": format!("/{}", channel_name),
                "workflow_id": workflow_id,
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED);
    let body = body_json(resp).await;
    let channel_id = body["data"]["channel_id"].as_str().unwrap().to_string();

    // Activate channel
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{}/status", channel_id),
            Some(serde_json::json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    (channel_name.to_string(), workflow_id)
}

/// Poll a trace until it reaches a terminal status or max iterations.
#[allow(dead_code)]
pub async fn poll_trace_until_done(
    app: &axum::Router,
    trace_id: &str,
    max_polls: usize,
) -> serde_json::Value {
    let mut body = serde_json::json!(null);
    for _ in 0..max_polls {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let resp = app
            .clone()
            .oneshot(json_request(
                "GET",
                &format!("/api/v1/data/traces/{}", trace_id),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        body = body_json(resp).await;
        let status = body["status"].as_str().unwrap_or("");
        if status == "completed" || status == "failed" {
            break;
        }
    }
    body
}
