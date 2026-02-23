use std::sync::Arc;

use clap::Parser;
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;

use orion::config::{self, LogFormat};
use orion::connector::ConnectorRegistry;
use orion::server::state::AppState;
use orion::storage::repositories::connectors::SqliteConnectorRepository;
use orion::storage::repositories::jobs::SqliteJobRepository;
use orion::storage::repositories::rules::{RuleRepository, SqliteRuleRepository, rule_to_workflow};

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

    // Init tracing
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.logging.level));

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

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "Starting Orion Rules Engine"
    );

    // Init database
    let pool = orion::storage::init_pool(&config.storage.path).await?;
    tracing::info!(path = %config.storage.path, "Database initialized");

    // Create repositories
    let rule_repo = Arc::new(SqliteRuleRepository::new(pool.clone()));
    let connector_repo = Arc::new(SqliteConnectorRepository::new(pool.clone()));
    let job_repo = Arc::new(SqliteJobRepository::new(pool.clone()));

    // Load connectors
    let connector_registry = Arc::new(ConnectorRegistry::new());
    let connector_count = connector_registry
        .load_from_repo(connector_repo.as_ref())
        .await?;
    tracing::info!(count = connector_count, "Connectors loaded");

    // Build custom function handlers (http_call, enrich, publish_kafka)
    #[allow(unused_mut)]
    let mut custom_functions = orion::engine::build_custom_functions(connector_registry.clone());

    // Kafka producer setup (when kafka feature is enabled)
    #[cfg(feature = "kafka")]
    let kafka_producer = if !config.kafka.brokers.is_empty() {
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
    let mut workflows = Vec::new();
    for rule in &active_rules {
        match rule_to_workflow(rule) {
            Ok(w) => workflows.push(w),
            Err(e) => {
                tracing::warn!(rule_id = %rule.id, error = %e, "Failed to convert rule, skipping");
            }
        }
    }

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

    // Start job queue worker pool
    let (job_queue, worker_handle) = orion::queue::start_workers(
        config.queue.workers,
        config.queue.buffer_size,
        engine.clone(),
        job_repo.clone() as Arc<dyn orion::storage::repositories::jobs::JobRepository>,
    );

    tracing::info!(
        workers = config.queue.workers,
        buffer = config.queue.buffer_size,
        "Job queue started"
    );

    // Build state and router
    let state = AppState {
        engine,
        rule_repo: rule_repo as Arc<dyn orion::storage::repositories::rules::RuleRepository>,
        connector_repo: connector_repo
            as Arc<dyn orion::storage::repositories::connectors::ConnectorRepository>,
        job_repo: job_repo as Arc<dyn orion::storage::repositories::jobs::JobRepository>,
        connector_registry,
        job_queue,
        config: Arc::new(config.clone()),
        start_time: chrono::Utc::now(),
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

    tracing::info!("Shutting down job queue workers...");
    worker_handle.shutdown().await;

    tracing::info!("Orion shut down cleanly");
    Ok(())
}
