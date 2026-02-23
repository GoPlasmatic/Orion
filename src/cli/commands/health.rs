use anyhow::Result;
use colored::Colorize;
use serde_json::Value;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};

pub async fn run(client: &OrionClient, format: &OutputFormat, quiet: bool) -> Result<i32> {
    let resp: Value = client.get("/health").await?;

    if quiet {
        let status = resp["status"].as_str().unwrap_or("unknown");
        println!("{status}");
        return Ok(if status == "ok" { 0 } else { 1 });
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        let status = resp["status"].as_str().unwrap_or("unknown");
        return Ok(if status == "ok" { 0 } else { 1 });
    }

    let status = resp["status"].as_str().unwrap_or("unknown");
    let version = resp["version"].as_str().unwrap_or("unknown");
    let uptime = resp["uptime_seconds"].as_i64().unwrap_or(0);
    let rules = resp["rules_loaded"].as_u64().unwrap_or(0);

    let status_display = if status == "ok" {
        "OK".green().bold()
    } else {
        "DEGRADED".red().bold()
    };

    println!(
        "{} {}",
        "Orion Server".bold(),
        format!("v{version}").dimmed()
    );
    println!("  Status:       {status_display}");
    println!("  Uptime:       {}", format_duration(uptime));
    println!("  Rules loaded: {rules}");

    if let Some(components) = resp.get("components").and_then(|c| c.as_object()) {
        println!("  {}", "Components:".bold());
        for (name, val) in components {
            let comp_status = val.as_str().unwrap_or("unknown");
            let indicator = if comp_status == "ok" {
                "OK".green()
            } else {
                "ERROR".red()
            };
            println!("    {name:<12} {indicator}");
        }
    }

    Ok(if status == "ok" { 0 } else { 1 })
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
