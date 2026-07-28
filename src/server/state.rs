use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use datalogic_rs::Engine as DatalogicEngine;
use metrics_exporter_prometheus::PrometheusHandle;
use tokio::sync::{Mutex, RwLock};

use crate::channel::ChannelRegistry;
use crate::config::AppConfig;
use crate::connector::ConnectorRegistry;
use crate::connector::cache_backend::CachePool;
use crate::queue::TraceQueue;
use crate::server::rate_limit::RateLimitState;
use crate::storage::DbPool;
use crate::storage::repositories::audit_logs::AuditLogRepository;
use crate::storage::repositories::channels::ChannelRepository;
use crate::storage::repositories::connectors::ConnectorRepository;
use crate::storage::repositories::trace_dlq::TraceDlqRepository;
use crate::storage::repositories::traces::TraceRepository;
use crate::storage::repositories::workflows::WorkflowRepository;

/// Owned fields shared across all route handlers.
///
/// Wrapped in an `Arc` (via the [`AppState`] type alias) so the per-request
/// clone Axum performs on `State<AppState>` is a single atomic refcount bump
/// rather than one per `Arc` field (~20+).
pub struct AppStateInner {
    pub engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    pub channel_repo: Arc<dyn ChannelRepository>,
    pub workflow_repo: Arc<dyn WorkflowRepository>,
    pub connector_repo: Arc<dyn ConnectorRepository>,
    pub trace_repo: Arc<dyn TraceRepository>,
    /// Backs the `/admin/trace-dlq` operator routes (O4). The same repository
    /// instance the worker pool and the retry loop write to.
    pub trace_dlq_repo: Arc<dyn TraceDlqRepository>,
    pub audit_log_repo: Arc<dyn AuditLogRepository>,
    pub connector_registry: Arc<ConnectorRegistry>,
    pub cache_pool: Arc<CachePool>,
    pub channel_registry: Arc<ChannelRegistry>,
    pub trace_queue: TraceQueue,
    /// The startup pool. **Route handlers should not reach for this** — go
    /// through [`AppStateInner::pool_stats`] or
    /// [`AppStateInner::backup_sqlite_into`] (R26). It stays public because
    /// bootstrap assembles it and the integration harness seeds rows through
    /// it; the two route-layer call sites that used to unwrap a concrete
    /// `sqlx` pool here now go through those methods instead.
    #[doc(hidden)]
    pub db_pool: DbPool,
    pub config: Arc<AppConfig>,
    pub start_time: chrono::DateTime<chrono::Utc>,
    pub metrics_handle: PrometheusHandle,
    pub http_client: reqwest::Client,
    pub datalogic: Arc<DatalogicEngine>,
    pub rate_limit_state: Option<Arc<RateLimitState>>,
    /// Startup readiness flag — set to true after engine is fully loaded.
    pub ready: Arc<AtomicBool>,
    /// External SQL connection pool cache — shared so admin routes can evict stale pools.
    pub sql_pool_cache: Arc<crate::connector::pool_cache::SqlPoolCache>,
    /// External MongoDB connection pool cache — shared so admin routes can evict stale pools.
    pub mongo_pool_cache: Arc<crate::connector::mongo_pool::MongoPoolCache>,
    /// Kafka consumer handle — stored here so engine reload can restart the
    /// consumer when async channel topic mappings change.
    pub kafka_consumer_handle: Arc<Mutex<Option<crate::kafka::consumer::ConsumerHandle>>>,
    /// Kafka ingest health (K7): set degraded when a consumer (re)start
    /// fails, cleared once a consumer runs again. Reported as the `kafka`
    /// component of `/health` and `/readyz` (O10).
    pub kafka_ingest_status: Arc<crate::kafka::KafkaIngestStatus>,
    /// Kafka producer — needed to restart consumer with DLQ support.
    pub kafka_producer: Option<Arc<crate::kafka::producer::KafkaProducer>>,
    /// Background queue for trace-storage writes. A no-op handle in sync/off modes.
    pub trace_persistence_queue: crate::queue::TracePersistenceQueue,
    /// Multi-instance coordination runtime. Inert when `cluster.enabled = false`.
    pub cluster: Arc<crate::cluster::ClusterRuntime>,
    /// Per-client failed-admin-auth backoff. Node-local and ephemeral by
    /// design: it exists to blunt online guessing, not to be a shared ledger.
    pub admin_auth_failures: Arc<crate::server::admin_auth::FailedAuthTracker>,
}

impl AppStateInner {
    /// Trusted-proxy list used for client identification, empty when rate
    /// limiting is disabled (the list lives on the rate-limit config).
    pub fn rate_limit_trusted_proxies(&self) -> &[ipnet::IpNet] {
        self.rate_limit_state
            .as_ref()
            .map(|s| s.trusted_proxies.as_slice())
            .unwrap_or(&[])
    }

    /// `(size, idle)` connection counts for the `/health` gauges (R26).
    pub fn pool_stats(&self) -> (u32, usize) {
        (self.db_pool.size(), self.db_pool.num_idle())
    }

    /// Copy the database to `path` via SQLite's `VACUUM INTO` (R26).
    ///
    /// `Ok(false)` when the backend is not SQLite — the operation has no
    /// equivalent on PostgreSQL or MySQL, which rely on operator snapshot and
    /// PITR tooling. The backup route used to `match` on the pool variant
    /// itself, which is the only reason a concrete `sqlx` pool was reachable
    /// from a handler.
    pub async fn backup_sqlite_into(&self, path: &str) -> Result<bool, sqlx::Error> {
        let DbPool::Sqlite(pool) = &self.db_pool else {
            return Ok(false);
        };
        // `VACUUM INTO` takes a literal, not a bind parameter. The path is
        // operator-configured (`backup.directory`) plus a generated timestamp,
        // never caller-supplied; the escape is belt-and-braces.
        sqlx::query(&format!("VACUUM INTO '{}'", path.replace('\'', "''")))
            .execute(pool)
            .await?;
        Ok(true)
    }
}

/// Shared application state accessible from all route handlers.
///
/// Cloning is O(1) — one atomic refcount bump on the `Arc`. Field access goes
/// through `Arc<T>`'s built-in `Deref` so call sites (`state.engine`,
/// `state.config`, …) work directly against the inner struct.
pub type AppState = Arc<AppStateInner>;
