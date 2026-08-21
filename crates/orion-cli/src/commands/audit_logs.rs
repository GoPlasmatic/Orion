use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use serde_json::Value;
use tabled::Tabled;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};
use crate::utils;
use orion_client::paths;

#[derive(Args)]
pub struct AuditLogsCmd {
    #[command(subcommand)]
    command: AuditLogsSubcommand,
}

#[derive(Subcommand)]
enum AuditLogsSubcommand {
    /// List audit log entries
    #[command(
        after_help = "Filters match exactly and combine with AND. A mistyped filter is\n\
            rejected with 400 rather than answered with unfiltered rows.\n\n\
            Examples:\n  \
            orion-cli audit-logs list --action status_active --resource-type workflow\n  \
            orion-cli audit-logs list --resource-id wf-orders --start-time 2026-07-01T00:00:00Z\n  \
            orion-cli audit-logs list --principal ci-deploy --limit 200"
    )]
    List {
        /// Filter by action (create, update, delete, status_active, ...)
        #[arg(long)]
        action: Option<String>,
        /// Filter by resource type (workflow, channel, connector, engine, ...)
        #[arg(long)]
        resource_type: Option<String>,
        /// Filter by resource ID
        #[arg(long)]
        resource_id: Option<String>,
        /// Filter by acting principal (the admin key id, or "anonymous")
        #[arg(long)]
        principal: Option<String>,
        /// Inclusive lower bound on created_at (RFC 3339, e.g. 2026-07-01T00:00:00Z)
        #[arg(long)]
        start_time: Option<String>,
        /// Exclusive upper bound on created_at (RFC 3339)
        #[arg(long)]
        end_time: Option<String>,
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
            AuditLogsSubcommand::List {
                action,
                resource_type,
                resource_id,
                principal,
                start_time,
                end_time,
                limit,
                offset,
            } => {
                let qs = utils::build_query_string(&[
                    ("action", action.clone()),
                    ("resource_type", resource_type.clone()),
                    ("resource_id", resource_id.clone()),
                    ("principal", principal.clone()),
                    ("start_time", start_time.clone()),
                    ("end_time", end_time.clone()),
                    ("limit", limit.map(|l| l.to_string())),
                    ("offset", offset.map(|o| o.to_string())),
                ]);
                list(client, format, quiet, &qs).await
            }
        }
    }
}

async fn list(client: &OrionClient, format: &OutputFormat, quiet: bool, qs: &str) -> Result<i32> {
    let resp: Value = client.get(&format!("{}{qs}", paths::AUDIT_LOGS)).await?;
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
    // The admin list envelope carries `total` at the top level alongside
    // `data`/`limit`/`offset` — there has never been a `pagination` object, so
    // reading one meant every page reported its own length as the total.
    utils::print_list_footer(&resp, entries.len(), "audit log entry(ies)");
    Ok(0)
}
