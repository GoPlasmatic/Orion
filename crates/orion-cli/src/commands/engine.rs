use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use serde_json::Value;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};
use crate::utils;

#[derive(Args)]
pub struct EngineCmd {
    #[command(subcommand)]
    command: EngineSubcommand,
}

#[derive(Subcommand)]
enum EngineSubcommand {
    /// Show engine status
    Status,
    /// Reload engine
    Reload,
}

impl EngineCmd {
    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
        yes: bool,
    ) -> Result<i32> {
        match &self.command {
            EngineSubcommand::Status => status(client, format, quiet).await,
            EngineSubcommand::Reload => reload(client, quiet, yes).await,
        }
    }
}

async fn status(client: &OrionClient, format: &OutputFormat, quiet: bool) -> Result<i32> {
    let resp: Value = client.get("/api/v1/admin/engine/status").await?;

    if quiet {
        let workflows = resp["workflows_count"].as_u64().unwrap_or(0);
        println!("{workflows}");
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    let version = resp["version"].as_str().unwrap_or("unknown");
    let uptime = resp["uptime_seconds"].as_i64().unwrap_or(0);
    let workflows_count = resp["workflows_count"].as_u64().unwrap_or(0);
    let active = resp["active_workflows"].as_u64().unwrap_or(0);

    println!("{}", "Engine Status".bold());
    println!("  Version:         {version}");
    println!("  Uptime:          {}", utils::format_duration(uptime));
    println!("  Total workflows: {workflows_count}");
    println!("  Active:          {}", active.to_string().green());

    if let Some(channels) = resp.get("channels").and_then(|c| c.as_array()) {
        let channel_names: Vec<&str> = channels.iter().filter_map(|c| c.as_str()).collect();
        println!(
            "  Channels:     {}",
            if channel_names.is_empty() {
                "(none)".dimmed().to_string()
            } else {
                channel_names.join(", ")
            }
        );
    }

    Ok(0)
}

async fn reload(client: &OrionClient, quiet: bool, yes: bool) -> Result<i32> {
    if !utils::confirm("Reload engine?", yes)? {
        println!("Cancelled.");
        return Ok(0);
    }

    let resp: Value = client.post_empty("/api/v1/admin/engine/reload").await?;

    if !quiet {
        let workflows = resp["workflows_count"].as_u64().unwrap_or(0);
        println!(
            "{} Engine reloaded with {workflows} workflow(s)",
            "OK".green().bold()
        );
    }

    Ok(0)
}
