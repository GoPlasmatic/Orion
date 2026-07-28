use std::sync::Arc;

use clap::Parser;

use orion::config;
use orion::server::state::AppState;

mod cli;

use orion::bootstrap;

#[derive(Parser)]
#[command(
    name = "orion-server",
    version,
    long_version = concat!(
        env!("CARGO_PKG_VERSION"),
        "\ngit hash:  ", env!("GIT_HASH"),
        "\nbuilt:     ", env!("BUILD_TIMESTAMP"),
    ),
    about = "Orion — Declarative Services Runtime",
    long_about = "Orion — Declarative Services Runtime\n\n\
        A workflow engine that processes data through configurable channels \
        and workflows. Supports REST, HTTP, Kafka, and async processing modes.\n\
        Ships as a single binary with an embedded SQLite database.",
    after_help = "\
EXAMPLES:\n    \
    orion-server                              Start with default config\n    \
    orion-server -c config.toml               Start with a config file\n    \
    orion-server validate-config              Validate config and show summary\n    \
    orion-server -c config.toml migrate       Run pending database migrations\n    \
    orion-server migrate --dry-run            Preview pending migrations\n    \
    orion-server lint workflow.json           Validate a workflow JSON file\n    \
    orion-server dry-run -w wf.json -i x.json Dry-run a workflow against an input\n    \
    orion-server test-connectivity            Probe DB (and Kafka if enabled)\n    \
    orion-server dump-openapi > spec.json     Write the OpenAPI 3.1 spec to a file\n\n\
ENVIRONMENT VARIABLES:\n    \
    All settings can be overridden via ORION_SECTION__KEY env vars:\n\n    \
    ORION_SERVER__PORT=9090            Override server port\n    \
    ORION_STORAGE__URL=sqlite:app.db   Override database URL\n    \
    ORION_LOGGING__LEVEL=debug         Override log level\n    \
    ORION_ENV=production               Set deployment environment\n\n    \
    See config.toml.example for all available settings."
)]
struct Cli {
    /// Path to TOML configuration file
    #[arg(short, long, global = true)]
    config: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Validate configuration without starting the server.
    ValidateConfig,
    /// Run database migrations without starting the server.
    Migrate {
        /// Preview pending migrations without applying them.
        #[arg(long)]
        dry_run: bool,
    },
    /// Statically validate a workflow JSON file (A6).
    ///
    /// Runs the same checks the admin POST /workflows endpoint performs:
    /// name/id/description, task uniqueness, and the A1 function-input
    /// schema registry. Exits non-zero with field-pathed errors on
    /// failure — wire into CI to catch broken workflows before deploy.
    Lint {
        /// Path to a workflow JSON file (matches the CreateWorkflowRequest shape).
        workflow: String,
    },
    /// Dry-run a workflow against a JSON input file (A6).
    ///
    /// Boots an in-process engine with just the supplied workflow and
    /// no connectors, then prints the per-task execution trace from
    /// dataflow_rs. Useful for testing pure-mapping/log/filter
    /// workflows; tasks that resolve connectors at runtime will fail
    /// with `Connector '...' not found`.
    DryRun {
        /// Path to a workflow JSON file.
        #[arg(short, long)]
        workflow: String,
        /// Path to a JSON file used as the message payload.
        #[arg(short, long)]
        input: String,
    },
    /// Probe configured backends for reachability (A6).
    ///
    /// Opens the configured database pool (using the same `storage.url`)
    /// and runs a no-op query. Catches "DB credentials wrong / file
    /// unreadable" before the server tries to start.
    TestConnectivity,
    /// Print the public HTTP API's OpenAPI 3.1 spec as JSON to stdout.
    ///
    /// Needs no config, database, or running server. Redirect it to refresh
    /// the checked-in copy: `orion-server dump-openapi > docs/openapi.json`.
    DumpOpenapi,
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("Error: {err}");
        let mut source = std::error::Error::source(&*err);
        while let Some(cause) = source {
            eprintln!("  Caused by: {cause}");
            source = std::error::Error::source(cause);
        }
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Load configuration
    let mut config = config::load_config(cli.config.as_deref())?;
    // Resolve the instance identity once, up front, so the tracing resource,
    // cluster runtime, and Kafka static membership all agree on it.
    config.cluster.instance_id = config.cluster.effective_instance_id();
    let config = config;

    if cli.config.is_none() {
        eprintln!(
            "Note: no config file specified (-c <path>). Using defaults + ORION_* env overrides."
        );
    }

    // Handle subcommands that exit early (before starting the server)
    match cli.command {
        Some(Command::ValidateConfig) => return cli::handle_validate_config(&config),
        Some(Command::Migrate { dry_run }) => return cli::handle_migrate(&config, dry_run).await,
        Some(Command::Lint { workflow }) => return cli::run_lint(&workflow),
        Some(Command::DryRun { workflow, input }) => {
            return cli::run_dry_run(&workflow, &input).await;
        }
        Some(Command::TestConnectivity) => return cli::run_test_connectivity(&config).await,
        Some(Command::DumpOpenapi) => return cli::run_dump_openapi(),
        None => {} // Continue to start the server
    }

    // Init tracing subscriber with optional OpenTelemetry layer (see
    // `bootstrap::init_observability`). The provider is flushed at shutdown.
    let _otel_provider = bootstrap::init_observability(&config)?;

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        git_hash = env!("GIT_HASH"),
        build_timestamp = env!("BUILD_TIMESTAMP"),
        environment = %config.environment,
        "Starting Orion — Declarative Services Runtime"
    );

    // Init metrics (gated by config)
    let metrics_handle = bootstrap::init_metrics_handle(&config);

    // Install sqlx Any drivers for external connector pools (must be before any pool creation)
    sqlx::any::install_default_drivers();

    // Init database. With auto_migrate = false (multi-replica deploys) a
    // stale schema is a hard startup error — a replica must never serve
    // against pending migrations; `orion-server migrate` is the deploy step.
    let pool = orion::storage::init_pool_for_startup(&config.storage).await?;
    tracing::info!(path = %config.storage.url, "Database initialized");
    if config.cluster.enabled && config.storage.auto_migrate {
        tracing::warn!(
            "cluster.enabled with storage.auto_migrate = true: replicas will race \
             migrations at boot (safe but noisy) — prefer auto_migrate = false plus \
             an `orion-server migrate` deploy step"
        );
    }

    // Cluster runtime: instance identity + shared Redis (fails fast when
    // enabled and Redis is unreachable).
    let cluster = orion::cluster::init_cluster_runtime(&config.cluster, &pool).await?;
    tracing::info!(
        instance_id = %cluster.instance_id,
        cluster_enabled = cluster.enabled,
        "Instance identity"
    );

    // Create repositories
    let repos = bootstrap::Repositories::new(&pool);

    // Channel registry
    let channel_registry = Arc::new(if config.cluster.enabled {
        orion::channel::ChannelRegistry::with_cluster((&*cluster).into())
    } else {
        orion::channel::ChannelRegistry::new()
    });

    // Connector registry, shared HTTP client, engine lock, cache pools,
    // custom function handlers, and the Kafka producer (see
    // `bootstrap::build_engine_components`).
    let mut components =
        bootstrap::build_engine_components(&config, &repos, channel_registry.clone()).await?;

    // Readiness flag — set after engine is fully initialized
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Load active channels and workflows, build the engine, and populate the
    // pre-created engine lock. Channels that fail to load are quarantined —
    // refused at every ingress until fixed.
    let (channels, active_workflow_count) = components
        .load_channels_and_build_engine(&config, &repos, &channel_registry)
        .await?;

    // Mark the service as ready now that the engine and channel registry are loaded
    ready.store(true, std::sync::atomic::Ordering::Release);

    let bootstrap::EngineComponents {
        connector_registry,
        http_client,
        datalogic: datalogic_engine,
        engine,
        cache_pool,
        sql_pool_cache,
        mongo_pool_cache,
        custom_functions: _,
        kafka_producer,
    } = components;

    let kafka_consumer_handle = bootstrap::start_kafka_ingest(
        &config.kafka,
        &channels,
        engine.clone(),
        channel_registry.clone(),
        datalogic_engine.clone(),
        kafka_producer.clone(),
        cluster.enabled.then(|| cluster.instance_id.as_str()),
    )?;

    // Start the background tasks: trace persistence queue, trace queue
    // worker pool (with DLQ for failed async traces), trace + audit-log
    // cleanup, and the DLQ retry consumer.
    let (trace_persistence_queue, trace_queue, mut task_handles) =
        bootstrap::start_background_tasks(
            &config,
            engine.clone(),
            &repos,
            channel_registry.clone(),
            &cluster,
        );

    // Set initial active rules gauge
    orion::metrics::set_active_workflows(active_workflow_count as f64);

    // Build rate limiter (if enabled)
    let rate_limit_state = bootstrap::build_rate_limit_state(&config);

    // Build state and router
    let config = Arc::new(config);

    let kafka_consumer_handle_arc = Arc::new(tokio::sync::Mutex::new(kafka_consumer_handle));

    let state = AppState::new(orion::server::state::AppStateInner {
        engine,
        channel_repo: repos.channels,
        workflow_repo: repos.workflows,
        connector_repo: repos.connectors,
        trace_repo: repos.traces,
        trace_dlq_repo: repos.trace_dlq,
        audit_log_repo: repos.audit_logs,
        connector_registry,
        cache_pool,
        channel_registry,
        trace_queue,
        db_pool: pool,
        config: config.clone(),
        start_time: chrono::Utc::now(),
        metrics_handle,
        http_client,
        datalogic: datalogic_engine,
        rate_limit_state,
        ready: ready.clone(),
        sql_pool_cache,
        mongo_pool_cache,
        kafka_consumer_handle: kafka_consumer_handle_arc.clone(),
        kafka_producer,
        trace_persistence_queue,
        cluster,
    });

    // Cluster background tasks (epoch watcher). Empty when disabled.
    task_handles.cluster_task_handles = orion::cluster::start_cluster_tasks(&state);

    let router = orion::server::build_router(state);

    let addr = format!("{}:{}", config.server.host, config.server.port);

    if config.server.tls.enabled {
        serve_tls(&addr, &config, ready.clone(), router).await?;
    } else {
        serve_plain_http(
            &addr,
            &config.storage.url,
            config.server.shutdown_drain_secs,
            config.server.shutdown_force_timeout_secs,
            ready.clone(),
            router,
        )
        .await?;
    }

    // Graceful shutdown
    if let Some(handle) = kafka_consumer_handle_arc.lock().await.take() {
        tracing::info!("Shutting down Kafka consumer...");
        handle.shutdown().await;
    }

    task_handles.shutdown().await;

    // Flush pending OTel spans before exit
    if let Some(provider) = _otel_provider {
        tracing::info!("Flushing OpenTelemetry spans...");
        if let Err(e) = provider.shutdown() {
            tracing::warn!(error = %e, "Error shutting down OTel tracer provider");
        }
    }

    tracing::info!("Orion shut down cleanly");
    Ok(())
}

/// Create a bound TCP listener with `TCP_NODELAY` enabled (avoids Nagle's
/// 40 ms latency on small responses) and `SO_REUSEADDR` set.
fn create_tcp_listener(addr: &str) -> Result<tokio::net::TcpListener, orion::errors::OrionError> {
    let socket_addr = addr.parse::<std::net::SocketAddr>().map_err(|e| {
        orion::errors::OrionError::Internal(format!("Invalid address '{addr}': {e}"))
    })?;
    let domain = if socket_addr.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let map_err = |stage: &str, e: std::io::Error| orion::errors::OrionError::InternalSource {
        context: format!("Failed to {stage} for {addr}"),
        source: Box::new(e),
    };
    let socket = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))
        .map_err(|e| map_err("create socket", e))?;
    socket.set_tcp_nodelay(true).ok();
    socket.set_reuse_address(true).ok();
    socket
        .bind(&socket_addr.into())
        .map_err(|e| map_err("bind", e))?;
    socket.listen(1024).map_err(|e| map_err("listen", e))?;
    socket.set_nonblocking(true).ok();
    tokio::net::TcpListener::from_std(socket.into())
        .map_err(|e| map_err("create async listener", e))
}

/// Bind a TLS (HTTPS) listener via axum-server and serve `router` with
/// graceful shutdown — same drain sequence as the plain-HTTP path.
async fn serve_tls(
    addr: &str,
    config: &config::AppConfig,
    ready: Arc<std::sync::atomic::AtomicBool>,
    router: axum::Router,
) -> Result<(), Box<dyn std::error::Error>> {
    let rustls_config = orion::server::tls::load_rustls_config(
        &config.server.tls.cert_path,
        &config.server.tls.key_path,
    )
    .await?;

    let bind_addr: std::net::SocketAddr = addr.parse()?;
    let handle = axum_server::Handle::new();
    let shutdown_handle = handle.clone();
    let drain_secs = config.server.shutdown_drain_secs;
    let force_timeout_secs = config.server.shutdown_force_timeout_secs;
    let ready_for_drain = ready.clone();
    tokio::spawn(async move {
        // Withdraw readiness, keep accepting through the LB grace window,
        // THEN stop accepting — same sequence as the plain-HTTP path.
        orion::server::drain::drain_gate(
            orion::server::shutdown_signal(),
            ready_for_drain,
            std::time::Duration::from_secs(drain_secs),
        )
        .await;
        let force =
            (force_timeout_secs > 0).then(|| std::time::Duration::from_secs(force_timeout_secs));
        shutdown_handle.graceful_shutdown(force);
    });

    tracing::info!(
        address = %addr,
        storage = %config.storage.url,
        tls = true,
        "Orion is ready (HTTPS)"
    );

    axum_server::bind_rustls(bind_addr, rustls_config)
        .handle(handle)
        .serve(router.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .await?;
    Ok(())
}

/// Bind a plain (non-TLS) HTTP listener and serve `router` with graceful
/// shutdown.
async fn serve_plain_http(
    addr: &str,
    storage_url: &str,
    drain_secs: u64,
    force_timeout_secs: u64,
    ready: Arc<std::sync::atomic::AtomicBool>,
    router: axum::Router,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = create_tcp_listener(addr)?;
    tracing::info!(
        address = %addr,
        storage = %storage_url,
        tcp_nodelay = true,
        "Orion is ready"
    );
    // `drained` fires when the gate resolves (accept stopped); the force
    // timeout counts from that point, bounding the in-flight wait.
    let (drained_tx, drained_rx) = tokio::sync::oneshot::channel::<()>();
    let gate = async move {
        orion::server::drain::drain_gate(
            orion::server::shutdown_signal(),
            ready,
            std::time::Duration::from_secs(drain_secs),
        )
        .await;
        let _ = drained_tx.send(());
    };
    let serve = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(gate);
    if force_timeout_secs == 0 {
        serve.await?;
    } else {
        tokio::select! {
            result = serve => result?,
            _ = async {
                // Wait for the gate, then bound the in-flight drain. If the
                // server finishes first the sender is dropped and this arm
                // pends forever — the select resolves through `serve`.
                match drained_rx.await {
                    Ok(()) => {
                        tokio::time::sleep(std::time::Duration::from_secs(force_timeout_secs)).await;
                        tracing::warn!(
                            force_timeout_secs,
                            "Shutdown force timeout elapsed; aborting remaining in-flight connections"
                        );
                    }
                    Err(_) => std::future::pending::<()>().await,
                }
            } => {}
        }
    }
    Ok(())
}
