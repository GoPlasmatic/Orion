//! Startup-sequence helpers for `run()` in `main.rs` — observability init,
//! repository construction, and background-task lifecycle. `main.rs` stays
//! the readable orchestration script and calls these phases in order.

use std::sync::Arc;

use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use orion::config::{self, LogFormat};
use orion::storage::repositories::channels::{ChannelRepository, SqlChannelRepository};
use orion::storage::repositories::connectors::{ConnectorRepository, SqlConnectorRepository};
use orion::storage::repositories::traces::{SqlTraceRepository, TraceRepository};
use orion::storage::repositories::workflows::{SqlWorkflowRepository, WorkflowRepository};

/// Initialise a plain `tracing_subscriber::fmt` subscriber (no OpenTelemetry).
fn init_fmt_subscriber(level: &str, format: &LogFormat) {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    match format {
        LogFormat::Json => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .json()
                .init();
        }
        LogFormat::Pretty => {
            tracing_subscriber::fmt().with_env_filter(env_filter).init();
        }
    }
}

/// Init tracing subscriber with optional OpenTelemetry layer.
///
/// When `tracing.enabled = true`, an additional OpenTelemetry layer is added
/// that exports all spans via OTLP. Existing `#[instrument]` annotations
/// automatically become distributed-trace-compatible with zero changes.
/// Returns the OTel tracer provider (for the shutdown flush) when enabled.
pub(crate) fn init_observability(
    config: &config::AppConfig,
) -> Result<Option<opentelemetry_sdk::trace::SdkTracerProvider>, Box<dyn std::error::Error>> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.logging.level));
    if config.tracing.enabled {
        let (provider, tracer) =
            orion::server::otel::init_otel_pipeline(&config.tracing, &config.cluster.instance_id)?;
        match config.logging.format {
            LogFormat::Json => {
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(tracing_subscriber::fmt::layer().json())
                    .with(tracing_opentelemetry::layer().with_tracer(tracer))
                    .init();
            }
            LogFormat::Pretty => {
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(tracing_subscriber::fmt::layer())
                    .with(tracing_opentelemetry::layer().with_tracer(tracer))
                    .init();
            }
        }
        Ok(Some(provider))
    } else {
        init_fmt_subscriber(&config.logging.level, &config.logging.format);
        Ok(None)
    }
}

/// Init metrics (gated by config).
pub(crate) fn init_metrics_handle(
    config: &config::AppConfig,
) -> metrics_exporter_prometheus::PrometheusHandle {
    if config.metrics.enabled {
        let handle = orion::metrics::init_metrics();
        tracing::info!("Prometheus metrics initialized");
        handle
    } else {
        // Create a no-op handle that still works but doesn't install a global recorder
        metrics_exporter_prometheus::PrometheusBuilder::new()
            .build_recorder()
            .handle()
    }
}

/// The repository set backing `AppState` and the background tasks, all
/// constructed from the same startup pool.
pub(crate) struct Repositories {
    pub(crate) workflows: Arc<dyn WorkflowRepository>,
    pub(crate) channels: Arc<dyn ChannelRepository>,
    pub(crate) connectors: Arc<dyn ConnectorRepository>,
    pub(crate) traces: Arc<dyn TraceRepository>,
    pub(crate) audit_logs: Arc<dyn orion::storage::repositories::audit_logs::AuditLogRepository>,
    pub(crate) trace_dlq: Arc<dyn orion::storage::repositories::trace_dlq::TraceDlqRepository>,
}

impl Repositories {
    /// Create repositories.
    pub(crate) fn new(pool: &orion::storage::DbPool) -> Self {
        Self {
            workflows: Arc::new(SqlWorkflowRepository::new(pool.clone())),
            channels: Arc::new(SqlChannelRepository::new(pool.clone())),
            connectors: Arc::new(SqlConnectorRepository::new(pool.clone())),
            traces: Arc::new(SqlTraceRepository::new(pool.clone())),
            audit_logs: Arc::new(
                orion::storage::repositories::audit_logs::SqlAuditLogRepository::new(pool.clone()),
            ),
            trace_dlq: Arc::new(
                orion::storage::repositories::trace_dlq::SqlTraceDlqRepository::new(pool.clone()),
            ),
        }
    }
}

/// Handles for the background tasks started by [`start_background_tasks`],
/// plus the cluster tasks `run()` adds once `AppState` exists. Owns the
/// abort/join sequence executed on graceful shutdown.
pub(crate) struct TaskHandles {
    trace_persistence_handle: orion::queue::trace_persistence::PersistenceWorkerHandle,
    worker_handle: orion::queue::WorkerHandle,
    trace_cleanup_handle: Option<tokio::task::JoinHandle<()>>,
    audit_cleanup_handle: Option<tokio::task::JoinHandle<()>>,
    dlq_retry_handle: Option<tokio::task::JoinHandle<()>>,
    /// Cluster background tasks (epoch watcher). Empty when disabled.
    /// Populated by `run()` after `AppState` is built.
    pub(crate) cluster_task_handles: Vec<tokio::task::JoinHandle<()>>,
}

impl TaskHandles {
    /// Graceful shutdown: abort the periodic tasks, then drain the trace
    /// queue workers and the persistence queue — same order as before the
    /// extraction.
    pub(crate) async fn shutdown(self) {
        if let Some(handle) = self.trace_cleanup_handle {
            tracing::info!("Stopping trace cleanup task...");
            handle.abort();
        }

        if let Some(handle) = self.audit_cleanup_handle {
            tracing::info!("Stopping audit log cleanup task...");
            handle.abort();
        }

        if let Some(handle) = self.dlq_retry_handle {
            tracing::info!("Stopping DLQ retry consumer...");
            handle.abort();
        }

        for handle in self.cluster_task_handles {
            handle.abort();
        }

        tracing::info!("Shutting down trace queue workers...");
        self.worker_handle.shutdown().await;

        tracing::info!("Draining trace persistence queue...");
        self.trace_persistence_handle.shutdown().await;
    }
}

/// Start the background tasks: trace persistence queue, trace queue worker
/// pool, trace cleanup, audit-log cleanup, and the DLQ retry consumer.
/// Returns the two queues `AppState` needs plus the [`TaskHandles`] owning
/// the shutdown sequence.
pub(crate) fn start_background_tasks(
    config: &config::AppConfig,
    engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    repos: &Repositories,
    channel_registry: Arc<orion::channel::ChannelRegistry>,
    cluster: &orion::cluster::ClusterRuntime,
) -> (
    orion::queue::TracePersistenceQueue,
    orion::queue::TraceQueue,
    TaskHandles,
) {
    // Start trace persistence queue (async/batch modes). A no-op queue is
    // returned for `sync` / `off`, so callers can submit unconditionally.
    let (trace_persistence_queue, trace_persistence_handle) =
        orion::queue::trace_persistence::start(&config.tracing.storage, repos.traces.clone());
    tracing::info!(
        mode = ?config.tracing.storage.mode,
        max_pending = config.tracing.storage.max_pending,
        "Trace persistence queue started"
    );

    // Start trace queue worker pool (with DLQ for failed async traces).
    // The pool needs the persistence queue + channel registry so it can route
    // status / result writes through the configured mode.
    let (trace_queue, worker_handle) = orion::queue::start_workers(
        &config.queue,
        engine,
        repos.traces.clone(),
        Some(repos.trace_dlq.clone()),
        channel_registry.clone(),
        trace_persistence_queue.clone(),
        config.tracing.storage.clone(),
        config.engine.rollout_sticky_header.clone(),
    );

    tracing::info!(
        workers = config.queue.workers,
        buffer = config.queue.buffer_size,
        "Trace queue started"
    );

    // Cluster-mode single-flight gate for background jobs (None on a single node).
    let job_lease_gate = cluster.enabled.then(|| {
        Arc::new(orion::cluster::JobLeaseGate::new(
            cluster.repo.clone(),
            cluster.instance_id.clone(),
        ))
    });

    // Start trace cleanup task
    let trace_cleanup_handle = orion::queue::start_trace_cleanup(
        config.queue.trace_retention_hours,
        config.queue.trace_cleanup_interval_secs,
        repos.traces.clone(),
        job_lease_gate.clone(),
    );

    // Start audit-log cleanup task
    let audit_cleanup_handle = orion::queue::audit_cleanup::start_audit_cleanup(
        config.queue.audit_retention_days,
        config.queue.trace_cleanup_interval_secs,
        repos.audit_logs.clone(),
        job_lease_gate.clone(),
    );

    // Start DLQ retry consumer
    let dlq_retry_handle = if config.queue.dlq_retry_enabled {
        let handle = orion::queue::start_dlq_retry(
            orion::queue::DlqRetryOptions {
                poll_interval_secs: config.queue.dlq_poll_interval_secs,
                batch_size: config.queue.dlq_batch_size,
                lease_secs: config.queue.dlq_lease_secs,
                claimant: cluster.instance_id.clone(),
                lease_gate: job_lease_gate.clone(),
            },
            repos.trace_dlq.clone(),
            trace_queue.clone(),
            repos.traces.clone(),
            channel_registry,
        );
        tracing::info!(
            poll_interval_secs = config.queue.dlq_poll_interval_secs,
            max_retries = config.queue.dlq_max_retries,
            "DLQ retry consumer started"
        );
        Some(handle)
    } else {
        None
    };

    (
        trace_persistence_queue,
        trace_queue,
        TaskHandles {
            trace_persistence_handle,
            worker_handle,
            trace_cleanup_handle,
            audit_cleanup_handle,
            dlq_retry_handle,
            cluster_task_handles: Vec::new(),
        },
    )
}
