use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tokio::sync::RwLock;

use orion::config::AppConfig;
use orion::connector::ConnectorRegistry;
use orion::server::state::AppState;
use orion::storage::repositories::connectors::SqliteConnectorRepository;
use orion::storage::repositories::jobs::SqliteJobRepository;
use orion::storage::repositories::rules::SqliteRuleRepository;

/// Create a test app with an in-memory SQLite database.
pub async fn test_app() -> Router {
    let pool = orion::storage::init_pool(":memory:", 5).await.unwrap();

    let rule_repo = Arc::new(SqliteRuleRepository::new(pool.clone()));
    let connector_repo = Arc::new(SqliteConnectorRepository::new(pool.clone()));
    let job_repo = Arc::new(SqliteJobRepository::new(pool.clone()));
    let connector_registry = Arc::new(ConnectorRegistry::new());

    let custom_functions = orion::engine::build_custom_functions(connector_registry.clone());
    let engine = dataflow_rs::Engine::new(vec![], Some(custom_functions));
    let engine = Arc::new(RwLock::new(Arc::new(engine)));

    // Start a small worker pool for async job tests
    let (job_queue, _worker_handle) = orion::queue::start_workers(
        2,
        100,
        engine.clone(),
        job_repo.clone() as Arc<dyn orion::storage::repositories::jobs::JobRepository>,
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

    let state = AppState {
        engine,
        rule_repo,
        connector_repo,
        job_repo,
        connector_registry,
        job_queue,
        config: Arc::new(AppConfig::default()),
        start_time: chrono::Utc::now(),
        db_pool: pool,
        metrics_handle,
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
