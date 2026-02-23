mod client;
mod commands;
mod config;
mod output;

use clap::Parser;
use colored::Colorize;
use std::process;

use client::OrionClient;
use commands::Commands;
use config::CliConfig;
use output::OutputFormat;

#[derive(Parser)]
#[command(
    name = "orion-cli",
    version,
    about = "CLI tool for interacting with an Orion rules engine server"
)]
struct Cli {
    /// Orion server URL (overrides config)
    #[arg(long, global = true, env = "ORION_SERVER_URL")]
    server: Option<String>,

    /// Output format
    #[arg(long, global = true, default_value = "table")]
    output: OutputFormat,

    /// Suppress output, print only IDs or minimal info
    #[arg(long, global = true)]
    quiet: bool,

    /// Show full response bodies and extra details
    #[arg(long, global = true)]
    verbose: bool,

    /// Disable colored output
    #[arg(long, global = true, env = "NO_COLOR")]
    no_color: bool,

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
        Commands::Config(cmd) => {
            cmd.run().await?;
            Ok(0)
        }
        Commands::Health => {
            let client = build_client(&cli)?;
            commands::health::run(&client, &cli.output, cli.quiet).await
        }
        Commands::Rules(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet, cli.verbose, cli.yes)
                .await
        }
        Commands::Connectors(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet, cli.yes).await
        }
        Commands::Send(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet, cli.verbose).await
        }
        Commands::Jobs(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet).await
        }
        Commands::Engine(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet, cli.yes).await
        }
        Commands::Metrics(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client).await
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
        let config = CliConfig::load()?;
        config.server_url.ok_or_else(|| {
            anyhow::anyhow!(
                "No server URL configured. Run {} or use {}",
                "orion-cli config set-server <url>".yellow(),
                "--server <url>".yellow()
            )
        })?
    };

    OrionClient::new(&server_url)
}
