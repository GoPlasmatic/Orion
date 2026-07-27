// Helpers in this module are shared across many test binaries; any given
// binary uses only a subset, so `dead_code` would fire for every other
// helper. Allow at the module level so individual functions don't each
// need their own marker.
#![allow(dead_code)]

/// Testcontainers-backed harness for the portable data_query/data_write dialects.
pub mod backends;
/// Shared task-builder DSL for the data-plane (data_query/data_write) tests.
pub mod dsl;

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
pub async fn test_app_with_config(config: AppConfig) -> Router {
    orion::server::build_router(test_state_with_config(config).await)
}

/// Build a full `AppState` for tests. Split from [`test_app_with_config`] so
/// multi-node harnesses (tests/cluster) can hold the state itself — e.g. to
/// spawn cluster tasks or inspect the channel registry directly.
pub async fn test_state_with_config(config: AppConfig) -> AppState {
    test_state_inner(config, None).await
}

/// `test_state_with_config` with the Kafka publisher registered against
/// `brokers`, so `publish_kafka` can be driven through a workflow (T3).
pub async fn test_state_with_kafka(config: AppConfig, brokers: &str) -> AppState {
    test_state_inner(config, Some(brokers.to_string())).await
}

async fn test_state_inner(config: AppConfig, kafka_brokers: Option<String>) -> AppState {
    // Install sqlx Any drivers for external connector pools (db_read/db_write tests)
    sqlx::any::install_default_drivers();

    // Use storage URL from config if set, otherwise default to in-memory SQLite
    let storage_config = if config.storage.url.is_empty()
        || config.storage.url == orion::config::StorageConfig::default().url
    {
        orion::config::StorageConfig {
            url: "sqlite::memory:".to_string(),
            max_connections: 5,
            ..config.storage.clone()
        }
    } else {
        orion::config::StorageConfig {
            max_connections: config.storage.max_connections.max(5),
            ..config.storage.clone()
        }
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
    let cluster = orion::cluster::init_cluster_runtime(&config.cluster, &pool)
        .await
        .expect("cluster runtime");
    let channel_registry = Arc::new(if config.cluster.enabled {
        ChannelRegistry::with_cluster((&*cluster).into())
    } else {
        ChannelRegistry::new()
    });
    let cache_pool = Arc::new(orion::connector::cache_backend::CachePool::new(
        config.engine.max_pool_cache_entries,
        60,
        config.engine.max_memory_cache_entries,
    ));
    let sql_pool_cache = Arc::new(orion::connector::pool_cache::SqlPoolCache::new(
        config.engine.max_pool_cache_entries,
    ));
    let mongo_pool_cache = Arc::new(orion::connector::mongo_pool::MongoPoolCache::new(
        config.engine.max_pool_cache_entries,
    ));

    // Mirror the production client (main.rs): no auto-redirects (execute_request
    // follows manually with SSRF re-validation) and pinned DNS resolution.
    let http_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(Arc::new(orion::validation::PinnedDnsResolver))
        .build()
        .unwrap();
    let engine = Arc::new(RwLock::new(Arc::new(
        dataflow_rs::Engine::builder().build().unwrap(),
    )));
    let mut custom_functions = orion::engine::build_custom_functions(
        connector_registry.clone(),
        http_client.clone(),
        engine.clone(),
        channel_registry.clone(),
        &config.engine,
        &config.query,
        &config.write,
        cache_pool.clone(),
        sql_pool_cache.clone(),
        mongo_pool_cache.clone(),
    );
    let kafka_producer = kafka_brokers.map(|brokers| {
        let producer = Arc::new(
            orion::kafka::producer::KafkaProducer::new(
                &brokers,
                &orion::config::KafkaAuthConfig::default(),
                &std::collections::HashMap::new(),
            )
            .expect("kafka producer"),
        );
        let producers = Arc::new(orion::kafka::producer::KafkaProducerCache::new(
            brokers,
            producer.clone(),
            orion::config::KafkaAuthConfig::default(),
            std::collections::HashMap::new(),
        ));
        orion::engine::register_kafka_publisher(
            &mut custom_functions,
            connector_registry.clone(),
            producers,
        );
        producer
    });
    let built_engine = dataflow_rs::Engine::new(vec![], custom_functions).unwrap();
    *engine.write().await = Arc::new(built_engine);

    // Start a small worker pool for async trace tests
    let dlq_repo: Arc<dyn orion::storage::repositories::trace_dlq::TraceDlqRepository> =
        Arc::new(orion::storage::repositories::trace_dlq::SqlTraceDlqRepository::new(pool.clone()));
    let test_queue_config = orion::config::QueueConfig {
        workers: 2,
        buffer_size: 100,
        shutdown_timeout_secs: 30,
        processing_timeout_ms: 60_000,
        max_result_size_bytes: 1_048_576,    // 1 MB
        max_queue_memory_bytes: 104_857_600, // 100 MB
        ..Default::default()
    };
    let (trace_persistence_queue_for_workers, _trace_persistence_handle) =
        orion::queue::trace_persistence::start(
            &config.tracing.storage,
            trace_repo.clone() as Arc<dyn orion::storage::repositories::traces::TraceRepository>,
        );
    let (trace_queue, _worker_handle) = orion::queue::start_workers(
        &test_queue_config,
        engine.clone(),
        trace_repo.clone() as Arc<dyn orion::storage::repositories::traces::TraceRepository>,
        Some(dlq_repo.clone()),
        channel_registry.clone(),
        trace_persistence_queue_for_workers.clone(),
        config.tracing.storage.clone(),
        config.engine.rollout_sticky_header.clone(),
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

    AppState::new(orion::server::state::AppStateInner {
        engine,
        channel_repo,
        workflow_repo,
        connector_repo,
        trace_repo,
        trace_dlq_repo: dlq_repo,
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
        datalogic: Arc::new(datalogic_rs::Engine::new()),
        rate_limit_state,
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        sql_pool_cache,
        mongo_pool_cache,
        kafka_consumer_handle: Arc::new(tokio::sync::Mutex::new(None)),
        kafka_producer,
        trace_persistence_queue: trace_persistence_queue_for_workers,
        cluster,
    })
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
pub async fn create_and_activate_channel(
    app: &axum::Router,
    channel_name: &str,
    workflow_json: serde_json::Value,
) -> (String, String) {
    create_and_activate_channel_with_config(app, channel_name, workflow_json, serde_json::json!({}))
        .await
}

// ============================================================
// Common test fixtures — avoids duplicating JSON payloads across tests
// ============================================================

/// A simple workflow that just logs a message. Used by most tests that need
/// an active workflow but don't care about its logic.
pub fn simple_log_workflow(name: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "condition": true,
        "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
    })
}

/// A workflow with priority and optional description. For tests that exercise
/// those specific fields.
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
pub fn workflow_with_tasks(name: &str, tasks: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "condition": true,
        "tasks": tasks
    })
}

/// A workflow that parses the JSON payload and echoes it back with a marker:
/// `data.echo` reflects the request's `data` and `data.matched` proves the
/// workflow ran. Lets tests observe the response shape (response caching,
/// REST routing).
pub fn echo_workflow(name: &str) -> serde_json::Value {
    workflow_with_tasks(
        name,
        serde_json::json!([
            {
                "id": "parse",
                "name": "Parse payload",
                "function": {
                    "name": "parse_json",
                    "input": { "source": "payload", "target": "input" }
                }
            },
            {
                "id": "echo",
                "name": "Echo input",
                "function": {
                    "name": "map",
                    "input": {
                        "mappings": [
                            { "path": "data.echo", "logic": { "var": "data.input" } },
                            { "path": "data.matched", "logic": true }
                        ]
                    }
                }
            }
        ]),
    )
}

/// Create and activate a channel with custom config (dedup, cache, validation, etc.).
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

/// Build a POST request with a custom Idempotency-Key header.
pub fn post_with_idempotency_key(uri: &str, key: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("Idempotency-Key", key)
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap()
}

/// Create a workflow from JSON, activate it, and return the workflow_id.
pub async fn create_and_activate_workflow(
    app: &axum::Router,
    workflow_json: serde_json::Value,
) -> String {
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
    let wf_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{}/status", wf_id),
            Some(serde_json::json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    wf_id
}

/// Create a REST channel with a specific route pattern and methods, activate it,
/// and return the channel_id.
pub async fn create_rest_channel(
    app: &axum::Router,
    name: &str,
    route_pattern: &str,
    methods: Vec<&str>,
    workflow_id: &str,
) -> String {
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(serde_json::json!({
                "name": name,
                "channel_type": "sync",
                "protocol": "rest",
                "methods": methods,
                "route_pattern": route_pattern,
                "workflow_id": workflow_id,
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED);
    let body = body_json(resp).await;
    let ch_id = body["data"]["channel_id"].as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{}/status", ch_id),
            Some(serde_json::json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    ch_id
}

/// Submit to an async endpoint and return `(trace_id, trace_token)` from the
/// 202. The token is required to poll the trace (R12).
pub async fn submit_async(
    app: &axum::Router,
    uri: &str,
    body: serde_json::Value,
) -> (String, String) {
    let resp = app
        .clone()
        .oneshot(json_request("POST", uri, Some(body)))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    let trace_id = body["trace_id"]
        .as_str()
        .expect("202 must carry trace_id")
        .to_string();
    let token = body["trace_token"]
        .as_str()
        .expect("202 must carry trace_token")
        .to_string();
    (trace_id, token)
}

/// Poll a trace until it reaches a terminal status or max iterations.
/// `token` is the `trace_token` from the async 202; `None` works only for
/// tokenless traces (sync rows) or when presenting admin credentials is
/// unnecessary (auth disabled).
pub async fn poll_trace_until_done(
    app: &axum::Router,
    trace_id: &str,
    max_polls: usize,
    token: Option<&str>,
) -> serde_json::Value {
    let mut body = serde_json::json!(null);
    for _ in 0..max_polls {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mut builder = Request::builder()
            .method("GET")
            .uri(format!("/api/v1/data/traces/{}", trace_id))
            .header("content-type", "application/json");
        if let Some(t) = token {
            builder = builder.header("x-trace-token", t);
        }
        let req = builder.body(Body::empty()).unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        body = body_json(resp).await;
        let status = body["status"].as_str().unwrap_or("");
        if status == "completed" || status == "failed" {
            break;
        }
    }
    body
}
