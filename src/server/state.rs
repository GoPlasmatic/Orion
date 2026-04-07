use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use datalogic_rs::DataLogic;
use metrics_exporter_prometheus::PrometheusHandle;
use tokio::sync::{Mutex, RwLock};

use crate::channel::ChannelRegistry;
use crate::config::AppConfig;
use crate::connector::ConnectorRegistry;
use crate::queue::TraceQueue;
use crate::server::rate_limit::RateLimitState;
use crate::storage::DbPool;
use crate::storage::repositories::channels::ChannelRepository;
use crate::storage::repositories::connectors::ConnectorRepository;
use crate::storage::repositories::traces::TraceRepository;
use crate::storage::repositories::workflows::WorkflowRepository;

/// Shared application state accessible from all route handlers.
#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    pub channel_repo: Arc<dyn ChannelRepository>,
    pub workflow_repo: Arc<dyn WorkflowRepository>,
    pub connector_repo: Arc<dyn ConnectorRepository>,
    pub trace_repo: Arc<dyn TraceRepository>,
    pub connector_registry: Arc<ConnectorRegistry>,
    pub channel_registry: Arc<ChannelRegistry>,
    pub trace_queue: TraceQueue,
    pub db_pool: DbPool,
    pub config: Arc<AppConfig>,
    pub start_time: chrono::DateTime<chrono::Utc>,
    pub metrics_handle: PrometheusHandle,
    pub http_client: reqwest::Client,
    pub datalogic: Arc<DataLogic>,
    pub rate_limit_state: Option<Arc<RateLimitState>>,
    /// Startup readiness flag — set to true after engine is fully loaded.
    pub ready: Arc<AtomicBool>,
    /// Kafka consumer handle — stored here so engine reload can restart the
    /// consumer when async channel topic mappings change.
    #[cfg(feature = "kafka")]
    pub kafka_consumer_handle: Arc<Mutex<Option<crate::kafka::consumer::ConsumerHandle>>>,
    /// Kafka producer — needed to restart consumer with DLQ support.
    #[cfg(feature = "kafka")]
    pub kafka_producer: Option<Arc<crate::kafka::producer::KafkaProducer>>,
}
