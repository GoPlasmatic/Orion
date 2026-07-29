use std::sync::Arc;

use clap::Parser;

use orion::config;

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
    orion-server validate-config              Validate + dump effective config (TOML)\n    \
    orion-server validate-config --format summary  Short human summary instead\n    \
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
    ORION_ENVIRONMENT=production       Set deployment environment\n\n    \
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
    /// Validate configuration without starting the server, then print the
    /// full effective config (defaults + file + ORION_* env overrides) with
    /// secrets masked. `--format summary` prints a short human summary
    /// instead.
    ValidateConfig {
        /// Output format for the effective config.
        #[arg(long, value_enum, default_value = "toml")]
        format: cli::ConfigFormat,
    },
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
        Some(Command::ValidateConfig { format }) => {
            return cli::handle_validate_config(&config, format);
        }
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
    let components =
        bootstrap::build_engine_components(&config, &repos, channel_registry.clone()).await?;

    // Readiness flag — set after engine is fully initialized
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Load active channels and workflows, build the engine, and populate the
    // pre-created engine lock. Channels that fail to load are quarantined —
    // refused at every ingress until fixed. Consumes `components`: the
    // handler map goes into the engine, and what comes back is the half that
    // backs `AppState` (F55).
    let (components, channels, active_workflow_count) = components
        .load_channels_and_build_engine(&config, &repos, &channel_registry)
        .await?;

    // Mark the service as ready now that the engine and channel registry are loaded
    ready.store(true, std::sync::atomic::Ordering::Release);

    let kafka_consumer_handle = bootstrap::start_kafka_ingest(
        &config.kafka,
        &channels,
        components.engine.clone(),
        channel_registry.clone(),
        components.datalogic.clone(),
        components.kafka_producer.clone(),
        cluster.enabled.then(|| cluster.instance_id.as_str()),
    )?;

    // Start the background tasks: trace persistence queue, trace queue
    // worker pool (with DLQ for failed async traces), trace + audit-log
    // cleanup, and the DLQ retry consumer.
    let (trace_persistence_queue, trace_queue, audit_queue, mut task_handles) =
        bootstrap::start_background_tasks(
            &config,
            components.engine.clone(),
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

    let state = bootstrap::build_app_state(bootstrap::AppStateParams {
        config: config.clone(),
        pool,
        repos,
        components,
        channel_registry,
        trace_queue,
        trace_persistence_queue,
        audit_queue,
        rate_limit_state,
        metrics_handle,
        ready: ready.clone(),
        kafka_consumer_handle,
        cluster,
    });

    // Cluster background tasks (epoch watcher). Empty when disabled.
    task_handles.cluster_task_handles = orion::cluster::start_cluster_tasks(&state);

    let router = orion::server::build_router(state.clone());

    // O12: optional dedicated metrics listener. Bound *before* the main
    // server starts, so an address clash or a permission problem is a startup
    // failure rather than a silently missing scrape target. Its shutdown
    // future is an independent `shutdown_signal()` — signal handlers fan out
    // to every registered stream, so both listeners see the same SIGTERM.
    let metrics_server = match config.metrics.dedicated_bind_addr() {
        Some(addr) => {
            let listener = orion::server::serve::create_tcp_listener(addr)?;
            if !listener.local_addr().is_ok_and(|a| a.ip().is_loopback()) {
                tracing::warn!(
                    address = %addr,
                    "metrics.bind_addr is not a loopback address and the metrics listener is \
                     unauthenticated — make sure it is reachable only from your scrapers"
                );
            }
            Some(tokio::spawn(orion::server::serve::serve_metrics(
                listener,
                config.clone(),
                orion::server::metrics_router(state.clone()),
                orion::server::shutdown_signal(),
            )))
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
            None
        }
    };

    if config.server.tls.enabled {
        let handle = axum_server::Handle::new();
        orion::server::serve::serve_tls(
            config.clone(),
            ready.clone(),
            router,
            handle,
            orion::server::shutdown_signal(),
        )
        .await?;
    } else {
        let addr = format!("{}:{}", config.server.host, config.server.port);
        let listener = orion::server::serve::create_tcp_listener(&addr)?;
        orion::server::serve::serve_plain_http(
            listener,
            config.clone(),
            ready.clone(),
            router,
            orion::server::shutdown_signal(),
        )
        .await?;
    }

    // The metrics listener drains on the same grace window, so by the time the
    // main server has returned it is at most a scheduling hop behind. Bound
    // the join anyway — a stuck scrape must not hold the process open.
    if let Some(handle) = metrics_server {
        match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
            Ok(Ok(Err(e))) => tracing::warn!(error = %e, "Metrics listener exited with an error"),
            Ok(Err(e)) => tracing::warn!(error = %e, "Metrics listener task panicked"),
            Ok(Ok(Ok(()))) => tracing::info!("Metrics listener stopped"),
            Err(_) => tracing::warn!("Metrics listener did not stop within 5s; abandoning it"),
        }
    }

    // Graceful shutdown
    if let Some(handle) = state.kafka_consumer_handle.lock().await.take() {
        tracing::info!("Shutting down Kafka consumer...");
        handle.shutdown().await;
    }

    // Release the state's trace-queue sender before draining the workers —
    // they exit when the last sender closes, and holding `state` here would
    // stall the drain until its timeout.
    drop(state);
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
