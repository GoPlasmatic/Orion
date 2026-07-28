//! Startup-sequence helpers for `run()` in `main.rs` — observability init,
//! repository construction, and background-task lifecycle. `main.rs` stays
//! the readable orchestration script and calls these phases in order.

use std::sync::Arc;

use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::{self, LogFormat};
use crate::connector::ConnectorRegistry;
use crate::storage::repositories::channels::{ChannelRepository, SqlChannelRepository};
use crate::storage::repositories::connectors::{ConnectorRepository, SqlConnectorRepository};
use crate::storage::repositories::traces::{SqlTraceRepository, TraceRepository};
use crate::storage::repositories::workflows::{SqlWorkflowRepository, WorkflowRepository};

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
pub fn init_observability(
    config: &config::AppConfig,
) -> Result<Option<opentelemetry_sdk::trace::SdkTracerProvider>, Box<dyn std::error::Error>> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.logging.level));
    if config.tracing.enabled {
        let (provider, tracer) =
            crate::server::otel::init_otel_pipeline(&config.tracing, &config.cluster.instance_id)?;
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
pub fn init_metrics_handle(
    config: &config::AppConfig,
) -> metrics_exporter_prometheus::PrometheusHandle {
    if config.metrics.enabled {
        let handle = crate::metrics::init_metrics();
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
pub struct Repositories {
    pub workflows: Arc<dyn WorkflowRepository>,
    pub channels: Arc<dyn ChannelRepository>,
    pub connectors: Arc<dyn ConnectorRepository>,
    pub traces: Arc<dyn TraceRepository>,
    pub audit_logs: Arc<dyn crate::storage::repositories::audit_logs::AuditLogRepository>,
    pub trace_dlq: Arc<dyn crate::storage::repositories::trace_dlq::TraceDlqRepository>,
}

impl Repositories {
    /// Create repositories.
    pub fn new(pool: &crate::storage::DbPool) -> Self {
        Self {
            workflows: Arc::new(SqlWorkflowRepository::new(pool.clone())),
            channels: Arc::new(SqlChannelRepository::new(pool.clone())),
            connectors: Arc::new(SqlConnectorRepository::new(pool.clone())),
            traces: Arc::new(SqlTraceRepository::new(pool.clone())),
            audit_logs: Arc::new(
                crate::storage::repositories::audit_logs::SqlAuditLogRepository::new(pool.clone()),
            ),
            trace_dlq: Arc::new(
                crate::storage::repositories::trace_dlq::SqlTraceDlqRepository::new(pool.clone()),
            ),
        }
    }
}

/// Construct the Kafka producer and wire it into the engine's
/// `publish_kafka` handler. Returns `None` when Kafka is disabled or no
/// brokers are configured.
fn setup_kafka_producer(
    kafka_config: &config::KafkaIngestConfig,
    custom_functions: &mut std::collections::HashMap<String, dataflow_rs::BoxedFunctionHandler>,
    connector_registry: Arc<ConnectorRegistry>,
) -> Result<Option<Arc<crate::kafka::producer::KafkaProducer>>, Box<dyn std::error::Error>> {
    if !kafka_config.enabled || kafka_config.brokers.is_empty() {
        return Ok(None);
    }
    let producer = Arc::new(crate::kafka::producer::KafkaProducer::new(
        &kafka_config.brokers.join(","),
        &kafka_config.auth,
        &kafka_config.extra_config,
    )?);
    // F13: per-connector producers resolve through this cache; the global
    // brokers map back to the producer created here.
    let producers = Arc::new(crate::kafka::producer::KafkaProducerCache::new(
        kafka_config.brokers.join(","),
        producer.clone(),
        kafka_config.auth.clone(),
        kafka_config.extra_config.clone(),
    ));
    crate::engine::register_kafka_publisher(custom_functions, connector_registry, producers);
    tracing::info!("Kafka producer initialized");
    Ok(Some(producer))
}

/// The engine's serving components, built in one pass by
/// [`build_engine_components`]: connector registry (with the F16 fail-fast
/// check), shared HTTP client, datalogic engine, the pre-created engine
/// lock, cache + external connector pool caches, the custom function
/// handlers, and the Kafka producer.
pub struct EngineComponents {
    pub connector_registry: Arc<ConnectorRegistry>,
    pub http_client: reqwest::Client,
    pub datalogic: Arc<datalogic_rs::Engine>,
    pub engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    pub cache_pool: Arc<crate::connector::cache_backend::CachePool>,
    pub sql_pool_cache: Arc<crate::connector::pool_cache::SqlPoolCache>,
    pub mongo_pool_cache: Arc<crate::connector::mongo_pool::MongoPoolCache>,
    pub custom_functions: std::collections::HashMap<String, dataflow_rs::BoxedFunctionHandler>,
    pub kafka_producer: Option<Arc<crate::kafka::producer::KafkaProducer>>,
}

/// Build the [`EngineComponents`]: load the connector registry, create the
/// shared HTTP client, the datalogic engine, the engine lock (pre-created so
/// the `channel_call` handler can reference it), the cache + external pool
/// caches, the custom function handlers, and the Kafka producer.
pub async fn build_engine_components(
    config: &config::AppConfig,
    repos: &Repositories,
    channel_registry: Arc<crate::channel::ChannelRegistry>,
) -> Result<EngineComponents, Box<dyn std::error::Error>> {
    // Load connectors
    let connector_registry = Arc::new(ConnectorRegistry::new(
        config.engine.circuit_breaker.clone(),
    ));
    let connector_count = connector_registry
        .load_from_repo(repos.connectors.as_ref())
        .await?;
    tracing::info!(count = connector_count, "Connectors loaded");

    // F16: an enabled connector that fails to load is absent from the
    // registry, so the failure surfaces as a 500 on the first request that
    // needs it — possibly hours after the deploy that caused it. Operators who
    // would rather have the rollout fail at boot opt in here.
    let connector_issues = connector_registry.load_issues().await;
    if !connector_issues.is_empty() && config.engine.fail_on_connector_load_error {
        let detail = connector_issues
            .iter()
            .map(|i| format!("{} ({}): {}", i.connector, i.stage, i.reason))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(crate::errors::OrionError::Config {
            message: format!(
                "refused to start: {} enabled connector(s) failed to load: {detail}. \
                 Set engine.fail_on_connector_load_error = false to start anyway \
                 (they will fail at request time instead).",
                connector_issues.len()
            ),
        }
        .into());
    }

    // Create a shared HTTP client. Redirects are off: execute_request follows
    // them manually with per-hop SSRF validation. The pinned resolver connects
    // to the exact addresses SSRF validation vetted (no DNS rebinding between
    // check and connect).
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            config.engine.global_http_timeout_secs,
        ))
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(std::sync::Arc::new(crate::validation::PinnedDnsResolver))
        .build()
        .map_err(|e| {
            crate::errors::OrionError::Internal(format!("Failed to build HTTP client: {e}"))
        })?;

    // Shared datalogic engine — used by handlers for template evaluation and
    // by the channel registry to pre-compile per-channel JSONLogic.
    let datalogic_engine = Arc::new(datalogic_rs::Engine::new());

    // Create the engine lock early so channel_call handler can reference it.
    // We'll populate it with the real engine after building workflows.
    let engine: Arc<RwLock<Arc<dataflow_rs::Engine>>> = Arc::new(RwLock::new(Arc::new(
        dataflow_rs::Engine::builder().build()?,
    )));

    // Build cache pool (memory backend always available, redis always compiled)
    let cache_pool = Arc::new(crate::connector::cache_backend::CachePool::new(
        config.engine.max_pool_cache_entries,
        config.engine.cache_cleanup_interval_secs,
        config.engine.max_memory_cache_entries,
    ));

    // Create external connector pool caches (shared with AppState for eviction on update/delete)
    let sql_pool_cache = Arc::new(crate::connector::pool_cache::SqlPoolCache::new(
        config.engine.max_pool_cache_entries,
    ));
    let mongo_pool_cache = Arc::new(crate::connector::mongo_pool::MongoPoolCache::new(
        config.engine.max_pool_cache_entries,
    ));

    // Build custom function handlers (http_call, channel_call, cache_read, cache_write, etc.)
    let mut custom_functions = crate::engine::build_custom_functions(
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

    let kafka_producer = setup_kafka_producer(
        &config.kafka,
        &mut custom_functions,
        connector_registry.clone(),
    )?;

    Ok(EngineComponents {
        connector_registry,
        http_client,
        datalogic: datalogic_engine,
        engine,
        cache_pool,
        sql_pool_cache,
        mongo_pool_cache,
        custom_functions,
        kafka_producer,
    })
}

impl EngineComponents {
    /// Load active channels and workflows, build the engine's workflow set,
    /// reload the channel registry (quarantining channels that fail to
    /// load), and populate the pre-created engine lock. Returns the loaded
    /// channels (the Kafka topic merge needs them) and the active-workflow
    /// count (for the gauge).
    pub async fn load_channels_and_build_engine(
        &mut self,
        config: &config::AppConfig,
        repos: &Repositories,
        channel_registry: &crate::channel::ChannelRegistry,
    ) -> Result<(Vec<crate::storage::models::Channel>, usize), Box<dyn std::error::Error>> {
        // Load active channels and workflows, build engine
        let channels = repos.channels.list_active().await?;
        let total_active = channels.len();
        let channels = crate::engine::filter_channels(channels, &config.channels);
        // F32: a wrong include/exclude pattern silently drops a channel; the
        // resolved list makes the filter's effect visible at boot.
        if !config.channels.include.is_empty() || !config.channels.exclude.is_empty() {
            tracing::info!(
                resolved = ?channels.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
                filtered_out = total_active - channels.len(),
                "Channel include/exclude filters applied"
            );
        }
        let active_workflows = repos.workflows.list_active().await?;
        let (workflows, engine_issues) =
            crate::engine::build_engine_workflows(&channels, &active_workflows);
        let load_issues = channel_registry
            .reload(
                &channels,
                &self.connector_registry,
                &self.cache_pool,
                &self.datalogic,
                &config.tracing.storage,
                engine_issues,
            )
            .await;
        if !load_issues.is_empty() {
            // A channel whose stored config or validation_logic no longer loads
            // (any mode), or whose shared backend cannot be built (cluster mode),
            // must never be served unguarded. It is quarantined: absent from the
            // registry and the route table, and refused at every ingress with a
            // 503. Booting anyway is the F35 change — the alternative was that one
            // broken row stopped the whole instance, including every channel that
            // is fine.
            for issue in &load_issues {
                tracing::error!(
                    channel = %issue.channel,
                    reason = %issue.reason,
                    "Channel quarantined: it will be refused at every ingress until fixed"
                );
            }
        }

        let channel_names: std::collections::HashSet<&str> =
            workflows.iter().map(|w| w.channel.as_str()).collect();

        tracing::info!(
            workflows = active_workflows.len(),
            channels = channel_names.len(),
            "Workflows loaded"
        );

        // Populate the pre-created engine lock with the real engine
        let built_engine =
            dataflow_rs::Engine::new(workflows, std::mem::take(&mut self.custom_functions))?;
        *crate::engine::acquire_engine_write(&self.engine).await = Arc::new(built_engine);

        Ok((channels, active_workflows.len()))
    }
}

/// Start the Kafka consumer in a background task. Merges config-file topic
/// mappings with DB-driven async-channel topics. Returns `None` when Kafka
/// is disabled or the merged topic list is empty.
pub fn start_kafka_ingest(
    kafka_config: &config::KafkaIngestConfig,
    channels: &[crate::storage::models::Channel],
    engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    channel_registry: Arc<crate::channel::ChannelRegistry>,
    datalogic: Arc<datalogic_rs::Engine>,
    kafka_producer: Option<Arc<crate::kafka::producer::KafkaProducer>>,
    instance_id: Option<&str>,
) -> Result<Option<crate::kafka::consumer::ConsumerHandle>, Box<dyn std::error::Error>> {
    if !kafka_config.enabled {
        return Ok(None);
    }

    let all_topics = crate::kafka::merge_kafka_topics(kafka_config, channels);

    if all_topics.is_empty() {
        return Ok(None);
    }

    let merged_config = crate::config::KafkaIngestConfig {
        topics: all_topics,
        ..kafka_config.clone()
    };

    let (dlq_producer, dlq_topic) = if kafka_config.dlq.enabled {
        (kafka_producer, Some(kafka_config.dlq.topic.clone()))
    } else {
        (None, None)
    };

    let handle = crate::kafka::consumer::start_consumer(
        &merged_config,
        engine,
        channel_registry,
        datalogic,
        dlq_producer,
        dlq_topic,
        instance_id,
    )?;

    tracing::info!(
        config_topics = kafka_config.topics.len(),
        db_topics = merged_config.topics.len() - kafka_config.topics.len(),
        total_topics = merged_config.topics.len(),
        group_id = %kafka_config.group_id,
        "Kafka consumer started"
    );

    Ok(Some(handle))
}

/// Build rate limiter (if enabled).
pub fn build_rate_limit_state(
    config: &config::AppConfig,
) -> Option<Arc<crate::server::rate_limit::RateLimitState>> {
    if config.rate_limit.enabled {
        let rls = crate::server::rate_limit::RateLimitState::from_config(&config.rate_limit);
        tracing::info!(
            default_rps = config.rate_limit.default_rps,
            default_burst = config.rate_limit.default_burst,
            "Rate limiting enabled"
        );
        Some(Arc::new(rls))
    } else {
        None
    }
}

/// Handles for the background tasks started by [`start_background_tasks`],
/// plus the cluster tasks `run()` adds once `AppState` exists. Owns the
/// abort/join sequence executed on graceful shutdown.
pub struct TaskHandles {
    trace_persistence_handle: crate::queue::trace_persistence::PersistenceWorkerHandle,
    worker_handle: crate::queue::WorkerHandle,
    trace_cleanup_handle: Option<tokio::task::JoinHandle<()>>,
    audit_cleanup_handle: Option<tokio::task::JoinHandle<()>>,
    dlq_retry_handle: Option<tokio::task::JoinHandle<()>>,
    /// Cluster background tasks (epoch watcher). Empty when disabled.
    /// Populated by `run()` after `AppState` is built.
    pub cluster_task_handles: Vec<tokio::task::JoinHandle<()>>,
}

impl TaskHandles {
    /// Graceful shutdown: abort the periodic tasks, then drain the trace
    /// queue workers and the persistence queue — same order as before the
    /// extraction.
    pub async fn shutdown(self) {
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
pub fn start_background_tasks(
    config: &config::AppConfig,
    engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    repos: &Repositories,
    channel_registry: Arc<crate::channel::ChannelRegistry>,
    cluster: &crate::cluster::ClusterRuntime,
) -> (
    crate::queue::TracePersistenceQueue,
    crate::queue::TraceQueue,
    TaskHandles,
) {
    // Start trace persistence queue (async/batch modes). A no-op queue is
    // returned for `sync` / `off`, so callers can submit unconditionally.
    let (trace_persistence_queue, trace_persistence_handle) =
        crate::queue::trace_persistence::start(&config.tracing.storage, repos.traces.clone());
    tracing::info!(
        mode = ?config.tracing.storage.mode,
        max_pending = config.tracing.storage.max_pending,
        "Trace persistence queue started"
    );

    // Start trace queue worker pool (with DLQ for failed async traces).
    // The pool needs the persistence queue + channel registry so it can route
    // status / result writes through the configured mode.
    let (trace_queue, worker_handle) = crate::queue::start_workers(
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
        Arc::new(crate::cluster::JobLeaseGate::new(
            cluster.repo.clone(),
            cluster.instance_id.clone(),
        ))
    });

    // Start trace cleanup task
    let trace_cleanup_handle = crate::queue::start_trace_cleanup(
        config.queue.trace_retention_hours,
        config.queue.trace_cleanup_interval_secs,
        repos.traces.clone(),
        job_lease_gate.clone(),
    );

    // Start audit-log cleanup task
    let audit_cleanup_handle = crate::queue::audit_cleanup::start_audit_cleanup(
        config.queue.audit_retention_days,
        config.queue.trace_cleanup_interval_secs,
        repos.audit_logs.clone(),
        job_lease_gate.clone(),
    );

    // Start DLQ retry consumer
    let dlq_retry_handle = if config.queue.dlq_retry_enabled {
        let handle = crate::queue::start_dlq_retry(
            crate::queue::DlqRetryOptions {
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
