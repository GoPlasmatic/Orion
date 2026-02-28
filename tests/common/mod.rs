use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tokio::sync::RwLock;
use tower::ServiceExt;

use orion::config::AppConfig;
use orion::connector::ConnectorRegistry;
use orion::server::state::AppState;
use orion::storage::repositories::connectors::SqliteConnectorRepository;
use orion::storage::repositories::rules::SqliteRuleRepository;
use orion::storage::repositories::traces::SqliteTraceRepository;

/// Create a test app with an in-memory SQLite database.
pub async fn test_app() -> Router {
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

    let state = AppState {
        engine,
        rule_repo,
        connector_repo,
        trace_repo,
        connector_registry,
        trace_queue,
        config: Arc::new(AppConfig::default()),
        start_time: chrono::Utc::now(),
        metrics_handle,
        http_client,
        rate_limit_state: None,
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

/// Create a rule and activate it in one go. Returns the rule_id.
/// Use this helper in tests that need an active rule for data processing.
#[allow(dead_code)]
pub async fn create_and_activate_rule(app: &axum::Router, rule_json: serde_json::Value) -> String {
    let resp = app
        .clone()
        .oneshot(json_request("POST", "/api/v1/admin/rules", Some(rule_json)))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED);
    let body = body_json(resp).await;
    let rule_id = body["data"]["rule_id"].as_str().unwrap().to_string();

    // Activate the draft
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/rules/{}/status", rule_id),
            Some(serde_json::json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    rule_id
}
