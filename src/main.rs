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

    // Load active rules and build engine
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

    let engine = dataflow_rs::Engine::new(workflows, None);

    // Build state and router
    let state = AppState {
        engine: Arc::new(RwLock::new(Arc::new(engine))),
        rule_repo: rule_repo as Arc<dyn orion::storage::repositories::rules::RuleRepository>,
        connector_repo: connector_repo
            as Arc<dyn orion::storage::repositories::connectors::ConnectorRepository>,
        job_repo: job_repo as Arc<dyn orion::storage::repositories::jobs::JobRepository>,
        connector_registry,
        config: Arc::new(config.clone()),
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

    tracing::info!("Orion shut down cleanly");
    Ok(())
}
