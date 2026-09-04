pub mod audit_logs;
pub mod backups;
pub mod benchmark;
pub mod channels;
pub mod completions;
pub mod config;
pub mod connectors;
pub mod data;
pub mod dlq;
pub mod engine;
pub mod functions;
pub mod health;
pub mod metrics;
pub mod packages;
pub mod plugins;
pub mod traces;
pub mod workflows;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum Commands {
    /// Run a performance benchmark against the Orion server
    #[command(alias = "bench")]
    Benchmark(benchmark::BenchmarkCmd),

    /// Manage CLI configuration (server URL, output format, API key)
    Config(config::ConfigCmd),

    /// Check server health, version, and component status
    Health,

    /// Manage workflows -- processing pipelines that transform and route data
    #[command(alias = "rules")]
    Workflows(workflows::WorkflowsCmd),

    /// Manage channels -- service endpoints that receive data and route to workflows
    #[command(alias = "ch")]
    Channels(channels::ChannelsCmd),

    /// Manage connectors -- external service connections (HTTP, Kafka) used by tasks
    #[command(alias = "conn")]
    Connectors(connectors::ConnectorsCmd),

    /// Manage plugins -- sandboxed WebAssembly task functions
    #[command(alias = "plug")]
    Plugins(plugins::PluginsCmd),

    /// Send data to a channel for synchronous or asynchronous processing
    Send(data::SendCmd),

    /// View and monitor execution traces for processed data
    Traces(traces::TracesCmd),

    /// Control the Orion engine -- view status and hot-reload configuration
    #[command(alias = "eng")]
    Engine(engine::EngineCmd),

    /// Inspect workflow task functions registered in the engine
    #[command(alias = "fn")]
    Functions(functions::FunctionsCmd),

    /// View server metrics in Prometheus exposition format
    Metrics(metrics::MetricsCmd),

    /// Generate shell completions for bash, zsh, fish, or powershell
    #[command(alias = "comp")]
    Completions(completions::CompletionsCmd),

    /// View audit logs -- records of all admin actions
    #[command(alias = "audit")]
    AuditLogs(audit_logs::AuditLogsCmd),

    /// Manage database backups (SQLite snapshots)
    Backups(backups::BackupsCmd),

    /// Inspect package promotion receipts (staged/applied versions)
    #[command(alias = "pkg")]
    Packages(packages::PackagesCmd),

    /// Inspect and drain the trace dead-letter queue
    Dlq(dlq::DlqCmd),
}
