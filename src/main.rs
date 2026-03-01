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
use orion::storage::repositories::connectors::SqliteConnectorRepository;
use orion::storage::repositories::rules::{RuleRepository, SqliteRuleRepository};
use orion::storage::repositories::traces::SqliteTraceRepository;

#[derive(Parser)]
#[command(name = "orion", version, about = "Orion Rules Engine Service")]
struct Cli {
    /// Path to TOML configuration file
    #[arg(short, long)]
    config: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Load configuration
    let config = config::load_config(cli.config.as_deref())?;

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
            match config.logging.format {
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
            None
        }
    };

    #[cfg(not(feature = "otel"))]
    {
        let env_filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(&config.logging.level));
        match config.logging.format {
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
        if config.tracing.enabled {
            eprintln!(
                "WARNING: tracing.enabled=true but Orion was compiled without the `otel` feature. \
                 Rebuild with `--features otel` to enable OpenTelemetry trace export."
            );
        }
    }

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "Starting Orion Rules Engine"
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

    // Init database
    let pool = orion::storage::init_pool(&config.storage).await?;
    tracing::info!(path = %config.storage.path, "Database initialized");

    // Create repositories
    let rule_repo = Arc::new(SqliteRuleRepository::new(pool.clone()));
    let connector_repo = Arc::new(SqliteConnectorRepository::new(pool.clone()));
    let trace_repo = Arc::new(SqliteTraceRepository::new(pool.clone()));

    // Load connectors
    let connector_registry = Arc::new(ConnectorRegistry::new(
        config.engine.circuit_breaker.clone(),
    ));
    let connector_count = connector_registry
        .load_from_repo(connector_repo.as_ref())
        .await?;
    tracing::info!(count = connector_count, "Connectors loaded");

    // Create a shared HTTP client
    let http_client = reqwest::Client::new();

    // Build custom function handlers (http_call, publish_kafka)
    #[allow(unused_mut)]
    let mut custom_functions =
        orion::engine::build_custom_functions(connector_registry.clone(), http_client.clone());

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

    // Load active rules and build engine with custom functions
    let active_rules = rule_repo.list_active().await?;
    let workflows = orion::engine::build_engine_workflows(&active_rules);

    let channels: std::collections::HashSet<&str> =
        workflows.iter().map(|w| w.channel.as_str()).collect();

    tracing::info!(
        rules = active_rules.len(),
        channels = channels.len(),
        "Rules loaded"
    );

    let engine = dataflow_rs::Engine::new(workflows, Some(custom_functions));
    let engine = Arc::new(RwLock::new(Arc::new(engine)));

    // Start Kafka consumer (when kafka feature is enabled and configured)
    #[cfg(feature = "kafka")]
    let kafka_consumer_handle = if config.kafka.enabled && !config.kafka.topics.is_empty() {
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
            &config.kafka,
            engine.clone(),
            dlq_producer,
            dlq_topic,
        )?;

        tracing::info!(
            topics = config.kafka.topics.len(),
            group_id = %config.kafka.group_id,
            "Kafka consumer started"
        );

        Some(handle)
    } else {
        None
    };

    // Start trace queue worker pool
    let (trace_queue, worker_handle) = orion::queue::start_workers(
        config.queue.workers,
        config.queue.buffer_size,
        config.queue.shutdown_timeout_secs,
        engine.clone(),
        trace_repo.clone() as Arc<dyn orion::storage::repositories::traces::TraceRepository>,
    );

    tracing::info!(
        workers = config.queue.workers,
        buffer = config.queue.buffer_size,
        "Trace queue started"
    );

    // Set initial active rules gauge
    orion::metrics::set_active_rules(active_rules.len() as f64);

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
    let state = AppState {
        engine,
        rule_repo: rule_repo as Arc<dyn orion::storage::repositories::rules::RuleRepository>,
        connector_repo: connector_repo
            as Arc<dyn orion::storage::repositories::connectors::ConnectorRepository>,
        trace_repo: trace_repo as Arc<dyn orion::storage::repositories::traces::TraceRepository>,
        connector_registry,
        trace_queue,
        config: config.clone(),
        start_time: chrono::Utc::now(),
        metrics_handle,
        http_client,
        rate_limit_state,
    };

    let router = orion::server::build_router(state);

    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    tracing::info!(
        address = %addr,
        storage = %config.storage.path,
        "Orion is ready"
    );

    axum::serve(listener, router)
        .with_graceful_shutdown(orion::server::shutdown_signal())
        .await?;

    // Graceful shutdown
    #[cfg(feature = "kafka")]
    if let Some(handle) = kafka_consumer_handle {
        tracing::info!("Shutting down Kafka consumer...");
        handle.shutdown().await;
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
