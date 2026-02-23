use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::AppConfig;
use crate::connector::ConnectorRegistry;
use crate::storage::repositories::connectors::ConnectorRepository;
use crate::storage::repositories::jobs::JobRepository;
use crate::storage::repositories::rules::RuleRepository;

/// Shared application state accessible from all route handlers.
#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    pub rule_repo: Arc<dyn RuleRepository>,
    pub connector_repo: Arc<dyn ConnectorRepository>,
    pub job_repo: Arc<dyn JobRepository>,
    pub connector_registry: Arc<ConnectorRegistry>,
    pub config: Arc<AppConfig>,
}
