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
use tower::ServiceExt;

use orion::channel::ChannelRegistry;
use orion::config::AppConfig;
use orion::server::state::AppState;

/// Create a test app with an in-memory SQLite database.
///
/// Trace persistence is pinned to `sync` explicitly, even though that is also
/// the product default: a test that submits a request and then asserts on the
/// trace needs the row committed before the response returns, and inheriting
/// that from a default is inheriting a decision that can be revisited. Under
/// `batch` the write lands on a background worker up to
/// `batch_flush_interval_ms` later, so every such assertion would race the
/// flush and fail intermittently.
///
/// This is a statement about test determinism, not about the default. Tests
/// that exercise a particular mode — including `sync` itself — set it
/// explicitly through [`test_app_with_config`] and do not rely on this.
pub async fn test_app() -> Router {
    let mut config = AppConfig::default();
    config.trace_storage.mode = orion::config::TraceStorageMode::Sync;
    test_app_with_config(config).await
}

/// Create a test app with a custom config (e.g. for rate limiting tests).
pub async fn test_app_with_config(config: AppConfig) -> Router {
    orion::server::build_router(test_state_with_config(config).await)
}

/// Build a full `AppState` for tests. Split from [`test_app_with_config`] so
/// multi-node harnesses (tests/cluster) can hold the state itself — e.g. to
/// spawn cluster tasks or inspect the channel registry directly.
pub async fn test_state_with_config(config: AppConfig) -> AppState {
    test_state_inner(config, None).await.0
}

/// Like [`test_state_with_config`] but also returns the background-task
/// handles, so the caller can own the worker lifecycle (abort the loops,
/// drive `TaskHandles::shutdown()` — the cluster harness and shutdown tests).
pub async fn test_state_with_handles(
    config: AppConfig,
) -> (AppState, orion::bootstrap::TaskHandles) {
    test_state_inner(config, None).await
}

/// `test_state_with_config` with the Kafka publisher registered against
/// `brokers`, so `publish_kafka` can be driven through a workflow (T3).
pub async fn test_state_with_kafka(config: AppConfig, brokers: &str) -> AppState {
    test_state_inner(config, Some(brokers.to_string())).await.0
}

/// Build `AppState` through the REAL boot path — `orion::bootstrap`'s
/// builders, in the same order as `main.rs::run()` — so wiring drift between
/// tests and production startup is impossible.
///
/// Deliberate divergences from production bootstrap (everything else must go
/// through `orion::bootstrap`):
///
/// 1. **Storage**: an empty/default URL becomes in-memory SQLite with a pool
///    of >= 5, via `init_pool` (always migrates) instead of
///    `init_pool_for_startup` (tests must not trip the migration guard). The
///    override is NOT written back into `config` — `state.config` keeps what
///    the test passed (pool_exhaustion_test depends on that).
/// 2. **Metrics**: `install_recorder()` with a no-op fallback — the recorder
///    is process-global and only the first test app can install it. Never
///    `init_metrics_handle`, and never `init_observability` (a global
///    tracing subscriber would collide across tests).
/// 3. **DLQ retry loop forced off**: a harness-owned 30 s DLQ poller would
///    steal claims from tests that drive retries themselves
///    (trace_dlq_poison_test, trace_dlq_admin_test). Tests that want the
///    loop call `orion::queue::start_dlq_retry` directly.
async fn test_state_inner(
    mut config: AppConfig,
    kafka_brokers: Option<String>,
) -> (AppState, orion::bootstrap::TaskHandles) {
    // Install sqlx Any drivers for external connector pools (db_read/db_write tests)
    sqlx::any::install_default_drivers();

    // Divergence 1: in-memory SQLite fallback (see doc comment).
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

    // Divergence 3: no harness-owned DLQ retry loop.
    config.trace_queue.dlq_retry_enabled = false;

    // Kafka: map the harness broker override onto the config so
    // bootstrap::setup_kafka_producer registers the publisher through the
    // same path as production.
    if let Some(brokers) = kafka_brokers {
        config.kafka.enabled = true;
        config.kafka.brokers = vec![brokers];
    }

    // From here on: the production startup sequence, same order as main::run.
    let cluster = orion::cluster::init_cluster_runtime(&config.cluster, &pool)
        .await
        .expect("cluster runtime");
    let repos = orion::bootstrap::Repositories::new(&pool, &config.storage).expect("repositories");
    let channel_registry = Arc::new(if config.cluster.enabled {
        ChannelRegistry::with_cluster((&*cluster).into())
    } else {
        ChannelRegistry::new()
    });
    let components =
        orion::bootstrap::build_engine_components(&config, &repos, channel_registry.clone())
            .await
            .expect("engine components");
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (components, channels, active_workflow_count) = components
        .load_channels_and_build_engine(&config, &repos, &channel_registry)
        .await
        .expect("load channels");
    ready.store(true, std::sync::atomic::Ordering::Release);

    // None unless the test's config carries `[kafka.topics]` mappings (the
    // DB is empty at boot, so DB-driven topics never contribute); wired for
    // parity with production either way.
    let kafka_consumer_handle = orion::bootstrap::start_kafka_ingest(
        &config.kafka,
        &channels,
        components.engine.clone(),
        channel_registry.clone(),
        components.datalogic.clone(),
        components.kafka_producer.clone(),
        cluster.enabled.then(|| cluster.instance_id.as_str()),
    )
    .expect("kafka ingest");

    let (trace_persistence_queue, trace_queue, audit_queue, task_handles) =
        orion::bootstrap::start_background_tasks(
            &config,
            components.engine.clone(),
            &repos,
            channel_registry.clone(),
            &cluster,
        );
    // Divergence 2: process-global metrics recorder (see doc comment).
    //
    // T37: through orion's own init rather than a raw `PrometheusBuilder` —
    // the raw install left `orion::metrics::is_enabled()` false, so every
    // metric write in the app short-circuited before reaching the recorder
    // and the rendered exposition was empty in *every* test app, first or
    // not (which is why `metrics_endpoint_test` could only assert status
    // codes). Same first-caller-wins semantics: later apps get a standalone
    // handle from the fallback inside `init_metrics_with_instance`. Ordered
    // before `set_active_workflows`, or the first app's gauge write lands
    // before any recorder exists and is dropped.
    let metrics_handle = orion::metrics::init_metrics_with_instance(None);
    orion::metrics::set_active_workflows(active_workflow_count as f64);
    let rate_limit_state = orion::bootstrap::build_rate_limit_state(&config);

    let state = orion::bootstrap::build_app_state(orion::bootstrap::AppStateParams {
        config: Arc::new(config),
        pool,
        repos,
        components,
        channel_registry,
        trace_queue,
        trace_persistence_queue,
        audit_queue,
        rate_limit_state,
        metrics_handle,
        ready,
        kafka_consumer_handle,
        cluster,
    });
    (state, task_handles)
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

/// Poll a GET endpoint until `pred` holds on the parsed body (5s deadline,
/// 25ms interval). For eventually-persisted state (traces, audit rows) —
/// a condition to poll for, never a duration to guess with a sleep.
pub async fn wait_for_body<F>(app: &axum::Router, uri: &str, pred: F) -> serde_json::Value
where
    F: Fn(&serde_json::Value) -> bool,
{
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let resp = app
            .clone()
            .oneshot(json_request("GET", uri, None))
            .await
            .unwrap();
        let body = body_json(resp).await;
        if pred(&body) {
            return body;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition on {uri} not met within 5s; last body: {body}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
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
///
/// `allow_private_urls` is required — the S6 endpoint check refuses loopback
/// targets, and the Redis container publishes on 127.0.0.1.
pub fn cache_connector_redis(name: &str, url: &str) -> serde_json::Value {
    serde_json::json!({
        "id": name,
        "name": name,
        "connector_type": "cache",
        "config": {
            "type": "cache",
            "backend": "redis",
            "url": url,
            "allow_private_urls": true
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
    let (_channel_id, workflow_id) =
        create_and_activate_channel_full(app, channel_name, workflow_json, channel_config).await;
    (channel_name.to_string(), workflow_id)
}

/// Like [`create_and_activate_channel_with_config`] but returns
/// `(channel_id, workflow_id)` — for tests that drive status changes or
/// deletes against the channel id (e.g. the cluster propagation tests).
pub async fn create_and_activate_channel_full(
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

    (channel_id, workflow_id)
}

// ============================================================
// Circuit-breaker fixtures (shared by circuit_breaker_test and the
// cluster breaker fan-out test)
// ============================================================

/// Spin up a mock HTTP server that always returns 500 Internal Server Error.
pub async fn start_failing_server() -> std::net::SocketAddr {
    let mock_app = axum::Router::new().route(
        "/fail",
        axum::routing::post(|| async {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({"error": "boom"})),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, mock_app).await.unwrap();
    });
    addr
}

/// Create an HTTP connector pointing at the given address with zero retries.
/// `allow_private_urls` is required — SSRF validation refuses loopback
/// targets otherwise.
pub async fn create_http_connector(app: &axum::Router, name: &str, addr: std::net::SocketAddr) {
    create_connector(
        app,
        serde_json::json!({
            "id": name,
            "name": name,
            "connector_type": "http",
            "config": {
                "type": "http",
                "url": format!("http://{}", addr),
                "retry": {"max_retries": 0, "retry_delay_ms": 10},
                "allow_private_urls": true
            }
        }),
    )
    .await;
}

/// Build a workflow whose single task calls http_call on the given connector
/// to POST /fail.
pub fn failing_http_workflow(name: &str, connector: &str) -> serde_json::Value {
    workflow_with_tasks(
        name,
        serde_json::json!([{
            "id": "t1",
            "name": "Call failing endpoint",
            "function": {
                "name": "http_call",
                "input": {
                    "connector": connector,
                    "method": "POST",
                    "path": "/fail",
                    "body": {"test": true},
                    "response_path": "data.result",
                    "timeout_ms": 5000
                }
            }
        }]),
    )
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
///
/// Returns the **unwrapped** trace — the `data` envelope every admin 2xx
/// carries since R17 is stripped here so callers assert on trace fields.
/// The envelope itself is pinned by `admin_envelope_test`.
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
            .uri(format!("/api/v1/admin/traces/{}", trace_id))
            .header("content-type", "application/json");
        if let Some(t) = token {
            builder = builder.header("x-trace-token", t);
        }
        let req = builder.body(Body::empty()).unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let mut envelope = body_json(resp).await;
        body = envelope["data"].take();
        let status = body["status"].as_str().unwrap_or("");
        if status == "completed" || status == "failed" {
            return body;
        }
    }
    // Running out of polls used to return the last non-terminal body, so a
    // pipeline that stopped finishing traces surfaced as a confusing
    // assertion further down — or, at the call sites that discard the result,
    // as nothing at all. Fail here instead, naming the status it was stuck in.
    panic!(
        "trace {trace_id} did not reach a terminal status within {max_polls} polls \
         ({}ms); last body: {body}",
        max_polls * 50
    );
}

/// Self-cleaning scratch directory; `tempfile` is not in the dependency tree.
/// One implementation for every module that needs a disk-backed fixture, so
/// the naming scheme and the cleanup-on-drop behaviour live here rather than
/// in per-module copies that drift.
pub struct ScratchDir(std::path::PathBuf);

impl ScratchDir {
    pub fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "orion_test_{label}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        Self(path)
    }

    pub fn path(&self) -> &std::path::Path {
        &self.0
    }

    /// A SQLite DSN for an `orion.db` inside this directory — the shape every
    /// storage-backed test wants.
    pub fn url(&self) -> String {
        format!("sqlite:{}/orion.db", self.0.display())
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
