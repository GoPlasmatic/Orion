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
use orion::storage::repositories::channels::SqlChannelRepository;
use orion::storage::repositories::connectors::SqlConnectorRepository;
use orion::storage::repositories::traces::SqlTraceRepository;
use orion::storage::repositories::workflows::SqlWorkflowRepository;

/// Create a test app with an in-memory SQLite database.
pub async fn test_app() -> Router {
    test_app_with_config(AppConfig::default()).await
}

/// Create a test app with a custom config (e.g. for rate limiting tests).
#[allow(dead_code)]
pub async fn test_app_with_config(config: AppConfig) -> Router {
    // Install sqlx Any drivers for external connector pools (db_read/db_write tests)
    sqlx::any::install_default_drivers();

    let storage_config = orion::config::StorageConfig {
        url: "sqlite::memory:".to_string(),
        max_connections: 5,
        ..Default::default()
    };
    let pool = orion::storage::init_pool(&storage_config).await.unwrap();

    let channel_repo = Arc::new(SqlChannelRepository::new(pool.clone()));
    let workflow_repo = Arc::new(SqlWorkflowRepository::new(pool.clone()));
    let connector_repo = Arc::new(SqlConnectorRepository::new(pool.clone()));
    let trace_repo = Arc::new(SqlTraceRepository::new(pool.clone()));
    let audit_log_repo = Arc::new(
        orion::storage::repositories::audit_logs::SqlAuditLogRepository::new(pool.clone()),
    );
    let connector_registry = Arc::new(ConnectorRegistry::new(
        config.engine.circuit_breaker.clone(),
    ));
    let channel_registry = Arc::new(ChannelRegistry::new());
    let cache_pool = Arc::new(orion::connector::cache_backend::CachePool::new(
        config.engine.max_pool_cache_entries,
        60,
    ));
    let sql_pool_cache = Arc::new(orion::connector::pool_cache::SqlPoolCache::new(
        config.engine.max_pool_cache_entries,
    ));
    let mongo_pool_cache = Arc::new(orion::connector::mongo_pool::MongoPoolCache::new(
        config.engine.max_pool_cache_entries,
    ));

    let http_client = reqwest::Client::new();
    let engine = Arc::new(RwLock::new(Arc::new(dataflow_rs::Engine::new(
        vec![],
        None,
    ))));
    let custom_functions = orion::engine::build_custom_functions(
        connector_registry.clone(),
        http_client.clone(),
        engine.clone(),
        &config.engine,
        cache_pool.clone(),
        sql_pool_cache.clone(),
        mongo_pool_cache.clone(),
    );
    let built_engine = dataflow_rs::Engine::new(vec![], Some(custom_functions));
    *engine.write().await = Arc::new(built_engine);

    // Start a small worker pool for async trace tests
    let dlq_repo: Option<Arc<dyn orion::storage::repositories::trace_dlq::TraceDlqRepository>> =
        Some(Arc::new(
            orion::storage::repositories::trace_dlq::SqlTraceDlqRepository::new(pool.clone()),
        ));
    let test_queue_config = orion::config::QueueConfig {
        workers: 2,
        buffer_size: 100,
        shutdown_timeout_secs: 30,
        processing_timeout_ms: 60_000,
        max_result_size_bytes: 1_048_576,    // 1 MB
        max_queue_memory_bytes: 104_857_600, // 100 MB
        ..Default::default()
    };
    let (trace_queue, _worker_handle) = orion::queue::start_workers(
        &test_queue_config,
        engine.clone(),
        trace_repo.clone() as Arc<dyn orion::storage::repositories::traces::TraceRepository>,
        dlq_repo,
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
        audit_log_repo,
        connector_registry,
        cache_pool,
        channel_registry,
        trace_queue,
        db_pool: pool,
        config: Arc::new(config),
        start_time: chrono::Utc::now(),
        metrics_handle,
        http_client,
        datalogic: Arc::new(datalogic_rs::DataLogic::new()),
        rate_limit_state,
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        sql_pool_cache,
        mongo_pool_cache,
        kafka_consumer_handle: Arc::new(tokio::sync::Mutex::new(None)),
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

// ============================================================
// Common test fixtures — avoids duplicating JSON payloads across tests
// ============================================================

/// A simple workflow that just logs a message. Used by most tests that need
/// an active workflow but don't care about its logic.
#[allow(dead_code)]
pub fn simple_log_workflow(name: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "condition": true,
        "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
    })
}

/// A workflow with priority and optional description. For tests that exercise
/// those specific fields.
#[allow(dead_code)]
pub fn workflow_with_priority(name: &str, priority: i64) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "priority": priority,
        "condition": true,
        "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
    })
}

/// A sync HTTP channel pointing at the given workflow_id. Used by tests
/// that need a channel associated with a specific route pattern.
#[allow(dead_code)]
pub fn sync_http_channel(name: &str, workflow_id: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "channel_type": "sync",
        "protocol": "http",
        "methods": ["POST"],
        "route_pattern": format!("/{}", name),
        "workflow_id": workflow_id,
    })
}

/// A database connector fixture for tests that exercise connector CRUD.
#[allow(dead_code)]
pub fn db_connector(name: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "connector_type": "db",
        "config": {
            "connection_string": "sqlite::memory:",
            "driver": "sqlite"
        }
    })
}

/// A SQLite database connector for integration tests.
/// Uses a shared named in-memory DB so multiple queries share the same data.
#[allow(dead_code)]
pub fn db_connector_sqlite(name: &str, db_path: &str) -> serde_json::Value {
    serde_json::json!({
        "id": name,
        "name": name,
        "connector_type": "db",
        "config": {
            "type": "db",
            "connection_string": db_path,
            "driver": "sqlite",
            "max_connections": 1,
            "query_timeout_ms": 5000
        }
    })
}

/// An in-memory cache connector for integration tests.
#[allow(dead_code)]
pub fn cache_connector_memory(name: &str) -> serde_json::Value {
    serde_json::json!({
        "id": name,
        "name": name,
        "connector_type": "cache",
        "config": {
            "type": "cache",
            "backend": "memory"
        }
    })
}

/// A Redis-backed cache connector (for #[ignore] tests).
#[allow(dead_code)]
pub fn cache_connector_redis(name: &str, url: &str) -> serde_json::Value {
    serde_json::json!({
        "id": name,
        "name": name,
        "connector_type": "cache",
        "config": {
            "type": "cache",
            "backend": "redis",
            "url": url
        }
    })
}

/// Create a connector via admin API and return the connector ID.
#[allow(dead_code)]
pub async fn create_connector(app: &axum::Router, connector_json: serde_json::Value) -> String {
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(connector_json),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED);
    let body = body_json(resp).await;
    body["data"]["id"].as_str().unwrap().to_string()
}

/// A generic workflow builder that wraps a tasks array with `condition: true`.
#[allow(dead_code)]
pub fn workflow_with_tasks(name: &str, tasks: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "condition": true,
        "tasks": tasks
    })
}

/// Create and activate a channel with custom config (dedup, cache, validation, etc.).
#[allow(dead_code)]
pub async fn create_and_activate_channel_with_config(
    app: &axum::Router,
    channel_name: &str,
    workflow_json: serde_json::Value,
    channel_config: serde_json::Value,
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

    // Create channel with config
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
                "config": channel_config,
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
