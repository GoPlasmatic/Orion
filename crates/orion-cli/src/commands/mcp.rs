use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use colored::Colorize;

use crate::Cli;
use crate::config::OrionConfig;

#[derive(Args)]
pub struct McpCmd {
    #[command(subcommand)]
    command: McpCommands,
}

#[derive(Subcommand)]
enum McpCommands {
    /// Start MCP server (stdio or HTTP transport)
    Serve(ServeArgs),
}

#[derive(Parser)]
pub struct ServeArgs {
    /// Use HTTP transport instead of stdio
    #[arg(long)]
    http: bool,

    /// Bind address for HTTP mode
    #[arg(long, default_value = "0.0.0.0:8081")]
    bind: String,
}

impl McpCmd {
    pub async fn run(&self, cli: &Cli) -> Result<i32> {
        match &self.command {
            McpCommands::Serve(args) => {
                let server_url = if let Some(url) = &cli.server {
                    url.clone()
                } else {
                    OrionConfig::resolve_server_url().map_err(|_| {
                        anyhow::anyhow!(
                            "No server URL configured. Run {} or use {}",
                            "orion-cli config set-server <url>".yellow(),
                            "--server <url>".yellow()
                        )
                    })?
                };

                crate::mcp::serve(server_url, args.http, args.bind.clone()).await?;
                Ok(0)
            }
        }
    }
}
