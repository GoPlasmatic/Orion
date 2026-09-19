use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use serde_json::Value;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};
use crate::utils;
use orion_client::paths;

#[derive(Args)]
pub struct EngineCmd {
    #[command(subcommand)]
    command: EngineSubcommand,
}

#[derive(Subcommand)]
enum EngineSubcommand {
    /// Show engine status (version, uptime, workflow and channel counts)
    Status,
    /// Hot-reload the engine to apply workflow and channel changes
    #[command(
        long_about = "Hot-reload the engine to apply workflow and channel changes without server restart.\n\n\
            Required after creating, updating, activating, or archiving workflows and channels.\n\
            Changes are not active until this command is run."
    )]
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
    let resp: Value = client.get(paths::ENGINE_STATUS).await?;
    // v1.0 wraps admin responses in {"data": …}; tolerate the bare pre-1.0 shape.
    let resp = resp.get("data").cloned().unwrap_or(resp);

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
    print_load_issues(&resp);

    Ok(0)
}

/// The load issues a status or reload answer carries — absent from a
/// server that predates them, in which case nothing is printed.
fn print_load_issues(resp: &Value) {
    let Some(value) = resp.get("load_issues").filter(|v| !v.is_null()) else {
        return;
    };
    let Ok(issues) = serde_json::from_value::<orion_api::EngineLoadIssues>(value.clone()) else {
        return;
    };
    if issues.is_empty() {
        println!("  Load issues:     {}", "none".green());
        return;
    }
    let lines: Vec<String> = issues
        .channels
        .iter()
        .map(|c| format!("channels/{}: {}", c.channel, c.reason))
        .chain(
            issues
                .plugins
                .iter()
                .map(|p| format!("plugins/{}: {}: {}", p.plugin, p.stage, p.reason)),
        )
        .chain(
            issues
                .models
                .iter()
                .map(|m| format!("models/{}: {}: {}", m.model, m.stage, m.reason)),
        )
        .chain(
            issues
                .connectors
                .iter()
                .map(|c| format!("connectors/{}: {}: {}", c.connector, c.stage, c.reason)),
        )
        .collect();
    println!("  Load issues:     {}", lines.len().to_string().yellow());
    for line in lines {
        println!("    {line}");
    }
}

async fn reload(client: &OrionClient, quiet: bool, yes: bool) -> Result<i32> {
    if !utils::confirm("Reload engine?", yes)? {
        println!("Cancelled.");
        return Ok(0);
    }

    let resp: Value = client.post_empty(paths::ENGINE_RELOAD).await?;
    let resp = resp.get("data").cloned().unwrap_or(resp);

    if !quiet {
        let workflows = resp["workflows_count"].as_u64().unwrap_or(0);
        println!(
            "{} Engine reloaded with {workflows} workflow(s)",
            "OK".green().bold()
        );
        print_load_issues(&resp);
    }

    Ok(0)
}
