use std::sync::Arc;

use clap::Parser;
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use orion::config::{self, LogFormat};
use orion::connector::ConnectorRegistry;
use orion::server::state::AppState;
use orion::storage::repositories::channels::{ChannelRepository, SqlChannelRepository};
use orion::storage::repositories::connectors::SqlConnectorRepository;
use orion::storage::repositories::traces::SqlTraceRepository;
use orion::storage::repositories::workflows::{SqlWorkflowRepository, WorkflowRepository};

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

/// Construct the Kafka producer and wire it into the engine's
/// `publish_kafka` handler. Returns `None` when Kafka is disabled or no
/// brokers are configured.
fn setup_kafka_producer(
    kafka_config: &config::KafkaIngestConfig,
    custom_functions: &mut std::collections::HashMap<String, dataflow_rs::BoxedFunctionHandler>,
    connector_registry: Arc<ConnectorRegistry>,
) -> Result<Option<Arc<orion::kafka::producer::KafkaProducer>>, Box<dyn std::error::Error>> {
    if !kafka_config.enabled || kafka_config.brokers.is_empty() {
        return Ok(None);
    }
    let producer = Arc::new(orion::kafka::producer::KafkaProducer::new(
        &kafka_config.brokers.join(","),
    )?);
    orion::engine::register_kafka_publisher(custom_functions, connector_registry, producer.clone());
    tracing::info!("Kafka producer initialized");
    Ok(Some(producer))
}

/// Start the Kafka consumer in a background task. Merges config-file topic
/// mappings with DB-driven async-channel topics. Returns `None` when Kafka
/// is disabled or the merged topic list is empty.
fn start_kafka_ingest(
    kafka_config: &config::KafkaIngestConfig,
    channels: &[orion::storage::models::Channel],
    engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    channel_registry: Arc<orion::channel::ChannelRegistry>,
    datalogic: Arc<datalogic_rs::Engine>,
    kafka_producer: Option<Arc<orion::kafka::producer::KafkaProducer>>,
    instance_id: Option<&str>,
) -> Result<Option<orion::kafka::consumer::ConsumerHandle>, Box<dyn std::error::Error>> {
    if !kafka_config.enabled {
        return Ok(None);
    }

    // Merge config-file topics with DB-driven async channels.
    let mut all_topics = kafka_config.topics.clone();
    for ch in channels {
        if (ch.protocol == orion::storage::models::ChannelProtocol::Kafka.as_str()
            || ch.channel_type == "async")
            && let Some(ref topic) = ch.topic
            && !all_topics.iter().any(|t| t.topic == *topic)
        {
            all_topics.push(orion::config::TopicMapping {
                topic: topic.clone(),
                channel: ch.name.clone(),
            });
        }
    }

    if all_topics.is_empty() {
        return Ok(None);
    }

    let merged_config = orion::config::KafkaIngestConfig {
        topics: all_topics,
        ..kafka_config.clone()
    };

    let (dlq_producer, dlq_topic) = if kafka_config.dlq.enabled {
        (kafka_producer, Some(kafka_config.dlq.topic.clone()))
    } else {
        (None, None)
    };

    let handle = orion::kafka::consumer::start_consumer(
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

/// `validate-config` subcommand: report parsed configuration and exit.
fn handle_validate_config(config: &config::AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    println!("Configuration is valid.\n");
    println!("  environment:     {}", config.environment);
    println!(
        "  server:          {}:{}",
        config.server.host, config.server.port
    );
    println!(
        "  tls:             {}",
        if config.server.tls.enabled {
            format!("enabled (cert={})", config.server.tls.cert_path)
        } else {
            "disabled".to_string()
        }
    );
    println!("  storage:         {}", config.storage.url);
    println!(
        "  logging:         level={}, format={}",
        config.logging.level,
        match config.logging.format {
            config::LogFormat::Json => "json",
            config::LogFormat::Pretty => "pretty",
        }
    );
    println!(
        "  admin_auth:      {}",
        if config.admin_auth.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "  cors:            {}",
        config.cors.allowed_origins.join(", ")
    );
    println!(
        "  rate_limiting:   {}",
        if config.rate_limit.enabled {
            format!(
                "enabled (rps={}, burst={})",
                config.rate_limit.default_rps, config.rate_limit.default_burst
            )
        } else {
            "disabled".to_string()
        }
    );
    println!(
        "  queue:           workers={}, buffer={}",
        config.queue.workers, config.queue.buffer_size
    );
    println!(
        "  metrics:         {}",
        if config.metrics.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "  tracing:         {}",
        if config.tracing.enabled {
            format!("enabled (endpoint={})", config.tracing.otlp_endpoint)
        } else {
            "disabled".to_string()
        }
    );
    println!(
        "  kafka:           {}",
        if config.kafka.enabled {
            format!("enabled (brokers={})", config.kafka.brokers.join(","))
        } else {
            "disabled".to_string()
        }
    );
    let features: &[&str] = &[
        "db-sqlite",
        "db-postgres",
        "db-mysql",
        "kafka",
        "tls",
        "otel",
        "swagger-ui",
        "connectors-sql",
        "connectors-mongodb",
        "connectors-redis",
    ];
    println!("  features:        {}", features.join(", "));
    Ok(())
}

/// `migrate [--dry-run]` subcommand: list or apply pending DB migrations.
async fn handle_migrate(
    config: &config::AppConfig,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let pool = orion::storage::init_pool_no_migrate(&config.storage).await?;
    let pending = orion::storage::pending_migrations(&pool).await?;

    if pending.is_empty() {
        println!("No pending migrations.");
        return Ok(());
    }

    if dry_run {
        println!("Pending migrations ({}):", pending.len());
        for (version, description) in &pending {
            println!("  {version} — {description}");
        }
    } else {
        println!("Applying {} migration(s)...", pending.len());
        orion::storage::run_migrations(&pool).await?;
        println!("Migrations applied successfully.");
    }
    Ok(())
}

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
        Some(Command::ValidateConfig) => return handle_validate_config(&config),
        Some(Command::Migrate { dry_run }) => return handle_migrate(&config, dry_run).await,
        Some(Command::Lint { workflow }) => return run_lint(&workflow),
        Some(Command::DryRun { workflow, input }) => return run_dry_run(&workflow, &input).await,
        Some(Command::TestConnectivity) => return run_test_connectivity(&config).await,
        Some(Command::DumpOpenapi) => return run_dump_openapi(),
        None => {} // Continue to start the server
    }

    // Init tracing subscriber with optional OpenTelemetry layer.
    //
    // When `tracing.enabled = true`, an additional OpenTelemetry layer is added
    // that exports all spans via OTLP. Existing `#[instrument]` annotations
    // automatically become distributed-trace-compatible with zero changes.
    let _otel_provider = {
        let env_filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(&config.logging.level));
        if config.tracing.enabled {
            let (provider, tracer) = orion::server::otel::init_otel_pipeline(
                &config.tracing,
                &config.cluster.instance_id,
            )?;
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
            Some(provider)
        } else {
            init_fmt_subscriber(&config.logging.level, &config.logging.format);
            None
        }
    };

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        git_hash = env!("GIT_HASH"),
        build_timestamp = env!("BUILD_TIMESTAMP"),
        environment = %config.environment,
        "Starting Orion — Declarative Services Runtime"
    );

    // Init metrics (gated by config)
    let metrics_handle = if config.metrics.enabled {
        let handle = orion::metrics::init_metrics();
        tracing::info!("Prometheus metrics initialized");
        handle
    } else {
        // Create a no-op handle that still works but doesn't install a global recorder
        metrics_exporter_prometheus::PrometheusBuilder::new()
            .build_recorder()
            .handle()
    };

    // Install sqlx Any drivers for external connector pools (must be before any pool creation)
    sqlx::any::install_default_drivers();

    // Init database. With auto_migrate = false (multi-replica deploys) a
    // stale schema is a hard startup error — a replica must never serve
    // against pending migrations; `orion-server migrate` is the deploy step.
    let pool = if config.storage.auto_migrate {
        orion::storage::init_pool(&config.storage).await?
    } else {
        let pool = orion::storage::init_pool_no_migrate(&config.storage).await?;
        let pending = orion::storage::pending_migrations(&pool).await?;
        if !pending.is_empty() {
            return Err(orion::errors::OrionError::Config {
                message: format!(
                    "{} pending migration(s) and storage.auto_migrate = false — \
                     run `orion-server migrate` first",
                    pending.len()
                ),
            }
            .into());
        }
        pool
    };
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
    let workflow_repo = Arc::new(SqlWorkflowRepository::new(pool.clone()));
    let channel_repo = Arc::new(SqlChannelRepository::new(pool.clone()));
    let connector_repo = Arc::new(SqlConnectorRepository::new(pool.clone()));
    let trace_repo = Arc::new(SqlTraceRepository::new(pool.clone()));
    let audit_log_repo = Arc::new(
        orion::storage::repositories::audit_logs::SqlAuditLogRepository::new(pool.clone()),
    );

    // Channel registry
    let channel_registry = Arc::new(if config.cluster.enabled {
        orion::channel::ChannelRegistry::with_cluster((&*cluster).into())
    } else {
        orion::channel::ChannelRegistry::new()
    });

    // Load connectors
    let connector_registry = Arc::new(ConnectorRegistry::new(
        config.engine.circuit_breaker.clone(),
    ));
    let connector_count = connector_registry
        .load_from_repo(connector_repo.as_ref())
        .await?;
    tracing::info!(count = connector_count, "Connectors loaded");

    // Create a shared HTTP client. Redirects are off: execute_request follows
    // them manually with per-hop SSRF validation. The pinned resolver connects
    // to the exact addresses SSRF validation vetted (no DNS rebinding between
    // check and connect).
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            config.engine.global_http_timeout_secs,
        ))
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(std::sync::Arc::new(orion::validation::PinnedDnsResolver))
        .build()
        .map_err(|e| {
            orion::errors::OrionError::Internal(format!("Failed to build HTTP client: {e}"))
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
    let cache_pool = Arc::new(orion::connector::cache_backend::CachePool::new(
        config.engine.max_pool_cache_entries,
        config.engine.cache_cleanup_interval_secs,
    ));

    // Create external connector pool caches (shared with AppState for eviction on update/delete)
    let sql_pool_cache = Arc::new(orion::connector::pool_cache::SqlPoolCache::new(
        config.engine.max_pool_cache_entries,
    ));
    let mongo_pool_cache = Arc::new(orion::connector::mongo_pool::MongoPoolCache::new(
        config.engine.max_pool_cache_entries,
    ));

    // Build custom function handlers (http_call, channel_call, cache_read, cache_write, etc.)
    let mut custom_functions = orion::engine::build_custom_functions(
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

    // Load active channels and workflows, build engine
    let channels = channel_repo.list_active().await?;
    let channels = orion::engine::filter_channels(channels, &config.channels);
    let active_workflows = workflow_repo.list_active().await?;
    let workflows = orion::engine::build_engine_workflows(&channels, &active_workflows);
    let load_issues = channel_registry
        .reload(
            &channels,
            &connector_registry,
            &cache_pool,
            &datalogic_engine,
            &config.tracing.storage,
        )
        .await;
    if !load_issues.is_empty() {
        // Cluster mode: a node that cannot build a channel's shared backend
        // must not boot and silently serve with per-node state.
        return Err(orion::channel::ChannelLoadIssue::refusal_error(&load_issues).into());
    }

    let channel_names: std::collections::HashSet<&str> =
        workflows.iter().map(|w| w.channel.as_str()).collect();

    tracing::info!(
        workflows = active_workflows.len(),
        channels = channel_names.len(),
        "Workflows loaded"
    );

    // Readiness flag — set after engine is fully initialized
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Populate the pre-created engine lock with the real engine
    let built_engine = dataflow_rs::Engine::new(workflows, custom_functions)?;
    *orion::engine::acquire_engine_write(&engine).await = Arc::new(built_engine);

    // Mark the service as ready now that the engine and channel registry are loaded
    ready.store(true, std::sync::atomic::Ordering::Release);

    let kafka_consumer_handle = start_kafka_ingest(
        &config.kafka,
        &channels,
        engine.clone(),
        channel_registry.clone(),
        datalogic_engine.clone(),
        kafka_producer.clone(),
        cluster.enabled.then(|| cluster.instance_id.as_str()),
    )?;

    // Start trace persistence queue (async/batch modes). A no-op queue is
    // returned for `sync` / `off`, so callers can submit unconditionally.
    let (trace_persistence_queue, trace_persistence_handle) =
        orion::queue::trace_persistence::start(
            &config.tracing.storage,
            trace_repo.clone() as Arc<dyn orion::storage::repositories::traces::TraceRepository>,
        );
    tracing::info!(
        mode = ?config.tracing.storage.mode,
        max_pending = config.tracing.storage.max_pending,
        "Trace persistence queue started"
    );

    // Start trace queue worker pool (with DLQ for failed async traces).
    // The pool needs the persistence queue + channel registry so it can route
    // status / result writes through the configured mode.
    let dlq_repo: Option<Arc<dyn orion::storage::repositories::trace_dlq::TraceDlqRepository>> =
        Some(Arc::new(
            orion::storage::repositories::trace_dlq::SqlTraceDlqRepository::new(pool.clone()),
        ));
    let (trace_queue, worker_handle) = orion::queue::start_workers(
        &config.queue,
        engine.clone(),
        trace_repo.clone() as Arc<dyn orion::storage::repositories::traces::TraceRepository>,
        dlq_repo,
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
        trace_repo.clone() as Arc<dyn orion::storage::repositories::traces::TraceRepository>,
        job_lease_gate.clone(),
    );

    // Start DLQ retry consumer
    let dlq_retry_handle = if config.queue.dlq_retry_enabled {
        let dlq_for_retry: Arc<dyn orion::storage::repositories::trace_dlq::TraceDlqRepository> =
            Arc::new(
                orion::storage::repositories::trace_dlq::SqlTraceDlqRepository::new(pool.clone()),
            );
        let handle = orion::queue::start_dlq_retry(
            orion::queue::DlqRetryOptions {
                poll_interval_secs: config.queue.dlq_poll_interval_secs,
                batch_size: config.queue.dlq_batch_size,
                lease_secs: config.queue.dlq_lease_secs,
                claimant: cluster.instance_id.clone(),
                lease_gate: job_lease_gate.clone(),
            },
            dlq_for_retry,
            trace_queue.clone(),
            trace_repo.clone() as Arc<dyn orion::storage::repositories::traces::TraceRepository>,
            channel_registry.clone(),
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

    // Set initial active rules gauge
    orion::metrics::set_active_workflows(active_workflows.len() as f64);

    // Build rate limiter (if enabled)
    let rate_limit_state = if config.rate_limit.enabled {
        let rls = orion::server::rate_limit::RateLimitState::from_config(&config.rate_limit);
        tracing::info!(
            default_rps = config.rate_limit.default_rps,
            default_burst = config.rate_limit.default_burst,
            "Rate limiting enabled"
        );
        Some(Arc::new(rls))
    } else {
        None
    };

    // Build state and router
    let config = Arc::new(config);

    let kafka_consumer_handle_arc = Arc::new(tokio::sync::Mutex::new(kafka_consumer_handle));

    let state = AppState::new(orion::server::state::AppStateInner {
        engine,
        channel_repo: channel_repo
            as Arc<dyn orion::storage::repositories::channels::ChannelRepository>,
        workflow_repo: workflow_repo
            as Arc<dyn orion::storage::repositories::workflows::WorkflowRepository>,
        connector_repo: connector_repo
            as Arc<dyn orion::storage::repositories::connectors::ConnectorRepository>,
        trace_repo: trace_repo as Arc<dyn orion::storage::repositories::traces::TraceRepository>,
        audit_log_repo: audit_log_repo
            as Arc<dyn orion::storage::repositories::audit_logs::AuditLogRepository>,
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
    let cluster_task_handles = orion::cluster::start_cluster_tasks(&state);

    let router = orion::server::build_router(state);

    let addr = format!("{}:{}", config.server.host, config.server.port);

    if config.server.tls.enabled {
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
            let force = (force_timeout_secs > 0)
                .then(|| std::time::Duration::from_secs(force_timeout_secs));
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

    if let Some(handle) = trace_cleanup_handle {
        tracing::info!("Stopping trace cleanup task...");
        handle.abort();
    }

    if let Some(handle) = dlq_retry_handle {
        tracing::info!("Stopping DLQ retry consumer...");
        handle.abort();
    }

    for handle in cluster_task_handles {
        handle.abort();
    }

    tracing::info!("Shutting down trace queue workers...");
    worker_handle.shutdown().await;

    tracing::info!("Draining trace persistence queue...");
    trace_persistence_handle.shutdown().await;

    // Flush pending OTel spans before exit
    if let Some(provider) = _otel_provider {
        tracing::info!("Flushing OpenTelemetry spans...");
        if let Err(e) = provider.shutdown() {
            tracing::warn!(error = %e, "Error shutting down OTel tracer provider");
        }
    }

    eprintln!("Orion shut down cleanly");
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

// ============================================================
// A6: CLI subcommands — lint / dry-run / test-connectivity
// ============================================================

/// Lint a workflow JSON file. Mirrors the checks the admin create
/// endpoint performs (`validate_create_workflow` plus A1 function-input
/// schema validation), printing field-pathed errors and exiting non-zero
/// on failure so this can be wired into pre-commit / CI hooks.
fn run_lint(workflow_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    use orion::storage::repositories::workflows::CreateWorkflowRequest;

    let raw = std::fs::read_to_string(workflow_path)
        .map_err(|e| format!("Failed to read '{workflow_path}': {e}"))?;
    let req: CreateWorkflowRequest = serde_json::from_str(&raw)
        .map_err(|e| format!("'{workflow_path}' is not a valid workflow JSON: {e}"))?;

    if let Err(err) = orion::validation::validate_create_workflow(&req) {
        return Err(format_lint_error(workflow_path, err).into());
    }

    println!("'{workflow_path}' is valid.");
    Ok(())
}

/// Print the OpenAPI spec to stdout. Backs the checked-in `docs/openapi.json`
/// (regenerate with `orion-server dump-openapi > docs/openapi.json`) and needs
/// neither config, a database, nor a running server.
fn run_dump_openapi() -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", orion::server::routes::openapi::pretty_json());
    Ok(())
}

/// Format an `OrionError::Validation` into a multi-line CLI message
/// listing each field detail with its path and code.
fn format_lint_error(workflow_path: &str, err: orion::errors::OrionError) -> String {
    use orion::errors::OrionError;
    match err {
        OrionError::Validation {
            code: _,
            message,
            details,
        } => {
            let mut out = format!("'{workflow_path}' is invalid: {message}\n");
            for d in &details {
                out.push_str(&format!("  - {} [{}]: {}\n", d.path, d.code, d.message));
            }
            out
        }
        other => format!("'{workflow_path}' is invalid: {other}"),
    }
}

/// Dry-run a workflow against an input JSON file. Builds a minimal
/// in-process dataflow engine with the supplied workflow and no
/// custom functions — i.e. only built-in `map`/`log`/`filter`/etc.
/// work. Connector-backed tasks will fail with a clear error.
async fn run_dry_run(
    workflow_path: &str,
    input_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use orion::storage::models::{EntityStatus, Workflow};
    use orion::storage::repositories::workflows::{CreateWorkflowRequest, workflow_to_dataflow};

    let raw = std::fs::read_to_string(workflow_path)
        .map_err(|e| format!("Failed to read '{workflow_path}': {e}"))?;
    let req: CreateWorkflowRequest = serde_json::from_str(&raw)
        .map_err(|e| format!("'{workflow_path}' is not a valid workflow JSON: {e}"))?;
    orion::validation::validate_create_workflow(&req)
        .map_err(|e| format_lint_error(workflow_path, e))?;

    let input_raw = std::fs::read_to_string(input_path)
        .map_err(|e| format!("Failed to read input '{input_path}': {e}"))?;
    let input: serde_json::Value = serde_json::from_str(&input_raw)
        .map_err(|e| format!("'{input_path}' is not valid JSON: {e}"))?;

    // Build a synthetic Workflow row from the request to reuse the
    // existing dataflow conversion. Version + timestamps are placeholders.
    let now = chrono::Utc::now().naive_utc();
    let synthetic = Workflow {
        workflow_id: req.workflow_id.clone().unwrap_or_else(|| "dry-run".into()),
        version: 1,
        name: req.name.clone(),
        description: req.description.clone(),
        priority: req.priority,
        status: EntityStatus::Active.as_str().to_string(),
        rollout_percentage: 100,
        condition_json: serde_json::to_string(&req.condition)?,
        tasks_json: serde_json::to_string(&req.tasks)?,
        tags: serde_json::to_string(&req.tags)?,
        continue_on_error: req.continue_on_error,
        created_at: now,
        updated_at: now,
    };
    let df_workflow = workflow_to_dataflow(&synthetic, "__dry_run__")?;
    let engine = dataflow_rs::Engine::new(vec![df_workflow], std::collections::HashMap::new())?;
    let mut message = dataflow_rs::Message::from_value(&input);

    let trace = engine
        .process_message_with_trace(&mut message)
        .await
        .map_err(orion::errors::OrionError::Engine)?;

    let output = serde_json::json!({
        "matched": !trace.steps.is_empty(),
        "trace": trace,
        "output": message.data(),
        "errors": message.errors().iter().filter_map(|e| serde_json::to_value(e).ok()).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

/// Probe the configured backends. Today: opens the database pool
/// and runs migrations check + a no-op query. Avoids the "wrong DB
/// credentials surface only at first request" footgun.
async fn run_test_connectivity(
    config: &config::AppConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Probing storage at {} ...", config.storage.url);
    let pool = orion::storage::init_pool_no_migrate(&config.storage)
        .await
        .map_err(|e| format!("storage: connection failed: {e}"))?;
    let pending = orion::storage::pending_migrations(&pool)
        .await
        .map_err(|e| format!("storage: pending_migrations query failed: {e}"))?;
    println!(
        "  storage:         OK ({} pending migrations)",
        pending.len()
    );
    if config.kafka.enabled {
        println!(
            "  kafka:           configured (brokers={})",
            config.kafka.brokers.join(",")
        );
        println!("                   (Kafka broker reachability probe is not yet implemented)");
    } else {
        println!("  kafka:           disabled");
    }
    Ok(())
}
