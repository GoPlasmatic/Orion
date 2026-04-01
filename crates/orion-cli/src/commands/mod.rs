pub mod channels;
pub mod completions;
pub mod config;
pub mod connectors;
pub mod data;
pub mod engine;
pub mod health;
pub mod mcp;
pub mod metrics;
pub mod traces;
pub mod workflows;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum Commands {
    /// Manage CLI configuration
    Config(config::ConfigCmd),

    /// Check server health
    Health,

    /// Manage workflows
    #[command(alias = "rules")]
    Workflows(workflows::WorkflowsCmd),

    /// Manage channels
    Channels(channels::ChannelsCmd),

    /// Manage connectors
    Connectors(connectors::ConnectorsCmd),

    /// Send data for processing
    Send(data::SendCmd),

    /// View and monitor traces
    Traces(traces::TracesCmd),

    /// Engine control
    Engine(engine::EngineCmd),

    /// View server metrics
    Metrics(metrics::MetricsCmd),

    /// Generate shell completions
    Completions(completions::CompletionsCmd),

    /// MCP server for AI tool integration
    Mcp(mcp::McpCmd),
}
