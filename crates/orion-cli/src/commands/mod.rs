pub mod audit_logs;
pub mod backups;
pub mod cache;
pub mod completions;
pub mod config;
pub mod cron;
pub mod data;
pub mod deployment_dir;
pub mod dlq;
pub mod engine;
pub mod functions;
pub mod health;
pub mod metrics;
pub mod traces;
pub mod v2;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum Commands {
    /// Manage CLI configuration (server URL, output format, API key)
    Config(config::ConfigCmd),

    /// Check server health, version, and component status
    Health,

    /// Send data to a channel for synchronous or asynchronous processing
    Send(data::SendCmd),

    /// View and monitor execution traces for processed data
    Traces(traces::TracesCmd),

    /// Show what the engine on this node serves
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

    /// Invalidate channel response-cache namespaces
    Cache(cache::CacheCmd),

    /// Inspect and drain the trace dead-letter queue
    Dlq(dlq::DlqCmd),

    /// Inspect scheduled runs of cron channels
    Cron(cron::CronCmd),

    /// Manage immutable v2 deployments, convergence, rollback, and edition preflight
    #[command(alias = "deploy")]
    Deployments(v2::DeploymentsCmd),

    /// Author, lint, pack, upload, admit, and revoke v2 connector provider packages
    Provider(v2::ProviderCmd),

    /// Manage immutable v2 connection revisions and execute audited health probes
    Connections(v2::ConnectionsCmd),
}
