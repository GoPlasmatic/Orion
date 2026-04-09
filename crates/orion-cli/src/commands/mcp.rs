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
    #[command(
        long_about = "Start the Model Context Protocol (MCP) server for AI tool integration.\n\n\
            Stdio transport (default): for Claude Desktop, Cursor, and other local AI clients.\n\
            HTTP transport (--http): for remote AI clients over the network.\n\n\
            The MCP server exposes 40+ tools for managing workflows, channels, connectors, \
            data, traces, engine, metrics, audit logs, and backups.",
        after_help = "Examples:\n  \
            orion-cli mcp serve --server http://localhost:8080\n  \
            orion-cli mcp serve --http --bind 0.0.0.0:9090\n\n\
            Claude Desktop config (~/.claude/claude_desktop_config.json):\n  \
            {\"mcpServers\":{\"orion\":{\"command\":\"orion-cli\",\"args\":[\"mcp\",\"serve\",\"--server\",\"http://localhost:8080\"]}}}"
    )]
    Serve(ServeArgs),
}

#[derive(Parser)]
pub struct ServeArgs {
    /// Use HTTP transport instead of stdio (for remote clients)
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

                let api_key = cli
                    .api_key
                    .clone()
                    .map(|k| (k, cli.api_key_header.clone()))
                    .or_else(OrionConfig::resolve_api_key);

                crate::mcp::serve(server_url, args.http, args.bind.clone(), api_key).await?;
                Ok(0)
            }
        }
    }
}
