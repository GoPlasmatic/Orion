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
#[command(
    long_about = "Inspect and drain the trace dead-letter queue (v1.0).\n\n\
        Traces whose async persistence failed land here and are retried\n\
        automatically. 'requeue' resets an entry for immediate retry;\n\
        'purge' permanently deletes exhausted entries older than a cut-off."
)]
pub struct DlqCmd {
    #[command(subcommand)]
    command: DlqSubcommand,
}

#[derive(Subcommand)]
enum DlqSubcommand {
    /// List dead-letter entries
    List {
        /// Filter by channel name
        #[arg(long)]
        channel: Option<String>,
        /// Only entries whose retries are exhausted (true/false)
        #[arg(long)]
        exhausted: Option<bool>,
        /// Page size
        #[arg(long)]
        limit: Option<i64>,
        /// Page offset
        #[arg(long)]
        offset: Option<i64>,
    },
    /// Show one dead-letter entry, including its payload
    Get {
        /// DLQ entry ID
        id: String,
    },
    /// Reset an entry's retry counter so the next retry pass picks it up
    Requeue {
        /// DLQ entry ID
        id: String,
    },
    /// Permanently delete exhausted entries older than a cut-off
    Purge {
        /// Age cut-off in hours (e.g. 168 = one week). Required: purging is destructive.
        #[arg(long)]
        older_than_hours: i64,
    },
}

#[derive(Tabled)]
struct DlqRow {
    #[tabled(rename = "ID")]
    id: String,
    #[tabled(rename = "Trace")]
    trace_id: String,
    #[tabled(rename = "Channel")]
    channel: String,
    #[tabled(rename = "Retries")]
    retries: String,
    #[tabled(rename = "Next Retry")]
    next_retry: String,
    #[tabled(rename = "Error")]
    error: String,
}

impl DlqCmd {
    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
        yes: bool,
    ) -> Result<i32> {
        match &self.command {
            DlqSubcommand::List {
                channel,
                exhausted,
                limit,
                offset,
            } => list(client, format, quiet, channel, exhausted, limit, offset).await,
            DlqSubcommand::Get { id } => get(client, format, quiet, id).await,
            DlqSubcommand::Requeue { id } => requeue(client, quiet, id).await,
            DlqSubcommand::Purge { older_than_hours } => {
                purge(client, quiet, yes, *older_than_hours).await
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn list(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    channel: &Option<String>,
    exhausted: &Option<bool>,
    limit: &Option<i64>,
    offset: &Option<i64>,
) -> Result<i32> {
    let qs = utils::build_query_string(&[
        ("channel", channel.clone()),
        ("exhausted", exhausted.map(|e| e.to_string())),
        ("limit", limit.map(|l| l.to_string())),
        ("offset", offset.map(|o| o.to_string())),
    ]);
    let resp: Value = client.get(&format!("{}{qs}", paths::TRACE_DLQ)).await?;
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
        println!("{}", "Dead-letter queue is empty.".dimmed());
        return Ok(0);
    }

    let rows: Vec<DlqRow> = entries
        .iter()
        .map(|e| DlqRow {
            id: e["id"].as_str().unwrap_or("").to_string(),
            trace_id: e["trace_id"].as_str().unwrap_or("").to_string(),
            channel: e["channel"].as_str().unwrap_or("").to_string(),
            retries: format!(
                "{}/{}",
                e["retry_count"].as_i64().unwrap_or(0),
                e["max_retries"].as_i64().unwrap_or(0)
            ),
            next_retry: e["next_retry_at"].as_str().unwrap_or("").to_string(),
            error: utils::truncate(e["error_message"].as_str().unwrap_or(""), 40),
        })
        .collect();

    output::print_table(rows);
    utils::print_list_footer(&resp, entries.len(), "entrie(s)");
    Ok(0)
}

async fn get(client: &OrionClient, format: &OutputFormat, quiet: bool, id: &str) -> Result<i32> {
    let resp: Value = client.get(&paths::trace_dlq_entry(id)).await?;
    let entry = &resp["data"];

    if quiet {
        println!("{}", entry["trace_id"].as_str().unwrap_or(""));
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    println!("{}", "DLQ Entry".bold());
    println!("  ID:         {}", entry["id"].as_str().unwrap_or(id));
    println!("  Trace:      {}", entry["trace_id"].as_str().unwrap_or(""));
    println!("  Channel:    {}", entry["channel"].as_str().unwrap_or(""));
    println!(
        "  Retries:    {}/{}",
        entry["retry_count"].as_i64().unwrap_or(0),
        entry["max_retries"].as_i64().unwrap_or(0)
    );
    println!(
        "  Next retry: {}",
        entry["next_retry_at"].as_str().unwrap_or("")
    );
    println!(
        "  Error:      {}",
        entry["error_message"].as_str().unwrap_or("").red()
    );

    if let Some(payload) = entry.get("payload_json").filter(|p| !p.is_null()) {
        let parsed = match payload.as_str() {
            Some(s) => serde_json::from_str::<Value>(s).ok(),
            None => Some(payload.clone()),
        };
        if let Some(parsed) = parsed {
            println!("\n{}", "Payload:".bold());
            println!("{}", serde_json::to_string_pretty(&parsed)?);
        }
    }

    Ok(0)
}

async fn requeue(client: &OrionClient, quiet: bool, id: &str) -> Result<i32> {
    let resp: Value = client.post_empty(&paths::trace_dlq_requeue(id)).await?;

    if !quiet {
        let entry = &resp["data"];
        println!(
            "{} Entry {} requeued (next retry: {})",
            "OK".green().bold(),
            entry["id"].as_str().unwrap_or(id),
            entry["next_retry_at"].as_str().unwrap_or("now")
        );
    }
    Ok(0)
}

async fn purge(client: &OrionClient, quiet: bool, yes: bool, older_than_hours: i64) -> Result<i32> {
    if !utils::confirm(
        &format!("Permanently delete exhausted DLQ entries older than {older_than_hours}h?"),
        yes,
    )? {
        println!("Cancelled.");
        return Ok(0);
    }

    let body = serde_json::json!({ "older_than_hours": older_than_hours });
    let resp: Value = client.post(paths::TRACE_DLQ_PURGE, &body).await?;
    let purged = resp["data"]["purged"].as_i64().unwrap_or(0);

    if quiet {
        println!("{purged}");
    } else {
        println!("{} Purged {purged} entrie(s)", "OK".green().bold());
    }
    Ok(0)
}
