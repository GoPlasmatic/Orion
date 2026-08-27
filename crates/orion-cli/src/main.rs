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
        Manage workflows, channels, connectors, data processing, traces, and engine \
        operations on an Orion server.\n\n\
        Core concepts:\n  \
        Workflows   Processing pipelines with condition + task sequences\n  \
        Channels    Service endpoints that receive data and route it to a workflow\n  \
        Connectors  External service connections (HTTP APIs, Kafka) used by tasks\n  \
        Engine      Runs active workflows -- reload after changes to apply them\n\n\
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

    /// Disable colored output
    #[arg(long, global = true, env = "NO_COLOR", help_heading = "Output")]
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

    if cli.no_color {
        colored::control::set_override(false);
    }

    let result = run(cli).await;
    match result {
        Ok(code) => process::exit(code),
        Err(e) => {
            eprintln!("{} {e}", "Error:".red().bold());
            process::exit(1);
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<i32> {
    match &cli.command {
        Commands::Benchmark(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet).await
        }
        Commands::Config(cmd) => {
            cmd.run().await?;
            Ok(0)
        }
        Commands::Health => {
            let client = build_client(&cli)?;
            commands::health::run(&client, &cli.output, cli.quiet).await
        }
        Commands::Workflows(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet, cli.verbose, cli.yes)
                .await
        }
        Commands::Channels(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet, cli.yes).await
        }
        Commands::Connectors(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet, cli.yes).await
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
            cmd.run(&client, &cli.output, cli.quiet, cli.yes).await
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
        Commands::Packages(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet).await
        }
        Commands::Dlq(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet, cli.yes).await
        }
        Commands::Completions(cmd) => {
            cmd.run();
            Ok(0)
        }
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
