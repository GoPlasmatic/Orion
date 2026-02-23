use std::sync::Arc;

use axum::Router;
use tokio::sync::RwLock;

use orion::config::AppConfig;
use orion::connector::ConnectorRegistry;
use orion::server::state::AppState;
use orion::storage::repositories::connectors::SqliteConnectorRepository;
use orion::storage::repositories::jobs::SqliteJobRepository;
use orion::storage::repositories::rules::SqliteRuleRepository;

/// Create a test app with an in-memory SQLite database.
pub async fn test_app() -> Router {
    let pool = orion::storage::init_pool(":memory:").await.unwrap();

    let rule_repo = Arc::new(SqliteRuleRepository::new(pool.clone()));
    let connector_repo = Arc::new(SqliteConnectorRepository::new(pool.clone()));
    let job_repo = Arc::new(SqliteJobRepository::new(pool.clone()));
    let connector_registry = Arc::new(ConnectorRegistry::new());

    let engine = dataflow_rs::Engine::new(vec![], None);

    let state = AppState {
        engine: Arc::new(RwLock::new(Arc::new(engine))),
        rule_repo,
        connector_repo,
        job_repo,
        connector_registry,
        config: Arc::new(AppConfig::default()),
        start_time: chrono::Utc::now(),
    };

    orion::server::build_router(state)
}
