//! Startup-sequence helpers for `run()` in `main.rs` — observability init,
//! repository construction, and background-task lifecycle. `main.rs` stays
//! the readable orchestration script and calls these phases in order.

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
    /// The process's one expression engine (I188), built once here over
    /// `[secrets]` and `engine.ops_budget`, and kept by every reload.
    pub exprs: dataflow_rs::expr::ExprEngine,
    /// The JWKS cache, built on `http_client` so key fetches ride the pinned
    /// resolver and the shared connection pool.
    pub jwks: Arc<crate::jwt::jwks::JwksCache>,
    /// The OIDC discovery cache (#355), on the same client and address-checked
    /// through the same `oauth2_login.allow_private_token_urls` gate as the token
    /// exchange.
    pub oidc_discovery: Arc<crate::channel::oidc_discovery::DiscoveryCache>,
    /// `[secrets]`, resolved once. Every engine built from here on carries it,
    /// including the ones the admin plane builds per request — a surface that
    /// built its engine without it would refuse a workflow the serving engine
    /// runs.
    pub secrets: Arc<crate::engine::ResolvedSecrets>,
    /// `[vars]` as one JSON object, or `None` when the instance declares none.
    ///
    /// Derived once, here, and cloned to every consumer — `AppState` and the
    /// cron scheduler. The "`None` strips
    /// the key rather than stamping `{}`" convention that makes `metadata.vars`
    /// unforgeable lives in `VarsConfig::to_json`, and re-deriving it per
    /// consumer is how two ingresses come to disagree about what a workflow
    /// reads.
    pub vars: Option<Arc<serde_json::Value>>,
    /// The live serving generation. Pre-created around an empty engine and an
    /// empty channel estate, so the `channel_call` handler —
    /// which is registered *on* the engine this handle will carry — has
    /// something to hold before either half exists.
    pub runtime: Arc<crate::runtime::RuntimeHandle>,
    pub cache_pool: Arc<crate::connector::cache_backend::CachePool>,
    pub sql_pool_cache: Arc<crate::connector::pool_cache::SqlPoolCache>,
    /// The plugin sandbox, when `plugins.enabled`: one engine per process,
    /// sized from the ceilings before the first generation loads anything
    /// into it. `None` makes every stored plugin a load issue on this node.
    pub plugins: Option<Arc<crate::plugin::WasmRuntime>>,
    /// What this node serves, frozen: built from the edition's
    /// [`crate::extensions::CompositionBuilder`] over the node's services.
    pub composition: crate::extensions::Composition,
    /// The node's v2 deployment store, created with the node's identity.
    pub deployments: crate::extensions::ImmutableDeploymentStore,
    /// The node's deployment reconciler, over the same store and identity.
    pub reconciler: crate::extensions::NodeReconciler,
    /// Bounded host-owned native work, sized from the config.
    pub native_scheduler: crate::extensions::NativeWorkScheduler,
}

/// The engine's serving components, built in one pass by
/// [`build_engine_components`]: connector registry (with the F16 fail-fast
/// check), shared HTTP client, expression engine, the pre-created engine
/// lock, cache + external connector pool caches, and the custom function
/// handlers.
pub struct EngineComponents {
    pub serving: ServingComponents,
    /// Orion's functions and the profile layer included ([`crate::engine::serving_registry`]): what the first
    /// generation compiles every stored workflow on, its plugins added. Its
    /// verdict decides which rows load (I182).
    pub registry: dataflow_rs::Registry,
}

/// Build the [`EngineComponents`]: load the connector registry, create the
/// shared HTTP client, the expression engine, the runtime handle (pre-created
/// so the `channel_call` handler can reference it), the cache + external pool
/// caches, and the custom function handlers.
pub async fn build_engine_components(
    config: &config::AppConfig,
    channel_loader: &Arc<crate::channel::ChannelLoader>,
    composition: crate::extensions::CompositionBuilder,
) -> Result<EngineComponents, Box<dyn std::error::Error>> {
    // `[vars]` before the connectors that may reference them. Pure config —
    // no resolution, no I/O — so it costs nothing to have it early, and the
    // registry then carries it into every reload rather than being handed it
    // per call.
    let vars = config.vars.to_json().map(Arc::new);

    // Empty until the committed deployment head, if any, is built: its
    // Connections over built-in drivers are what the registry loads.
    let connector_registry = Arc::new(
        ConnectorRegistry::new(config.engine.circuit_breaker.clone()).with_vars(vars.clone()),
    );

    // Create a shared HTTP client. Redirects are off: execute_request follows
    // them manually with per-hop SSRF validation. The pinned resolver connects
    // to the exact addresses SSRF validation vetted (no DNS rebinding between
    // check and connect). The `User-Agent` is set here rather than at each call
    // site, so http_call, the OAuth2 token exchange, OIDC discovery, JWKS and
    // the userinfo fetch all carry one (#356).
    let http_client = reqwest::Client::builder()
        .user_agent(crate::version::USER_AGENT)
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

    // One OIDC discovery cache per instance, on the same pinned client (#355).
    // Its egress is the same class as the token exchange — an authored issuer
    // Orion fetches — so it shares that gate rather than inventing a new one.
    let oidc_discovery = Arc::new(crate::channel::oidc_discovery::DiscoveryCache::new(
        http_client.clone(),
        config.oauth2_login.allow_private_token_urls,
    ));

    // Resolve `[secrets]` before anything builds an engine. A reference that
    // cannot be resolved stops the boot: handing one onward as its own literal
    // text is the failure the reference syntax exists to prevent, and an
    // instance serving a workflow whose signing key is the string
    // `env://PARTNER_HMAC_KEY` fails at the remote system with nothing
    // pointing back here.
    let secrets = Arc::new(crate::engine::ResolvedSecrets::resolve(&config.secrets).await?);

    // The process's one expression engine (I188): Orion's operators, the
    // secret store resolved above and the operation budget. The channel loader
    // compiles every channel expression on it (`validation_logic`,
    // `authorization_logic`, the rate-limit and cache `key_logic`), and the
    // guards, cron and `/validate` evaluate on it. Nothing
    // rebuilds it: every reload keeps it.
    //
    // Secrets are start-time config, resolved above, so there is no ordering
    // reason a channel guard should see fewer of them than a workflow does.
    let exprs =
        crate::engine::operators::orion_exprs(&secrets, config.engine.ops_budget).map_err(|e| {
            crate::errors::OrionError::internal_from(
                "Failed to build the shared expression engine",
                e,
            )
        })?;
    if !secrets.is_empty() {
        tracing::info!(
            names = ?secrets.names().collect::<Vec<_>>(),
            "Secrets resolved"
        );
    }

    // Create the runtime handle early so the `channel_call` function can
    // reference it. Generation 0 has no engine and an empty channel estate;
    // `load_channels_and_build_engine` publishes the first real generation
    // over it before anything is served.
    let runtime: Arc<crate::runtime::RuntimeHandle> = Arc::new(crate::runtime::RuntimeHandle::new(
        Arc::new(crate::channel::ChannelSnapshot::empty()),
        crate::engine::FunctionRegistry::builtin().clone(),
    ));

    // The in-memory cache backends behind `cache` connectors and the
    // single-node dedup/response-cache defaults.
    let cache_pool = Arc::new(crate::connector::cache_backend::CachePool::new(
        config.engine.cache_cleanup_interval_secs,
        config.engine.max_memory_cache_entries,
    ));

    // Create external connector pool caches (shared with AppState for eviction on update/delete)
    let sql_pool_cache = Arc::new(crate::connector::pool_cache::SqlPoolCache::new(
        config.engine.max_pool_cache_entries,
    ));

    let foundation = crate::extensions::FoundationOptions {
        http_client: http_client.clone(),
        sql: crate::extensions::SqlLimits {
            max_rows: usize::try_from(config.query.max_limit).unwrap_or(usize::MAX),
            max_result_bytes: config.query.max_result_bytes,
        },
        ..Default::default()
    };
    // The node's services, created once with its identity, then the
    // edition's composition frozen over them. The identity is the configured
    // instance id (resolved before `assemble` reaches here) and a fresh boot
    // id: the deployment store's receipts and the reconciler's status rows
    // both carry it.
    let node = crate::extensions::NodeIdentity::new(
        config.cluster.instance_id.clone(),
        format!("boot-{}", uuid::Uuid::new_v4().simple()),
    );
    let deployments = crate::extensions::ImmutableDeploymentStore::new(node.node_id.clone());
    let native_scheduler =
        crate::extensions::NativeWorkScheduler::new(config.engine.native_work_max_in_flight);
    let composition = composition
        .with_resource_pool(sql_pool_cache.clone())
        .build(crate::extensions::BuildContext {
            deployments: deployments.clone(),
            ledger: Arc::new(crate::extensions::RevocationLedger::new()),
            foundation,
        })?;
    let reconciler =
        crate::extensions::NodeReconciler::new(deployments.clone(), node.node_id, node.boot_id);
    let providers = composition.providers().clone();

    // The node's live components, as every function gets them.
    let handler_deps = || crate::engine::HandlerDeps {
        registry: connector_registry.clone(),
        client: http_client.clone(),
        runtime: runtime.clone(),
        exprs: exprs.clone(),
        jwks: jwks.clone(),
        engine_config: &config.engine,
        query_config: &config.query,
        write_config: &config.write,
        cache_pool: cache_pool.clone(),
        channel_loader: channel_loader.clone(),
        sql_pool_cache: sql_pool_cache.clone(),
        providers: providers.clone(),
    };
    let registry = crate::engine::serving_registry(handler_deps());

    let plugins = if config.plugins.enabled {
        Some(
            crate::plugin::WasmRuntime::with_native_work(
                &config.plugins,
                native_scheduler.clone(),
                std::time::Duration::from_millis(config.engine.native_work_timeout_ms),
            )
            .map_err(|e| crate::errors::OrionError::Config {
                message: format!("plugins: the sandbox could not be created: {e}"),
            })?,
        )
    } else {
        None
    };

    Ok(EngineComponents {
        serving: ServingComponents {
            connector_registry,
            http_client,
            exprs,
            jwks,
            oidc_discovery,
            secrets,
            vars,
            runtime,
            cache_pool,
            sql_pool_cache,
            plugins,
            composition,
            deployments,
            reconciler,
            native_scheduler,
        },
        registry,
    })
}

impl EngineComponents {
    /// Publish generation 1 through the pre-created runtime handle: the
    /// serving registry — Orion's functions, no workflows, no channels, no
    /// plugins — so a definition upload has the engine it compiles on before
    /// anything is deployed. The committed deployment head, if any, is built
    /// over it next. Returns the [`ServingComponents`] `AppState` is
    /// assembled from.
    ///
    /// F55: takes `self` by value. This step *consumes* the custom-function
    /// handlers, so when it took `&mut self` the map was left as an empty hole
    /// that `build_app_state` had to know to ignore — and calling
    /// `build_app_state` first compiled fine and silently produced an engine
    /// with no Orion handlers registered at all. Consuming the value makes that
    /// order a compile error instead.
    pub fn publish_first_generation(self, config: &config::AppConfig) -> ServingComponents {
        let EngineComponents { serving, registry } = self;
        let compiler = registry.compiler(dataflow_rs::program::CompileOptions::default());
        let engine = crate::engine::serving_engine(compiler.link(Vec::new()));
        let channels = serving.runtime.load().channels.clone();
        serving
            .runtime
            .publish_parts(crate::runtime::generation::GenerationParts {
                connectors: Some(serving.connector_registry.snapshot_configs()),
                run_budget_limits: config.engine.run_budget_limits(),
                ..crate::runtime::generation::GenerationParts::new(
                    engine,
                    channels,
                    crate::engine::FunctionRegistry::builtin().clone(),
                    Arc::new(crate::plugin::PluginSet::empty()),
                )
            });
        serving
    }
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
/// cleanup, DLQ retry) and the cluster reconciler used to be `JoinHandle`s
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

/// Structured bundle of the background queues and their drain handles (`SIMP-05`).
pub struct BackgroundQueues {
    pub trace_persistence_queue: crate::queue::TracePersistenceQueue,
    pub trace_queue: crate::queue::TraceQueue,
    pub audit_queue: crate::queue::audit_queue::AuditQueue,
    pub task_handles: TaskHandles,
}

/// Start the background tasks: trace persistence queue, trace queue worker
/// pool, trace cleanup, audit-log cleanup, and the DLQ retry consumer.
/// Returns the [`BackgroundQueues`] bundle owning the queues `AppState` needs
/// plus the [`TaskHandles`] owning the shutdown sequence.
pub fn start_background_tasks(
    config: &config::AppConfig,
    tasks: &crate::runtime::TaskRegistry,
    runtime: Arc<crate::runtime::RuntimeHandle>,
    repos: &Repositories,
    cluster: &crate::cluster::ClusterRuntime,
    cron: CronComponents,
) -> BackgroundQueues {
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
    // The pool needs the persistence queue + runtime handle so it can route
    // status / result writes through the configured mode.
    let (trace_queue, worker_handle) = crate::queue::start_workers(
        tasks,
        &config.trace_queue,
        crate::queue::WorkerDeps {
            runtime: runtime.clone(),
            trace_repo: repos.traces.clone(),
            dlq_repo: Some(repos.trace_dlq.clone()),
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

    // Cloned before the DLQ retry consumer takes ownership of `runtime`.
    let runtime_for_cron = runtime.clone();

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
            runtime,
        );
        tracing::info!(
            poll_interval_secs = config.trace_queue.dlq_poll_interval_secs,
            max_retries = config.trace_queue.dlq_max_retries,
            "DLQ retry consumer started"
        );
    }

    // The scheduler, after the queues it writes traces through: it produces
    // work that lands in the trace pipeline, so the pipeline has to exist first.
    crate::cron::start(
        tasks,
        crate::cron::CronDeps {
            runtime: runtime_for_cron,
            repo: repos.cron.clone(),
            trace_repo: repos.traces.clone(),
            persistence_queue: trace_persistence_queue.clone(),
            global_trace_storage: config.trace_storage.clone(),
            exprs: cron.exprs,
            vars: cron.vars,
            instance_id: cluster.instance_id.clone(),
            status: cron.status,
            config: config.cron.clone(),
            max_result_size_bytes: config.trace_queue.max_result_size_bytes,
            lease_gate: job_lease_gate.clone(),
        },
    );

    // Occurrence retention, on the same cadence and the same knob as trace
    // cleanup: an occurrence and the trace it produced age out together, so an
    // operator has one retention decision to make rather than two.
    crate::cron::start_cleanup(
        tasks,
        config.trace_queue.retention_hours,
        config.trace_queue.cleanup_interval_secs,
        repos.cron.clone(),
        job_lease_gate,
    );

    BackgroundQueues {
        trace_persistence_queue,
        trace_queue,
        audit_queue,
        task_handles: TaskHandles {
            trace_persistence_handle,
            worker_handle,
            audit_writer_handle,
        },
    }
}

/// The pieces the cron scheduler needs that the trace pipeline does not.
///
/// A struct rather than three more positional parameters on
/// [`start_background_tasks`], two of which are `Arc`s of unrelated things.
pub struct CronComponents {
    /// The process's expression engine, which the guards evaluate on.
    pub exprs: dataflow_rs::expr::ExprEngine,
    pub vars: Option<Arc<serde_json::Value>>,
    pub status: Arc<crate::cron::CronStatus>,
}

/// Inputs to [`build_app_state`] that aren't already carried by
/// [`Repositories`] / [`EngineComponents`].
pub struct AppStateParams {
    pub config: Arc<config::AppConfig>,
    pub pool: crate::storage::DbPool,
    pub repos: Repositories,
    pub components: ServingComponents,
    /// The node's channel builder. Held on `AppState` because every reload
    /// needs it; a request never does — it reads the estate off the published
    /// generation instead.
    pub channel_loader: Arc<crate::channel::ChannelLoader>,
    pub trace_queue: crate::queue::TraceQueue,
    pub trace_persistence_queue: crate::queue::TracePersistenceQueue,
    pub audit_queue: crate::queue::audit_queue::AuditQueue,
    pub rate_limit_state: Option<Arc<crate::server::rate_limit::RateLimitState>>,
    pub metrics_handle: metrics_exporter_prometheus::PrometheusHandle,
    pub ready: Arc<std::sync::atomic::AtomicBool>,
    pub cluster: Arc<crate::cluster::ClusterRuntime>,
    /// The supervisor the background tasks registered with, so the probes can
    /// read their liveness.
    pub tasks: Arc<crate::runtime::TaskRegistry>,
    /// The scheduler's own view of itself, for `/health`.
    pub cron_status: Arc<crate::cron::CronStatus>,
}

/// The two inputs [`assemble`] does not build itself, because its callers
/// build them differently on purpose:
///
/// - `pool`: production opens it with `init_pool_for_startup` (a stale schema
///   is a hard error); the test harness uses in-memory SQLite through
///   `init_pool` (always migrates). Handed in already open.
/// - `metrics_handle`: production installs the global recorder through
///   `init_metrics_handle`; the harness through `init_metrics_with_instance`
///   with first-caller-wins semantics. Installed before `assemble`, so any
///   metric the build sequence emits is recorded.
///
/// Everything else — the cluster runtime, the repositories, the channel
/// loader, the OAuth2 runtime (with its cluster lease derived from the config),
/// and the plugin epoch ticker — `assemble` builds the one
/// way both paths must agree on. `config` is already shared.
pub struct AssembleParams {
    pub config: Arc<config::AppConfig>,
    pub pool: crate::storage::DbPool,
    pub metrics_handle: metrics_exporter_prometheus::PrometheusHandle,
}

/// What [`assemble`] hands back: the wired `AppState` and the queue-drain
/// handles that own the graceful-shutdown sequence. The caller keeps the
/// handles so it, not `assemble`, decides when the drains run.
pub struct Assembled {
    pub state: crate::server::state::AppState,
    pub task_handles: TaskHandles,
}

/// The ordered startup sequence from an open pool to a wired `AppState`:
/// cluster runtime, repositories, channel loader, engine components, the
/// managed-OAuth2 runtime, the first published generation, the background
/// tasks and the plugin ticker. `main.rs` and the
/// integration-test harness (`tests/integration/common`) both call this so a
/// step added here reaches both — the sequence used to be spelled twice and a
/// step added to one ran untested in the other.
///
/// It stops at `AppState`: the caller then builds the router, spawns the
/// state-dependent serving tasks (cluster tasks, limiter
/// prune, `[packages] apply`) and runs the server, because those are where
/// production and the harness diverge — the harness leaves them to the test
/// that wants them.
///
/// `composition` is what the node serves: Community passes
/// [`crate::extensions::CompositionRoot::community`], an edition its own
/// builder. It is frozen here, over the node's deployment store and
/// revocation ledger; a duplicate registration fails the boot.
pub async fn assemble(
    params: AssembleParams,
    composition: crate::extensions::CompositionBuilder,
) -> Result<Assembled, Box<dyn std::error::Error>> {
    let AssembleParams {
        config,
        pool,
        metrics_handle,
    } = params;
    // One identity for this node, resolved once: the cluster runtime, the
    // node reconciler and the deployment store's receipts all
    // read it from here. `main` resolves it into the config already; a test
    // harness may leave it empty, which mints one.
    let config = if config.cluster.instance_id.is_empty() {
        let mut resolved = (*config).clone();
        resolved.cluster.instance_id = config.cluster.effective_instance_id();
        Arc::new(resolved)
    } else {
        config
    };

    // Cluster runtime: instance identity and the shared store over the state
    // database.
    let cluster = crate::cluster::init_cluster_runtime(&config.cluster, &pool).await?;
    tracing::info!(
        instance_id = %cluster.instance_id,
        cluster_enabled = cluster.enabled,
        "Instance identity"
    );

    let repos = Repositories::new(&pool, &config.storage)?;

    // Builds the channel half of every generation. Cluster mode swaps in the
    // strict backend matrix.
    let channel_loader = Arc::new(if config.cluster.enabled {
        crate::channel::ChannelLoader::with_cluster((&*cluster).into())
    } else {
        crate::channel::ChannelLoader::new()
    });

    // Connector registry, shared HTTP client, runtime handle, cache pools and
    // custom function handlers.
    let components = build_engine_components(&config, &channel_loader, composition).await?;

    // #268: hand the managed-OAuth2 token manager its runtime — the shared
    // client, the encrypted state store, and (in cluster mode) the refresh
    // lease that keeps N nodes from rotating against each other. The lease is
    // derived from the config here, so the harness wires it the same way a
    // cluster node does rather than passing `None` by hand.
    components
        .serving
        .connector_registry
        .oauth()
        .init(crate::connector::oauth::OAuthRuntimeDeps {
            http_client: components.serving.http_client.clone(),
            repo: repos.oauth_state.clone(),
            lease: config.cluster.enabled.then(|| {
                Arc::new(crate::cluster::JobLeaseGate::new(
                    cluster.repo.clone(),
                    cluster.instance_id.clone(),
                ))
            }),
        });

    // Readiness flag — set once the first generation is published and the
    // committed deployment head, if there is one, has been built.
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Publish the first generation through the pre-created runtime handle.
    // Consumes `components`: the handler map goes into the engine, and what
    // comes back is the half that backs `AppState` (F55).
    let components = components.publish_first_generation(&config);

    // One supervisor for every long-lived background task, so `/health` and
    // `/readyz` can report their liveness; the scheduler's own status is
    // shared too, for what its liveness does not say.
    let tasks = Arc::new(crate::runtime::TaskRegistry::new());
    let cron_status = Arc::new(crate::cron::CronStatus::new());
    let BackgroundQueues {
        trace_persistence_queue,
        trace_queue,
        audit_queue,
        task_handles,
    } = start_background_tasks(
        &config,
        &tasks,
        components.runtime.clone(),
        &repos,
        &cluster,
        CronComponents {
            exprs: components.exprs.clone(),
            vars: components.vars.clone(),
            status: cron_status.clone(),
        },
    );

    // The plugin epoch ticker: the clock every plugin deadline is measured in,
    // supervised as `Required` so a dead ticker degrades `/readyz` rather than
    // silently disabling every deadline. Only when the sandbox exists.
    if let Some(sandbox) = &components.plugins {
        crate::plugin::ticker::start(&tasks, sandbox.clone());
    }

    // Initial active-workflows gauge. After the recorder is installed (the
    // caller did that before `assemble`), or the write lands nowhere. The
    // head build below moves it.
    crate::metrics::set_active_workflows(0.0);

    let rate_limit_state = build_rate_limit_state(&config);

    let state = build_app_state(AppStateParams {
        config,
        pool,
        repos,
        components,
        channel_loader,
        trace_queue,
        trace_persistence_queue,
        audit_queue,
        rate_limit_state,
        metrics_handle,
        ready: ready.clone(),
        cluster,
        tasks,
        cron_status,
    });

    // Hydrate the v2 deployment store from the state database, so committed
    // revisions and the head survive a restart, then build the head. Either
    // failing stops the boot: a node that cannot read or build what is
    // committed must not report ready while serving something else.
    state
        .deployments
        .load_from_db_pool(&state.db_pool)
        .await
        .map_err(|e| format!("reading the committed deployments failed: {e}"))?;
    crate::cluster::reconcile::sync_v2_revocations(&state);
    crate::runtime::workflow_providers::sync(&state);
    let (head_rev, _) = state.deployments.current_head();
    if head_rev > 0 {
        crate::runtime::reconcile_deployment_revision(&state, head_rev)
            .await
            .map_err(|e| format!("deployment revision {head_rev} could not be built: {e}"))?;
    }
    // F16: a connection that fails to load is absent from the
    // registry, so the failure surfaces as a 500 on the first request that
    // needs it — possibly hours after the boot that caused it. Operators who
    // would rather have the rollout fail at boot opt in here.
    let connector_issues = state.connector_registry.load_issues().await;
    if !connector_issues.is_empty() && state.config.engine.fail_on_connector_load_error {
        let detail = connector_issues
            .iter()
            .map(|i| format!("{} ({}): {}", i.connector, i.stage, i.reason))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(crate::errors::OrionError::Config {
            message: format!(
                "refused to start: {} connection(s) of the committed deployment failed to load: {detail}. \
                 Set engine.fail_on_connector_load_error = false to start anyway \
                 (they will fail at request time instead).",
                connector_issues.len()
            ),
        }
        .into());
    }

    if let Err(e) = state
        .reconciler
        .refresh_observed_nodes_from_db_pool(&state.db_pool)
        .await
    {
        tracing::warn!(error = %e, "could not read the other nodes' deployment status");
    }
    if let Err(e) = state
        .reconciler
        .persist_local_status_to_db_pool(&state.db_pool)
        .await
    {
        tracing::warn!(error = %e, "could not record this node's deployment status");
    }
    ready.store(true, std::sync::atomic::Ordering::Release);
    crate::cluster::start_v2_reconciler(&state);
    crate::runtime::deployment_gc::start(&state);

    Ok(Assembled {
        state,
        task_handles,
    })
}

/// Assemble `AppState` from the bootstrap products — the single place the
/// [`Repositories`] / [`ServingComponents`] fields map onto `AppStateInner`.
/// Called only by [`assemble`], which is the shared entry point `main.rs` and
/// the test harness go through.
pub fn build_app_state(params: AppStateParams) -> crate::server::state::AppState {
    let AppStateParams {
        config,
        pool,
        repos,
        components,
        channel_loader,
        trace_queue,
        trace_persistence_queue,
        audit_queue,
        rate_limit_state,
        metrics_handle,
        ready,
        cluster,
        tasks,
        cron_status,
    } = params;
    let ServingComponents {
        connector_registry,
        http_client,
        exprs,
        jwks,
        oidc_discovery,
        secrets,
        vars,
        runtime,
        cache_pool,
        sql_pool_cache,
        plugins,
        composition,
        deployments,
        reconciler,
        native_scheduler,
    } = components;
    // Parsed once, unconditionally — not from `rate_limit_state`. Three
    // callers need it whether or not the platform limiter is enabled: the
    // audit trail (O7), the failed-auth backoff, and the per-channel rate
    // limit, which applies with the platform limiter off (S15) and keys on
    // the same client identity. See `AppStateInner::trusted_proxies`.
    let trusted_proxies = Arc::new(config.rate_limit.parsed_trusted_proxies());
    crate::server::state::AppState::new(crate::server::state::AppStateInner {
        runtime,
        channel_loader,
        reload_lock: tokio::sync::Mutex::new(()),
        backup_lock: tokio::sync::Mutex::new(()),
        secrets,
        vars,
        repos,
        audit_queue,
        connector_registry,
        caches: crate::server::state::Caches {
            cache_pool,
            sql_pool_cache,
        },
        trace_queue,
        db_pool: pool,
        config,
        start_time: chrono::Utc::now(),
        metrics_handle,
        http_client,
        exprs,
        jwks,
        oidc_discovery,
        rate_limit_state,
        ready,
        reload_degraded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        trace_persistence_queue,
        cluster,
        tasks,
        cron_status,
        plugins,
        admin_auth_failures: Arc::new(Default::default()),
        channel_auth_failures: Arc::new(Default::default()),
        trusted_proxies,
        composition,
        deployments,
        reconciler,
        native_scheduler,
    })
}
