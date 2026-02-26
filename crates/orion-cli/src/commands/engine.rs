use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use serde_json::Value;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};

#[derive(Args)]
pub struct EngineCmd {
    #[command(subcommand)]
    command: EngineSubcommand,
}

#[derive(Subcommand)]
enum EngineSubcommand {
    /// Show engine status
    Status,
    /// Reload engine rules
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
        let rules = resp["rules_count"].as_u64().unwrap_or(0);
        println!("{rules}");
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    let version = resp["version"].as_str().unwrap_or("unknown");
    let uptime = resp["uptime_seconds"].as_i64().unwrap_or(0);
    let rules_count = resp["rules_count"].as_u64().unwrap_or(0);
    let active = resp["active_rules"].as_u64().unwrap_or(0);
    let paused = resp["paused_rules"].as_u64().unwrap_or(0);

    println!("{}", "Engine Status".bold());
    println!("  Version:      {version}");
    println!("  Uptime:       {}", format_duration(uptime));
    println!("  Total rules:  {rules_count}");
    println!("  Active:       {}", active.to_string().green());
    println!(
        "  Paused:       {}",
        if paused > 0 {
            paused.to_string().yellow().to_string()
        } else {
            "0".to_string()
        }
    );

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
    if !yes {
        eprint!("Reload engine? [y/N] ");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(0);
        }
    }

    let resp: Value = client.post_empty("/api/v1/admin/engine/reload").await?;

    if !quiet {
        let rules = resp["rules_count"].as_u64().unwrap_or(0);
        println!(
            "{} Engine reloaded with {rules} rule(s)",
            "OK".green().bold()
        );
    }

    Ok(0)
}

fn format_duration(seconds: i64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else if seconds < 86400 {
        format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
    } else {
        format!("{}d {}h", seconds / 86400, (seconds % 86400) / 3600)
    }
}
