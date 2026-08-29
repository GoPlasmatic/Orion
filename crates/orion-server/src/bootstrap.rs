//! Startup-sequence helpers for `run()` in `main.rs` — observability init,
//! repository construction, and background-task lifecycle. `main.rs` stays
//! the readable orchestration script and calls these phases in order.

use dataflow_rs::datalogic_rs;
use std::sync::Arc;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::{self, LogFormat};
use crate::connector::ConnectorRegistry;

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
        // Label every metric with this node's identity in cluster mode, so a
        // scrape target set that changes under you (rolling deploy, HPA) still
        // attributes series to the right replica.
        let instance = config
            .cluster
            .enabled
            .then_some(config.cluster.instance_id.as_str())
            .filter(|id| !id.is_empty());
        let handle = crate::metrics::init_metrics_with_instance(instance);
        crate::metrics::record_build_info();
        tracing::info!("Prometheus metrics initialized");
        handle
    } else {
        // Create a no-op handle that still works but doesn't install a global recorder
        metrics_exporter_prometheus::PrometheusBuilder::new()
            .build_recorder()
            .handle()
    }
}

/// The repository set backing `AppState` and the background tasks. Lives in
/// `storage::repositories` since R26 (it is also the `repos` group on
/// `AppStateInner`); re-exported here so bootstrap callers keep their path.
pub use crate::storage::repositories::Repositories;

/// Construct the Kafka producer and wire it into the engine's
/// `publish_kafka` handler. Returns `None` when Kafka is disabled or no
/// brokers are configured.
fn setup_kafka_producer(
    kafka_config: &config::KafkaIngestConfig,
    custom_functions: &mut std::collections::HashMap<String, dataflow_rs::BoxedFunctionHandler>,
    connector_registry: Arc<ConnectorRegistry>,
    max_pool_cache_entries: usize,
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
        max_pool_cache_entries,
    ));
    crate::engine::register_kafka_publisher(custom_functions, connector_registry, producers);
    tracing::info!("Kafka producer initialized");
    Ok(Some(producer))
}

/// The long-lived half of [`EngineComponents`] — everything that outlives
/// engine construction and goes on to back `AppState`.
///
/// F55: this exists so the startup ordering is a *type* error rather than a
/// convention. Building the engine consumes the custom-function handlers
/// (dataflow-rs takes the map by value), so the step is destructive; the only
/// way to obtain a `ServingComponents` is to have run it.
pub struct ServingComponents {
    pub connector_registry: Arc<ConnectorRegistry>,
    pub http_client: reqwest::Client,
    pub datalogic: Arc<datalogic_rs::Engine>,
    /// The JWKS cache, built on `http_client` so key fetches ride the pinned
    /// resolver and the shared connection pool.
    pub jwks: Arc<crate::jwt::jwks::JwksCache>,
    /// `[secrets]`, resolved once. Every engine built from here on carries it,
    /// including the ones the admin plane builds per request — a surface that
    /// built its engine without it would refuse a workflow the serving engine
    /// runs.
    pub secrets: Arc<crate::engine::ResolvedSecrets>,
    /// `[vars]` as one JSON object, or `None` when the instance declares none.
    ///
    /// Derived once, here, and cloned to every consumer — `AppState`, the
    /// Kafka consumer, and the consumer restart on reload. The "`None` strips
    /// the key rather than stamping `{}`" convention that makes `metadata.vars`
    /// unforgeable lives in `VarsConfig::to_json`, and re-deriving it per
    /// consumer is how two ingresses come to disagree about what a workflow
    /// reads.
    pub vars: Option<Arc<serde_json::Value>>,
    pub engine: Arc<crate::engine::EngineHandle>,
    pub cache_pool: Arc<crate::connector::cache_backend::CachePool>,
    pub sql_pool_cache: Arc<crate::connector::pool_cache::SqlPoolCache>,
    pub mongo_pool_cache: Arc<crate::connector::mongo_pool::MongoPoolCache>,
    pub smtp_pool_cache: Arc<crate::connector::smtp_pool::SmtpPoolCache>,
    pub kafka_producer: Option<Arc<crate::kafka::producer::KafkaProducer>>,
}

/// The engine's serving components, built in one pass by
/// [`build_engine_components`]: connector registry (with the F16 fail-fast
/// check), shared HTTP client, datalogic engine, the pre-created engine
/// lock, cache + external connector pool caches, the custom function
/// handlers, and the Kafka producer.
pub struct EngineComponents {
    pub serving: ServingComponents,
    pub custom_functions: std::collections::HashMap<String, dataflow_rs::BoxedFunctionHandler>,
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
        .map_err(|e| crate::errors::OrionError::internal_from("Failed to build HTTP client", e))?;

    // One JWKS cache per instance, on the pinned client. `jwks_url` is
    // authored input, so this is the one egress path with no operator-chosen
    // connector behind it; `jwt.allow_private_jwks_urls` is its equivalent of
    // a connector's `allow_private_urls`.
    let jwks = Arc::new(crate::jwt::jwks::JwksCache::new(
        http_client.clone(),
        config.jwt.allow_private_jwks_urls,
    ));

    // Shared datalogic engine — used by handlers for template evaluation and
    // by the channel registry to pre-compile per-channel JSONLogic. Carries
    // Orion's custom operators so channel-level expressions speak the same
    // vocabulary as workflow logic.
    let datalogic_engine = Arc::new(
        crate::engine::operators::add_to_datalogic(datalogic_rs::Engine::builder()).build(),
    );

    // Resolve `[secrets]` before anything builds an engine. A reference that
    // cannot be resolved stops the boot: handing one onward as its own literal
    // text is the failure the reference syntax exists to prevent, and an
    // instance serving a workflow whose signing key is the string
    // `env://PARTNER_HMAC_KEY` fails at the remote system with nothing
    // pointing back here.
    let secrets = Arc::new(crate::engine::ResolvedSecrets::resolve(&config.secrets).await?);
    let vars = config.vars.to_json().map(Arc::new);
    if !secrets.is_empty() {
        tracing::info!(
            names = ?secrets.names().collect::<Vec<_>>(),
            "Secrets resolved"
        );
    }

    // Create the engine lock early so channel_call handler can reference it.
    // We'll populate it with the real engine after building workflows.
    let engine: Arc<crate::engine::EngineHandle> =
        Arc::new(crate::engine::EngineHandle::new(Arc::new(
            crate::engine::operators::with_orion_engine_defaults(
                dataflow_rs::Engine::builder(),
                &secrets,
            )
            .build()?,
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
    let smtp_pool_cache = Arc::new(crate::connector::smtp_pool::SmtpPoolCache::new(
        config.engine.max_pool_cache_entries,
    ));

    // Build custom function handlers (http_call, channel_call, cache_read, cache_write, etc.)
    let mut custom_functions = crate::engine::build_custom_functions(crate::engine::HandlerDeps {
        registry: connector_registry.clone(),
        client: http_client.clone(),
        engine: engine.clone(),
        channel_registry: channel_registry.clone(),
        jwks: jwks.clone(),
        engine_config: &config.engine,
        query_config: &config.query,
        write_config: &config.write,
        cache_pool: cache_pool.clone(),
        sql_pool_cache: sql_pool_cache.clone(),
        mongo_pool_cache: mongo_pool_cache.clone(),
        smtp_pool_cache: smtp_pool_cache.clone(),
    });

    let kafka_producer = setup_kafka_producer(
        &config.kafka,
        &mut custom_functions,
        connector_registry.clone(),
        config.engine.max_pool_cache_entries,
    )?;

    Ok(EngineComponents {
        serving: ServingComponents {
            connector_registry,
            http_client,
            datalogic: datalogic_engine,
            jwks,
            secrets,
            vars,
            engine,
            cache_pool,
            sql_pool_cache,
            mongo_pool_cache,
            smtp_pool_cache,
            kafka_producer,
        },
        custom_functions,
    })
}

impl EngineComponents {
    /// Load active channels and workflows, build the engine's workflow set,
    /// reload the channel registry (quarantining channels that fail to
    /// load), and populate the pre-created engine lock. Returns the
    /// [`ServingComponents`] `AppState` is assembled from, the loaded channels
    /// (the Kafka topic merge needs them) and the active-workflow count (for
    /// the gauge).
    ///
    /// F55: takes `self` by value. This step *consumes* the custom-function
    /// handlers, so when it took `&mut self` the map was left as an empty hole
    /// that `build_app_state` had to know to ignore — and calling
    /// `build_app_state` first compiled fine and silently produced an engine
    /// with no Orion handlers registered at all. Consuming the value makes that
    /// order a compile error instead.
    pub async fn load_channels_and_build_engine(
        self,
        config: &config::AppConfig,
        repos: &Repositories,
        channel_registry: &crate::channel::ChannelRegistry,
    ) -> Result<
        (
            ServingComponents,
            Vec<crate::storage::models::Channel>,
            usize,
        ),
        Box<dyn std::error::Error>,
    > {
        let EngineComponents {
            serving,
            custom_functions,
        } = self;
        // Load active channels and workflows, build engine
        let channels = repos.channels.list_active().await?;
        let total_active = channels.len();
        let channels = crate::engine::filter_channels(channels, &config.channel_filter);
        // F32: a wrong include/exclude pattern silently drops a channel; the
        // resolved list makes the filter's effect visible at boot.
        if !config.channel_filter.include.is_empty() || !config.channel_filter.exclude.is_empty() {
            tracing::info!(
                resolved = ?channels.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
                filtered_out = total_active - channels.len(),
                "Channel include/exclude filters applied"
            );
        }
        let active_workflows = repos.workflows.list_active().await?;

        // The handlers go on the builder *before* the workflows are converted,
        // because converting screens each one against them: a workflow naming
        // a function nothing will dispatch, or carrying an input its handler
        // cannot parse, is quarantined per channel instead of aborting the
        // whole build. `with_handlers` is the only thing this borrow is for —
        // the workflows are added below, on the same builder.
        let builder = crate::engine::operators::with_orion_engine_defaults(
            dataflow_rs::Engine::builder(),
            &serving.secrets,
        )
        .with_handlers(custom_functions);
        let (workflows, engine_issues) =
            crate::engine::build_engine_workflows(&channels, &active_workflows, &builder);
        channel_registry
            .reload(
                &channels,
                crate::channel::ReloadDeps {
                    connector_registry: &serving.connector_registry,
                    cache_pool: &serving.cache_pool,
                    datalogic: &serving.datalogic,
                    jwks: &serving.jwks,
                    global_trace_storage: &config.trace_storage,
                },
                engine_issues,
            )
            .await;
        // A channel whose stored config or validation_logic no longer loads
        // (any mode), or whose shared backend cannot be built (cluster mode),
        // must never be served unguarded. It is quarantined: absent from the
        // registry and the route table, and refused at every ingress with a
        // 503. Booting anyway is the F35 change — the alternative was that one
        // broken row stopped the whole instance, including every channel that
        // is fine.
        //
        // N21: read from the registry rather than from a return value.
        // `reload` used to hand the same list back, so the quarantine set had
        // two representations and `/health` and this log could disagree.
        for issue in channel_registry.quarantined() {
            tracing::error!(
                channel = %issue.channel,
                reason = %issue.reason,
                "Channel quarantined: it will be refused at every ingress until fixed"
            );
        }

        let channel_names: std::collections::HashSet<&str> =
            workflows.iter().map(|w| w.channel.as_str()).collect();

        tracing::info!(
            workflows = active_workflows.len(),
            channels = channel_names.len(),
            "Workflows loaded"
        );

        // Populate the pre-created engine lock with the real engine.
        //
        // The observer is attached here rather than on the placeholder at
        // startup because `Engine::new` builds a fresh engine; `with_new_workflows`
        // carries it across every subsequent reload, so this is the only place
        // it needs setting.
        let built_engine = builder
            .with_workflows(workflows)
            .build()?
            .with_observer(Arc::new(crate::engine::MetricsObserver));
        serving.engine.store(Arc::new(built_engine));

        Ok((serving, channels, active_workflows.len()))
    }
}

/// What the Kafka ingest needs from the running instance.
///
/// Named fields rather than a positional list because three of the six are
/// `Option` and the boot, test-harness and reload-restart paths all build the
/// same set — `state.engine`, `state.datalogic`, `state.vars`,
/// `state.kafka.producer` on the reload side, and the matching
/// [`ServingComponents`] fields on the boot side. A bare `None` in the fifth
/// position tells a reader nothing about which dependency is absent.
pub struct IngestDeps {
    pub engine: Arc<crate::engine::EngineHandle>,
    pub channel_registry: Arc<crate::channel::ChannelRegistry>,
    pub datalogic: Arc<datalogic_rs::Engine>,
    /// `[vars]` as one JSON object, stamped into every ingested message's
    /// `metadata.vars`. `None` when the instance declares none.
    pub vars: Option<Arc<serde_json::Value>>,
    /// The producer the DLQ writes through, when `kafka.dlq.enabled`.
    pub kafka_producer: Option<Arc<crate::kafka::producer::KafkaProducer>>,
    /// Cluster mode's static group membership id; `None` on a single node.
    pub instance_id: Option<String>,
    /// Where a completed Kafka message's trace row is written, and the cap on
    /// its serialized result. Kafka messages went untraced until the
    /// trace-plan/serialize/route sequence became one shared function
    /// (`queue::trace_record`).
    pub trace_repo: Arc<dyn crate::storage::repositories::traces::TraceSink>,
    pub persistence_queue: crate::queue::TracePersistenceQueue,
    pub max_result_size_bytes: usize,
}

/// Start the Kafka consumer in a background task. Merges config-file topic
/// mappings with DB-driven async-channel topics. Returns `None` when Kafka
/// is disabled or the merged topic list is empty.
pub fn start_kafka_ingest(
    kafka_config: &config::KafkaIngestConfig,
    channels: &[crate::storage::models::Channel],
    deps: IngestDeps,
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

    let IngestDeps {
        engine,
        channel_registry,
        datalogic,
        vars,
        kafka_producer,
        instance_id,
        trace_repo,
        persistence_queue,
        max_result_size_bytes,
    } = deps;
    let (dlq_producer, dlq_topic) = if kafka_config.dlq.enabled {
        (kafka_producer, Some(kafka_config.dlq.topic.clone()))
    } else {
        (None, None)
    };

    let handle = crate::kafka::consumer::start_consumer(
        &merged_config,
        crate::kafka::consumer::ConsumerDeps {
            engine,
            channel_registry,
            datalogic,
            vars,
            dlq_producer,
            dlq_topic,
            instance_id,
            trace_repo,
            persistence_queue,
            max_result_size_bytes,
        },
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

/// O12: optional dedicated metrics listener. Bound *before* the main
/// server starts, so an address clash or a permission problem is a startup
/// failure rather than a silently missing scrape target. Its shutdown
/// future is an independent `shutdown_signal()` — signal handlers fan out
/// to every registered stream, so both listeners see the same SIGTERM.
pub fn start_metrics_listener(
    config: &Arc<config::AppConfig>,
    state: &crate::server::state::AppState,
) -> Result<
    Option<tokio::task::JoinHandle<Result<(), crate::errors::OrionError>>>,
    crate::errors::OrionError,
> {
    match config.metrics.dedicated_bind_addr() {
        Some(addr) => {
            let listener = crate::server::serve::create_tcp_listener(addr)?;
            if !listener.local_addr().is_ok_and(|a| a.ip().is_loopback()) {
                tracing::warn!(
                    address = %addr,
                    "metrics.bind_addr is not a loopback address and the metrics listener is \
                     unauthenticated — make sure it is reachable only from your scrapers"
                );
            }
            Ok(Some(tokio::spawn(crate::server::serve::serve_metrics(
                listener,
                config.clone(),
                crate::server::metrics_router(state.clone()),
                crate::server::shutdown_signal(),
            ))))
        }
        None => {
            // O12 in reverse. `bind_addr` set with collection off raises no
            // listener *and* keeps `/metrics` off the main router, so the
            // endpoint exists nowhere — a values file that sets the address
            // but forgets `ORION_METRICS__ENABLED=true` (the default is
            // `false`) yields a silently metric-less deployment. Not a config
            // error: charts legitimately template the address and gate on
            // `enabled`. But it must not be silent.
            if let Some(addr) = config.metrics.bind_addr.as_deref() {
                tracing::warn!(
                    address = %addr,
                    "metrics.bind_addr is set but metrics.enabled is false — no metrics \
                     listener was started and /metrics is served nowhere. Set \
                     metrics.enabled = true (ORION_METRICS__ENABLED=true), or remove \
                     metrics.bind_addr"
                );
            }
            Ok(None)
        }
    }
}

/// Join the metrics listener started by [`start_metrics_listener`].
///
/// The metrics listener drains on the same grace window, so by the time the
/// main server has returned it is at most a scheduling hop behind. Bound
/// the join anyway — a stuck scrape must not hold the process open.
pub async fn join_metrics_listener(
    handle: Option<tokio::task::JoinHandle<Result<(), crate::errors::OrionError>>>,
) {
    if let Some(handle) = handle {
        match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
            Ok(Ok(Err(e))) => tracing::warn!(error = %e, "Metrics listener exited with an error"),
            Ok(Err(e)) => tracing::warn!(error = %e, "Metrics listener task panicked"),
            Ok(Ok(Ok(()))) => tracing::info!("Metrics listener stopped"),
            Err(_) => tracing::warn!("Metrics listener did not stop within 5s; abandoning it"),
        }
    }
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
/// The three queue drains, in the order they must run.
///
/// Only the queue consumers are here. The periodic jobs (trace cleanup, audit
/// cleanup, DLQ retry) and the cluster epoch watcher used to be `JoinHandle`s
/// in this struct that shutdown `abort()`ed; they belong to
/// [`crate::runtime::TaskRegistry`] now, which stops them cooperatively and —
/// the point of the move — reports them to `/health` and `/readyz` while they
/// are running.
///
/// **Why these three did not move.** Each owns the receiving end of an `mpsc`
/// channel and stops when the last sender drops, not on a signal; and the
/// order below is load-bearing — the worker pool submits to the persistence
/// queue, so persistence must drain after the workers finish, and the audit
/// writer last because an admin mutation accepted moments before SIGTERM still
/// has a row to write. A registry joining its set concurrently under one
/// deadline cannot express that. They register a
/// [`crate::runtime::TaskGuard`] instead, so their liveness is reported
/// without their shutdown being taken over.
pub struct TaskHandles {
    trace_persistence_handle: crate::queue::trace_persistence::PersistenceWorkerHandle,
    worker_handle: crate::queue::WorkerHandle,
    audit_writer_handle: crate::queue::audit_queue::AuditWriterHandle,
}

impl TaskHandles {
    /// Graceful shutdown: drain the trace queue workers, then the persistence
    /// queue, then the audit writer.
    pub async fn shutdown(self) {
        tracing::info!("Shutting down trace queue workers...");
        self.worker_handle.shutdown().await;

        tracing::info!("Draining trace persistence queue...");
        self.trace_persistence_handle.shutdown().await;

        // O7: last, and bounded. The caller has already dropped `AppState`
        // (and with it the last `AuditQueue` sender), so the writer sees the
        // channel close, finishes what it holds, and exits.
        self.audit_writer_handle.shutdown().await;
    }
}

/// Start the background tasks: trace persistence queue, trace queue worker
/// pool, trace cleanup, audit-log cleanup, and the DLQ retry consumer.
/// Returns the two queues `AppState` needs plus the [`TaskHandles`] owning
/// the shutdown sequence.
pub fn start_background_tasks(
    config: &config::AppConfig,
    tasks: &crate::runtime::TaskRegistry,
    engine: Arc<crate::engine::EngineHandle>,
    repos: &Repositories,
    channel_registry: Arc<crate::channel::ChannelRegistry>,
    cluster: &crate::cluster::ClusterRuntime,
) -> (
    crate::queue::TracePersistenceQueue,
    crate::queue::TraceQueue,
    crate::queue::audit_queue::AuditQueue,
    TaskHandles,
) {
    // Audit writer (O7): one bounded queue, one in-order writer, drained at
    // shutdown. Started first so no admin mutation can be accepted before
    // there is somewhere to record it.
    let (audit_queue, audit_writer_handle) =
        crate::queue::audit_queue::start(tasks, &config.audit, repos.audit_logs.clone());
    tracing::info!(
        max_pending = config.audit.max_pending,
        drain_timeout_secs = config.audit.drain_timeout_secs,
        "Audit-log writer started"
    );

    // Start trace persistence queue (async/batch modes). A no-op queue is
    // returned for `sync` / `off`, so callers can submit unconditionally.
    let (trace_persistence_queue, trace_persistence_handle) =
        crate::queue::trace_persistence::start(tasks, &config.trace_storage, repos.traces.clone());
    tracing::info!(
        mode = ?config.trace_storage.mode,
        max_pending = config.trace_storage.max_pending,
        "Trace persistence queue started"
    );

    // Start trace queue worker pool (with DLQ for failed async traces).
    // The pool needs the persistence queue + channel registry so it can route
    // status / result writes through the configured mode.
    let (trace_queue, worker_handle) = crate::queue::start_workers(
        tasks,
        &config.trace_queue,
        crate::queue::WorkerDeps {
            engine,
            trace_repo: repos.traces.clone(),
            dlq_repo: Some(repos.trace_dlq.clone()),
            channel_registry: channel_registry.clone(),
            persistence_queue: trace_persistence_queue.clone(),
            global_trace_storage: config.trace_storage.clone(),
            rollout_sticky_header: config.engine.rollout_sticky_header.clone(),
        },
    );

    tracing::info!(
        workers = config.trace_queue.workers,
        buffer = config.trace_queue.buffer_size,
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
    crate::queue::start_trace_cleanup(
        tasks,
        config.trace_queue.retention_hours,
        config.trace_queue.cleanup_interval_secs,
        repos.traces.clone(),
        job_lease_gate.clone(),
    );

    // Start audit-log cleanup task
    crate::queue::audit_cleanup::start_audit_cleanup(
        tasks,
        config.audit.retention_days,
        config.audit.cleanup_interval_secs,
        repos.audit_logs.clone(),
        job_lease_gate.clone(),
    );

    // Start DLQ retry consumer
    if config.trace_queue.dlq_retry_enabled {
        crate::queue::start_dlq_retry(
            tasks,
            crate::queue::DlqRetryOptions {
                poll_interval_secs: config.trace_queue.dlq_poll_interval_secs,
                batch_size: config.trace_queue.dlq_batch_size,
                lease_secs: config.trace_queue.dlq_lease_secs,
                claimant: cluster.instance_id.clone(),
                lease_gate: job_lease_gate.clone(),
            },
            repos.trace_dlq.clone(),
            trace_queue.clone(),
            repos.traces.clone(),
            channel_registry,
        );
        tracing::info!(
            poll_interval_secs = config.trace_queue.dlq_poll_interval_secs,
            max_retries = config.trace_queue.dlq_max_retries,
            "DLQ retry consumer started"
        );
    }

    (
        trace_persistence_queue,
        trace_queue,
        audit_queue,
        TaskHandles {
            trace_persistence_handle,
            worker_handle,
            audit_writer_handle,
        },
    )
}

/// Inputs to [`build_app_state`] that aren't already carried by
/// [`Repositories`] / [`EngineComponents`].
pub struct AppStateParams {
    pub config: Arc<config::AppConfig>,
    pub pool: crate::storage::DbPool,
    pub repos: Repositories,
    pub components: ServingComponents,
    pub channel_registry: Arc<crate::channel::ChannelRegistry>,
    pub trace_queue: crate::queue::TraceQueue,
    pub trace_persistence_queue: crate::queue::TracePersistenceQueue,
    pub audit_queue: crate::queue::audit_queue::AuditQueue,
    pub rate_limit_state: Option<Arc<crate::server::rate_limit::RateLimitState>>,
    pub metrics_handle: metrics_exporter_prometheus::PrometheusHandle,
    pub ready: Arc<std::sync::atomic::AtomicBool>,
    pub kafka_consumer_handle: Option<crate::kafka::consumer::ConsumerHandle>,
    pub cluster: Arc<crate::cluster::ClusterRuntime>,
    /// The supervisor the background tasks registered with, so the probes can
    /// read their liveness.
    pub tasks: Arc<crate::runtime::TaskRegistry>,
}

/// Assemble `AppState` from the bootstrap products — the single place the
/// [`Repositories`] / [`ServingComponents`] fields map onto `AppStateInner`,
/// shared by `main.rs` and the integration-test harness so the two can never
/// drift apart.
pub fn build_app_state(params: AppStateParams) -> crate::server::state::AppState {
    let AppStateParams {
        config,
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
        tasks,
    } = params;
    let ServingComponents {
        connector_registry,
        http_client,
        datalogic,
        jwks,
        secrets,
        vars,
        engine,
        cache_pool,
        sql_pool_cache,
        mongo_pool_cache,
        smtp_pool_cache,
        kafka_producer,
    } = components;
    // Parsed once, unconditionally — not from `rate_limit_state`. Three
    // callers need it whether or not the platform limiter is enabled: the
    // audit trail (O7), the failed-auth backoff, and the per-channel rate
    // limit, which applies with the platform limiter off (S15) and keys on
    // the same client identity. See `AppStateInner::trusted_proxies`.
    let trusted_proxies = Arc::new(config.rate_limit.parsed_trusted_proxies());
    crate::server::state::AppState::new(crate::server::state::AppStateInner {
        engine,
        reload_lock: tokio::sync::Mutex::new(()),
        secrets,
        vars,
        repos,
        audit_queue,
        connector_registry,
        caches: crate::server::state::Caches {
            cache_pool,
            sql_pool_cache,
            mongo_pool_cache,
            smtp_pool_cache,
        },
        channel_registry,
        trace_queue,
        db_pool: pool,
        config,
        start_time: chrono::Utc::now(),
        metrics_handle,
        http_client,
        datalogic,
        jwks,
        rate_limit_state,
        ready,
        reload_degraded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        kafka: crate::server::state::Kafka {
            producer: kafka_producer,
            consumer_handle: Arc::new(tokio::sync::Mutex::new(kafka_consumer_handle)),
            ingest_status: Arc::new(crate::kafka::KafkaIngestStatus::new()),
        },
        trace_persistence_queue,
        cluster,
        tasks,
        admin_auth_failures: Arc::new(Default::default()),
        channel_auth_failures: Arc::new(Default::default()),
        trusted_proxies,
    })
}
