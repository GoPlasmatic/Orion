use std::sync::Arc;

use clap::Parser;
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;
#[cfg(feature = "otel")]
use tracing_subscriber::layer::SubscriberExt;
#[cfg(feature = "otel")]
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
    name = "orion",
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
    orion                              Start with default config\n    \
    orion -c config.toml               Start with a config file\n    \
    orion validate-config              Validate config and show summary\n    \
    orion -c config.toml migrate       Run pending database migrations\n    \
    orion migrate --dry-run            Preview pending migrations\n\n\
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
    let config = config::load_config(cli.config.as_deref())?;

    if cli.config.is_none() {
        eprintln!(
            "Note: no config file specified (-c <path>). Using defaults + ORION_* env overrides."
        );
    }

    // Handle subcommands that exit early (before starting the server)
    match cli.command {
        Some(Command::ValidateConfig) => {
            println!("Configuration is valid.\n");
            println!("  environment:     {}", config.environment);
            println!("  server:          {}:{}", config.server.host, config.server.port);
            println!("  tls:             {}", if config.server.tls.enabled {
                format!("enabled (cert={})", config.server.tls.cert_path)
            } else {
                "disabled".to_string()
            });
            println!("  storage:         {}", config.storage.url);
            println!("  logging:         level={}, format={}", config.logging.level, match config.logging.format {
                config::LogFormat::Json => "json",
                config::LogFormat::Pretty => "pretty",
            });
            println!("  admin_auth:      {}", if config.admin_auth.enabled { "enabled" } else { "disabled" });
            println!("  cors:            {}", config.cors.allowed_origins.join(", "));
            println!("  rate_limiting:   {}", if config.rate_limit.enabled {
                format!("enabled (rps={}, burst={})", config.rate_limit.default_rps, config.rate_limit.default_burst)
            } else {
                "disabled".to_string()
            });
            println!("  queue:           workers={}, buffer={}", config.queue.workers, config.queue.buffer_size);
            println!("  metrics:         {}", if config.metrics.enabled { "enabled" } else { "disabled" });
            println!("  tracing:         {}", if config.tracing.enabled {
                format!("enabled (endpoint={})", config.tracing.otlp_endpoint)
            } else {
                "disabled".to_string()
            });
            #[cfg(feature = "kafka")]
            println!("  kafka:           {}", if config.kafka.enabled {
                format!("enabled (brokers={})", config.kafka.brokers.join(","))
            } else {
                "disabled".to_string()
            });
            let features: &[&str] = &[
                #[cfg(feature = "db-sqlite")]
                "db-sqlite",
                #[cfg(feature = "db-postgres")]
                "db-postgres",
                #[cfg(feature = "db-mysql")]
                "db-mysql",
                #[cfg(feature = "kafka")]
                "kafka",
                #[cfg(feature = "tls")]
                "tls",
                #[cfg(feature = "otel")]
                "otel",
                #[cfg(feature = "swagger-ui")]
                "swagger-ui",
                #[cfg(feature = "connectors-sql")]
                "connectors-sql",
                #[cfg(feature = "connectors-mongodb")]
                "connectors-mongodb",
                #[cfg(feature = "connectors-redis")]
                "connectors-redis",
            ];
            if features.is_empty() {
                println!("  features:        none");
            } else {
                println!("  features:        {}", features.join(", "));
            }
            return Ok(());
        }
        Some(Command::Migrate { dry_run }) => {
            let pool = orion::storage::init_pool_no_migrate(&config.storage).await?;
            if dry_run {
                let pending = orion::storage::pending_migrations(&pool).await?;
                if pending.is_empty() {
                    println!("No pending migrations.");
                } else {
                    println!("Pending migrations ({}):", pending.len());
                    for (version, description) in &pending {
                        println!("  {} — {}", version, description);
                    }
                }
            } else {
                let pending = orion::storage::pending_migrations(&pool).await?;
                if pending.is_empty() {
                    println!("No pending migrations.");
                } else {
                    println!("Applying {} migration(s)...", pending.len());
                    orion::storage::run_migrations(&pool).await?;
                    println!("Migrations applied successfully.");
                }
            }
            return Ok(());
        }
        None => {} // Continue to start the server
    }

    // Init tracing subscriber with optional OpenTelemetry layer.
    //
    // When the `otel` feature is compiled in and `tracing.enabled = true`,
    // an additional OpenTelemetry layer is added that exports all spans via
    // OTLP. Existing `#[instrument]` annotations automatically become
    // distributed-trace-compatible with zero changes.
    #[cfg(feature = "otel")]
    let _otel_provider = {
        let env_filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(&config.logging.level));
        if config.tracing.enabled {
            let (provider, tracer) = orion::server::otel::init_otel_pipeline(&config.tracing)?;
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

    #[cfg(not(feature = "otel"))]
    {
        init_fmt_subscriber(&config.logging.level, &config.logging.format);
        if config.tracing.enabled {
            eprintln!(
                "WARNING: tracing.enabled=true but Orion was compiled without the `otel` feature. \
                 Rebuild with `--features otel` to enable OpenTelemetry trace export."
            );
        }
    }

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
    #[cfg(feature = "connectors-sql")]
    sqlx::any::install_default_drivers();

    // Init database
    let pool = orion::storage::init_pool(&config.storage).await?;
    tracing::info!(path = %config.storage.url, "Database initialized");

    // Create repositories
    let workflow_repo = Arc::new(SqlWorkflowRepository::new(pool.clone()));
    let channel_repo = Arc::new(SqlChannelRepository::new(pool.clone()));
    let connector_repo = Arc::new(SqlConnectorRepository::new(pool.clone()));
    let trace_repo = Arc::new(SqlTraceRepository::new(pool.clone()));
    let audit_log_repo = Arc::new(
        orion::storage::repositories::audit_logs::SqlAuditLogRepository::new(pool.clone()),
    );

    // Channel registry
    let channel_registry = Arc::new(orion::channel::ChannelRegistry::new());

    // Load connectors
    let connector_registry = Arc::new(ConnectorRegistry::new(
        config.engine.circuit_breaker.clone(),
    ));
    let connector_count = connector_registry
        .load_from_repo(connector_repo.as_ref())
        .await?;
    tracing::info!(count = connector_count, "Connectors loaded");

    // Create a shared HTTP client
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            config.engine.global_http_timeout_secs,
        ))
        .build()
        .map_err(|e| {
            orion::errors::OrionError::Internal(format!("Failed to build HTTP client: {e}"))
        })?;

    // Create the engine lock early so channel_call handler can reference it.
    // We'll populate it with the real engine after building workflows.
    let engine: Arc<RwLock<Arc<dataflow_rs::Engine>>> = Arc::new(RwLock::new(Arc::new(
        dataflow_rs::Engine::new(vec![], None),
    )));

    // Build cache pool (memory backend always available, redis feature-gated)
    let cache_pool = Arc::new(orion::connector::cache_backend::CachePool::new(
        #[cfg(feature = "connectors-redis")]
        config.engine.max_pool_cache_entries,
        config.engine.cache_cleanup_interval_secs,
    ));

    // Create external connector pool caches (shared with AppState for eviction on update/delete)
    #[cfg(feature = "connectors-sql")]
    let sql_pool_cache = Arc::new(orion::connector::pool_cache::SqlPoolCache::new(
        config.engine.max_pool_cache_entries,
    ));
    #[cfg(feature = "connectors-mongodb")]
    let mongo_pool_cache = Arc::new(orion::connector::mongo_pool::MongoPoolCache::new(
        config.engine.max_pool_cache_entries,
    ));

    // Build custom function handlers (http_call, channel_call, cache_read, cache_write, etc.)
    #[allow(unused_mut)]
    let mut custom_functions = orion::engine::build_custom_functions(
        connector_registry.clone(),
        http_client.clone(),
        engine.clone(),
        &config.engine,
        cache_pool.clone(),
        #[cfg(feature = "connectors-sql")]
        sql_pool_cache.clone(),
        #[cfg(feature = "connectors-mongodb")]
        mongo_pool_cache.clone(),
    );

    // Kafka producer setup (when kafka feature is enabled)
    #[cfg(feature = "kafka")]
    let kafka_producer = if config.kafka.enabled && !config.kafka.brokers.is_empty() {
        let producer = Arc::new(orion::kafka::producer::KafkaProducer::new(
            &config.kafka.brokers.join(","),
        )?);
        orion::engine::register_kafka_publisher(
            &mut custom_functions,
            connector_registry.clone(),
            producer.clone(),
        );
        tracing::info!("Kafka producer initialized");
        Some(producer)
    } else {
        None
    };

    // Load active channels and workflows, build engine
    let channels = channel_repo.list_active().await?;
    let channels = orion::engine::filter_channels(channels, &config.channels);
    let active_workflows = workflow_repo.list_active().await?;
    let workflows = orion::engine::build_engine_workflows(&channels, &active_workflows);
    channel_registry
        .reload(&channels, &connector_registry, &cache_pool)
        .await;

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
    let built_engine = dataflow_rs::Engine::new(workflows, Some(custom_functions));
    *orion::engine::acquire_engine_write(&engine).await = Arc::new(built_engine);

    // Mark the service as ready now that the engine and channel registry are loaded
    ready.store(true, std::sync::atomic::Ordering::Release);

    // Start Kafka consumer (when kafka feature is enabled and configured).
    // Also load async channel topic mappings from the database.
    #[cfg(feature = "kafka")]
    let kafka_consumer_handle = if config.kafka.enabled {
        // Merge config-file topics with DB-driven async channels
        let mut all_topics = config.kafka.topics.clone();
        for ch in &channels {
            if (ch.protocol == orion::storage::models::ChannelProtocol::Kafka.as_str()
                || ch.channel_type == "async")
                && let Some(ref topic) = ch.topic
            {
                // Only add if not already mapped from config file
                if !all_topics.iter().any(|t| t.topic == *topic) {
                    all_topics.push(orion::config::TopicMapping {
                        topic: topic.clone(),
                        channel: ch.name.clone(),
                    });
                }
            }
        }

        if !all_topics.is_empty() {
            let merged_config = orion::config::KafkaIngestConfig {
                topics: all_topics,
                ..config.kafka.clone()
            };

            let dlq_producer = if config.kafka.dlq.enabled {
                kafka_producer.clone()
            } else {
                None
            };
            let dlq_topic = if config.kafka.dlq.enabled {
                Some(config.kafka.dlq.topic.clone())
            } else {
                None
            };

            let handle = orion::kafka::consumer::start_consumer(
                &merged_config,
                engine.clone(),
                dlq_producer,
                dlq_topic,
            )?;

            tracing::info!(
                config_topics = config.kafka.topics.len(),
                db_topics = merged_config.topics.len() - config.kafka.topics.len(),
                total_topics = merged_config.topics.len(),
                group_id = %config.kafka.group_id,
                "Kafka consumer started"
            );

            Some(handle)
        } else {
            None
        }
    } else {
        None
    };

    // Start trace queue worker pool (with DLQ for failed async traces)
    let dlq_repo: Option<Arc<dyn orion::storage::repositories::trace_dlq::TraceDlqRepository>> =
        Some(Arc::new(
            orion::storage::repositories::trace_dlq::SqlTraceDlqRepository::new(pool.clone()),
        ));
    let (trace_queue, worker_handle) = orion::queue::start_workers(
        &config.queue,
        engine.clone(),
        trace_repo.clone() as Arc<dyn orion::storage::repositories::traces::TraceRepository>,
        dlq_repo,
    );

    tracing::info!(
        workers = config.queue.workers,
        buffer = config.queue.buffer_size,
        "Trace queue started"
    );

    // Start trace cleanup task
    let trace_cleanup_handle = orion::queue::start_trace_cleanup(
        config.queue.trace_retention_hours,
        config.queue.trace_cleanup_interval_secs,
        trace_repo.clone() as Arc<dyn orion::storage::repositories::traces::TraceRepository>,
    );

    // Start DLQ retry consumer
    let dlq_retry_handle = if config.queue.dlq_retry_enabled {
        let dlq_for_retry: Arc<dyn orion::storage::repositories::trace_dlq::TraceDlqRepository> =
            Arc::new(
                orion::storage::repositories::trace_dlq::SqlTraceDlqRepository::new(pool.clone()),
            );
        let handle = orion::queue::start_dlq_retry(
            config.queue.dlq_poll_interval_secs,
            dlq_for_retry,
            trace_queue.clone(),
            trace_repo.clone() as Arc<dyn orion::storage::repositories::traces::TraceRepository>,
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

    #[cfg(feature = "kafka")]
    let kafka_consumer_handle_arc = Arc::new(tokio::sync::Mutex::new(kafka_consumer_handle));

    let state = AppState {
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
        datalogic: Arc::new(datalogic_rs::DataLogic::new()),
        rate_limit_state,
        ready,
        #[cfg(feature = "connectors-sql")]
        sql_pool_cache,
        #[cfg(feature = "connectors-mongodb")]
        mongo_pool_cache,
        #[cfg(feature = "kafka")]
        kafka_consumer_handle: kafka_consumer_handle_arc.clone(),
        #[cfg(feature = "kafka")]
        kafka_producer,
    };

    let router = orion::server::build_router(state);

    let addr = format!("{}:{}", config.server.host, config.server.port);

    // Warn if TLS is configured but the feature is not compiled
    #[cfg(not(feature = "tls"))]
    if config.server.tls.enabled {
        tracing::warn!(
            "server.tls.enabled=true but Orion was compiled without the `tls` feature. \
             Rebuild with `--features tls` to enable HTTPS. Starting in plain HTTP mode."
        );
    }

    #[cfg(feature = "tls")]
    if config.server.tls.enabled {
        let rustls_config = orion::server::tls::load_rustls_config(
            &config.server.tls.cert_path,
            &config.server.tls.key_path,
        )
        .await?;

        let handle = axum_server::Handle::new();
        let shutdown_handle = handle.clone();
        let drain_secs = config.server.shutdown_drain_secs;
        tokio::spawn(async move {
            orion::server::shutdown_signal().await;
            shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(drain_secs)));
        });

        tracing::info!(
            address = %addr,
            storage = %config.storage.url,
            tls = true,
            "Orion is ready (HTTPS)"
        );

        axum_server::bind_rustls(addr.parse()?, rustls_config)
            .handle(handle)
            .serve(router.into_make_service())
            .await?;
    } else {
        serve_plain_http(
            &addr,
            &config.storage.url,
            config.server.shutdown_drain_secs,
            router,
        )
        .await?;
    }

    #[cfg(not(feature = "tls"))]
    {
        serve_plain_http(
            &addr,
            &config.storage.url,
            config.server.shutdown_drain_secs,
            router,
        )
        .await?;
    }

    // Graceful shutdown
    #[cfg(feature = "kafka")]
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

    tracing::info!("Shutting down trace queue workers...");
    worker_handle.shutdown().await;

    // Flush pending OTel spans before exit
    #[cfg(feature = "otel")]
    if let Some(provider) = _otel_provider {
        tracing::info!("Flushing OpenTelemetry spans...");
        if let Err(e) = provider.shutdown() {
            tracing::warn!(error = %e, "Error shutting down OTel tracer provider");
        }
    }

    tracing::info!("Orion shut down cleanly");
    Ok(())
}

/// Bind a plain (non-TLS) HTTP listener and serve `router` with graceful
/// shutdown.  Extracted from the duplicated `#[cfg(feature = "tls")] else` and
/// `#[cfg(not(feature = "tls"))]` code paths.
async fn serve_plain_http(
    addr: &str,
    storage_url: &str,
    drain_secs: u64,
    router: axum::Router,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        orion::errors::OrionError::InternalSource {
            context: format!("Failed to bind to {addr}"),
            source: Box::new(e),
        }
    })?;
    tracing::info!(
        address = %addr,
        storage = %storage_url,
        "Orion is ready"
    );
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            orion::server::shutdown_signal().await;
            tracing::info!(drain_secs, "Starting HTTP connection drain");
            tokio::time::sleep(std::time::Duration::from_secs(drain_secs)).await;
        })
        .await?;
    Ok(())
}
