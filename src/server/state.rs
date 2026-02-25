use std::sync::Arc;

use metrics_exporter_prometheus::PrometheusHandle;
use tokio::sync::RwLock;

use crate::config::AppConfig;
use crate::connector::ConnectorRegistry;
use crate::queue::JobQueue;
use crate::server::rate_limit::RateLimitState;
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
    pub job_queue: JobQueue,
    pub config: Arc<AppConfig>,
    pub start_time: chrono::DateTime<chrono::Utc>,
    pub metrics_handle: PrometheusHandle,
    pub http_client: reqwest::Client,
    pub rate_limit_state: Option<Arc<RateLimitState>>,
}
