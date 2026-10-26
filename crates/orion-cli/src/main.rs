mod client;
mod commands;
mod config;
mod help;
mod output;
pub mod utils;

use clap::Parser;
use colored::Colorize;
use std::process;

use client::OrionClient;
use commands::Commands;
use config::OrionConfig;
use output::OutputFormat;

#[derive(Parser)]
#[command(
    name = "orion-cli",
    version,
    long_version = concat!(
        env!("CARGO_PKG_VERSION"),
        "\ngit hash:  ", env!("GIT_HASH"),
        "\nbuilt:     ", env!("BUILD_TIMESTAMP"),
    ),
    about = "CLI tool for the Orion services runtime",
    long_about = "CLI tool for the Orion services runtime.\n\n\
        Deploy definitions to an Orion server, send data through its channels, and \
        inspect traces, the cron ledger and the engine.\n\n\
        Core concepts:\n  \
        Workflows    Processing pipelines with condition + task sequences\n  \
        Channels     Service endpoints that receive data and route it to a workflow\n  \
        Connections  External services (HTTP APIs, databases) a workflow calls\n  \
        Deployments  The committed set of definitions a server serves; \
        `deployments apply -d` sends one\n\n\
        Config precedence: CLI flags > environment variables > ~/.orion/config.toml",
    after_help = help::GETTING_STARTED,
)]
pub struct Cli {
    /// Orion server URL (overrides config)
    #[arg(
        long,
        global = true,
        env = "ORION_SERVER_URL",
        help_heading = "Connection"
    )]
    server: Option<String>,

    /// API key for admin authentication
    #[arg(
        long,
        global = true,
        env = "ORION_API_KEY",
        help_heading = "Connection"
    )]
    api_key: Option<String>,

    /// Header name for API key (default: Authorization with Bearer prefix)
    #[arg(
        long,
        global = true,
        env = "ORION_API_KEY_HEADER",
        help_heading = "Connection"
    )]
    api_key_header: Option<String>,

    /// Output format
    #[arg(long, global = true, default_value = "table", help_heading = "Output")]
    output: OutputFormat,

    /// Suppress output, print only IDs or minimal info
    #[arg(long, global = true, help_heading = "Output")]
    quiet: bool,

    /// Show full response bodies and extra details
    #[arg(long, global = true, help_heading = "Output")]
    verbose: bool,

    /// Disable colored output (also honors nonempty NO_COLOR)
    #[arg(long, global = true, help_heading = "Output")]
    no_color: bool,

    /// Audit label for this change, e.g. "ticket=OPS-4412"
    #[arg(
        long,
        global = true,
        env = "ORION_CHANGE_CONTEXT",
        value_name = "CONTEXT",
        help_heading = "Connection"
    )]
    change_context: Option<String>,

    /// Skip confirmation prompts
    #[arg(long, global = true)]
    yes: bool,

    #[command(subcommand)]
    command: Commands,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    if cli.no_color || std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()) {
        colored::control::set_override(false);
    }

    let result = run(cli).await;
    match result {
        Ok(code) => process::exit(code),
        Err(e) => {
            // `{:#}` prints the whole context chain, so a step that names
            // what it was doing ("uploading 'x'") still shows why it failed.
            eprintln!("{} {e:#}", "Error:".red().bold());
            for detail in error_details(&e) {
                eprintln!("  {}: {} {}", detail.path, detail.code, detail.message);
            }
            process::exit(1);
        }
    }
}

/// The per-field refusals an API error carried, wherever in the chain it is.
fn error_details(e: &anyhow::Error) -> &[orion_api::FieldError] {
    e.chain()
        .find_map(
            |cause| match cause.downcast_ref::<orion_client::ClientError>() {
                Some(orion_client::ClientError::Api { error, .. }) => {
                    Some(error.details.as_slice())
                }
                _ => None,
            },
        )
        .unwrap_or_default()
}

async fn run(cli: Cli) -> anyhow::Result<i32> {
    match &cli.command {
        Commands::Config(cmd) => {
            cmd.run().await?;
            Ok(0)
        }
        Commands::Health => {
            let client = build_client(&cli)?;
            commands::health::run(&client, &cli.output, cli.quiet).await
        }
        Commands::Send(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet, cli.verbose).await
        }
        Commands::Traces(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet).await
        }
        Commands::Engine(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet).await
        }
        Commands::Functions(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet).await
        }
        Commands::Metrics(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client).await
        }
        Commands::AuditLogs(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet).await
        }
        Commands::Backups(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet, cli.yes).await
        }
        Commands::Cache(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet).await
        }
        Commands::Dlq(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet, cli.yes).await
        }
        Commands::Cron(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet).await
        }
        Commands::Deployments(cmd) => {
            let client = build_client(&cli)?;
            commands::v2::require_api_version(&client).await?;
            cmd.run(&client, &cli.output, cli.quiet).await
        }
        Commands::Provider(cmd) => {
            let client = build_client(&cli)?;
            commands::v2::require_api_version(&client).await?;
            cmd.run(&client, &cli.output, cli.quiet).await
        }
        Commands::Connections(cmd) => {
            let client = build_client(&cli)?;
            commands::v2::require_api_version(&client).await?;
            cmd.run(&client, &cli.output, cli.quiet).await
        }
        Commands::Completions(cmd) => {
            cmd.run();
            Ok(0)
        }
        Commands::Lint(cmd) => cmd.run(),
        Commands::Compile(cmd) => cmd.run(),
        Commands::DryRun(cmd) => cmd.run().await,
        Commands::Test(cmd) => cmd.run().await,
        Commands::Fmt(cmd) => cmd.run(),
        Commands::Clippy(cmd) => cmd.run(),
        Commands::Sql(cmd) => cmd.run().await,
        Commands::Plugin(cmd) => cmd.run(),
    }
}

fn build_client(cli: &Cli) -> anyhow::Result<OrionClient> {
    let server_url = if let Some(url) = &cli.server {
        url.clone()
    } else {
        let config = OrionConfig::load()?;
        config.server_url.ok_or_else(|| {
            anyhow::anyhow!(
                "No server URL configured. Run {} or use {}",
                "orion-cli config set-server <url>".yellow(),
                "--server <url>".yellow()
            )
        })?
    };

    // Flag first, then the env var, then `~/.orion/config.toml`. Reading the
    // flag alone would mean `orion-cli config set api_key <key>` stored a key
    // no command ever sent, and every request went out unauthenticated.
    let api_key = cli
        .api_key
        .clone()
        .map(|k| (k, cli.api_key_header.clone()))
        .or_else(OrionConfig::resolve_api_key);

    let mut client = OrionClient::new(&server_url)?;
    if cli.verbose {
        eprintln!("{} {}", "Server:".dimmed(), client.base_url());
    }
    if let Some((key, header)) = api_key {
        client = client.with_api_key(key, header);
    }
    if let Some(context) = &cli.change_context {
        client = client.with_change_context(context.clone());
    }
    // An admin key over plain http to anything but the local machine crosses
    // the network in the clear; say so once rather than let it pass silently.
    // Printed from the URL we were given, not from the keyed client — the
    // text is the same, and nothing derived from the key reaches stderr.
    if client.sends_credential_in_clear() {
        eprintln!(
            "{} API key will be sent over plain http to {} — use https for any server that is not local",
            "Warning:".yellow().bold(),
            server_url.trim_end_matches('/')
        );
    }
    Ok(client)
}
