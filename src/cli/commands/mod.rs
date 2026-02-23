pub mod completions;
pub mod config;
pub mod connectors;
pub mod data;
pub mod engine;
pub mod health;
pub mod jobs;
pub mod metrics;
pub mod rules;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum Commands {
    /// Manage CLI configuration
    Config(config::ConfigCmd),

    /// Check server health
    Health,

    /// Manage rules
    Rules(rules::RulesCmd),

    /// Manage connectors
    Connectors(connectors::ConnectorsCmd),

    /// Send data for processing
    Send(data::SendCmd),

    /// Monitor async jobs
    Jobs(jobs::JobsCmd),

    /// Engine control
    Engine(engine::EngineCmd),

    /// View server metrics
    Metrics(metrics::MetricsCmd),

    /// Generate shell completions
    Completions(completions::CompletionsCmd),
}
