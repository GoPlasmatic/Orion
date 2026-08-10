use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use serde_json::Value;
use tabled::Tabled;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};
use crate::utils;

#[derive(Args)]
pub struct AuditLogsCmd {
    #[command(subcommand)]
    command: AuditLogsSubcommand,
}

#[derive(Subcommand)]
enum AuditLogsSubcommand {
    /// List audit log entries
    List {
        /// Maximum entries to return (default: 50, max: 1000)
        #[arg(long)]
        limit: Option<i64>,
        /// Number of entries to skip
        #[arg(long)]
        offset: Option<i64>,
    },
}

#[derive(Tabled)]
struct AuditLogRow {
    #[tabled(rename = "ID")]
    id: String,
    #[tabled(rename = "Principal")]
    principal: String,
    #[tabled(rename = "Action")]
    action: String,
    #[tabled(rename = "Resource Type")]
    resource_type: String,
    #[tabled(rename = "Resource ID")]
    resource_id: String,
    #[tabled(rename = "Created")]
    created: String,
}

impl AuditLogsCmd {
    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
    ) -> Result<i32> {
        match &self.command {
            AuditLogsSubcommand::List { limit, offset } => {
                list(client, format, quiet, limit, offset).await
            }
        }
    }
}

async fn list(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    limit: &Option<i64>,
    offset: &Option<i64>,
) -> Result<i32> {
    let qs = utils::build_query_string(&[
        ("limit", limit.map(|l| l.to_string())),
        ("offset", offset.map(|o| o.to_string())),
    ]);

    let resp: Value = client.get(&format!("/api/v1/admin/audit-logs{qs}")).await?;
    let entries = resp["data"].as_array().cloned().unwrap_or_default();

    if quiet {
        for e in &entries {
            if let Some(id) = e["id"].as_str() {
                println!("{id}");
            }
        }
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    if entries.is_empty() {
        println!("{}", "No audit log entries found.".dimmed());
        return Ok(0);
    }

    let rows: Vec<AuditLogRow> = entries
        .iter()
        .map(|e| AuditLogRow {
            id: utils::truncate(e["id"].as_str().unwrap_or(""), 12),
            principal: e["principal"].as_str().unwrap_or("").to_string(),
            action: e["action"].as_str().unwrap_or("").to_string(),
            resource_type: e["resource_type"].as_str().unwrap_or("").to_string(),
            resource_id: utils::truncate(e["resource_id"].as_str().unwrap_or(""), 20),
            created: e["created_at"].as_str().unwrap_or("").to_string(),
        })
        .collect();

    output::print_table(rows);
    let total = resp["pagination"]["total"]
        .as_i64()
        .unwrap_or(entries.len() as i64);
    println!("{}", format!("{total} audit log entry(ies)").dimmed());
    Ok(0)
}
