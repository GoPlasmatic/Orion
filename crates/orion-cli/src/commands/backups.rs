use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use serde_json::Value;
use tabled::Tabled;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};
use crate::utils;

#[derive(Args)]
pub struct BackupsCmd {
    #[command(subcommand)]
    command: BackupsSubcommand,
}

#[derive(Subcommand)]
enum BackupsSubcommand {
    /// Create a database backup (SQLite only)
    Create,
    /// List existing backups
    List,
}

#[derive(Tabled)]
struct BackupRow {
    #[tabled(rename = "Filename")]
    filename: String,
    #[tabled(rename = "Size")]
    size: String,
    #[tabled(rename = "Modified")]
    modified: String,
}

impl BackupsCmd {
    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
        yes: bool,
    ) -> Result<i32> {
        match &self.command {
            BackupsSubcommand::Create => create(client, format, quiet, yes).await,
            BackupsSubcommand::List => list(client, format, quiet).await,
        }
    }
}

async fn create(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    yes: bool,
) -> Result<i32> {
    if !utils::confirm("Create database backup?", yes)? {
        println!("Cancelled.");
        return Ok(0);
    }

    let resp: Value = client.post_empty("/api/v1/admin/backups").await?;

    if quiet {
        let filename = resp["data"]["filename"].as_str().unwrap_or("");
        println!("{filename}");
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    let data = &resp["data"];
    let filename = data["filename"].as_str().unwrap_or("unknown");
    let path = data["path"].as_str().unwrap_or("");
    let size = data["size_bytes"].as_u64().unwrap_or(0);

    println!("{} Backup created: {filename}", "OK".green().bold());
    if !path.is_empty() {
        println!("  Path: {path}");
    }
    println!("  Size: {}", format_bytes(size));

    Ok(0)
}

async fn list(client: &OrionClient, format: &OutputFormat, quiet: bool) -> Result<i32> {
    let resp: Value = client.get("/api/v1/admin/backups").await?;
    let backups = resp["data"].as_array().cloned().unwrap_or_default();

    if quiet {
        for b in &backups {
            if let Some(f) = b["filename"].as_str() {
                println!("{f}");
            }
        }
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    if backups.is_empty() {
        println!("{}", "No backups found.".dimmed());
        return Ok(0);
    }

    let rows: Vec<BackupRow> = backups
        .iter()
        .map(|b| BackupRow {
            filename: b["filename"].as_str().unwrap_or("").to_string(),
            size: format_bytes(b["size_bytes"].as_u64().unwrap_or(0)),
            modified: b["modified_at"].as_str().unwrap_or("").to_string(),
        })
        .collect();

    output::print_table(rows);
    println!("{}", format!("{} backup(s)", backups.len()).dimmed());
    Ok(0)
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}
