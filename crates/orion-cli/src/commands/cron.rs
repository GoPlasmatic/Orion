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
#[command(long_about = "Inspect scheduled runs.\n\n\
        Every scheduled instant of a cron channel becomes a durable\n\
        occurrence, recorded before it runs and kept after it finishes — so\n\
        'did last night's job run?' is a question with an answer, whatever\n\
        the trace-storage settings are.\n\n\
        'status' is the overview: what is scheduled and when it next fires.\n\
        'list' is the history. 'retry' re-attempts one that failed.")]
pub struct CronCmd {
    #[command(subcommand)]
    command: CronSubcommand,
}

#[derive(Subcommand)]
enum CronSubcommand {
    /// What is scheduled, when it next fires, and how it last went
    Status,
    /// List occurrences, newest first
    List {
        /// Only this channel (its id, not its name)
        #[arg(long)]
        channel_id: Option<String>,
        /// Only this status: pending, running, completed, failed,
        /// skipped_misfire, skipped_singleton
        #[arg(long)]
        status: Option<String>,
        /// Page size
        #[arg(long)]
        limit: Option<i64>,
        /// Page offset
        #[arg(long)]
        offset: Option<i64>,
    },
    /// Show one occurrence in full, including why it failed
    Get {
        /// Occurrence id
        id: String,
    },
    /// Attempt a failed or skipped occurrence again
    ///
    /// Keeps the occurrence's identity and its scheduled time — it is another
    /// attempt at the work that was due then, not a new piece of work. To run
    /// a schedule again now, use `orion-cli channels trigger` instead.
    Retry {
        /// Occurrence id
        id: String,
    },
}

#[derive(Tabled)]
struct ScheduleRow {
    #[tabled(rename = "Channel")]
    channel: String,
    #[tabled(rename = "Schedule")]
    schedule: String,
    #[tabled(rename = "Zone")]
    timezone: String,
    #[tabled(rename = "Next Fire")]
    next_fire: String,
    #[tabled(rename = "Last")]
    last: String,
    #[tabled(rename = "Pending")]
    pending: String,
}

#[derive(Tabled)]
struct OccurrenceRow {
    #[tabled(rename = "ID")]
    id: String,
    #[tabled(rename = "Channel")]
    channel: String,
    #[tabled(rename = "Scheduled For")]
    scheduled_for: String,
    #[tabled(rename = "Status")]
    status: String,
    #[tabled(rename = "Trigger")]
    trigger: String,
    #[tabled(rename = "Attempt")]
    attempt: String,
}

fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or("-").to_string()
}

impl CronCmd {
    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
    ) -> Result<i32> {
        match &self.command {
            CronSubcommand::Status => status(client, format, quiet).await,
            CronSubcommand::List {
                channel_id,
                status,
                limit,
                offset,
            } => {
                let qs = utils::build_query_string(&[
                    ("channel_id", channel_id.clone()),
                    ("status", status.clone()),
                    ("limit", limit.map(|l| l.to_string())),
                    ("offset", offset.map(|o| o.to_string())),
                ]);
                list(client, format, quiet, &qs).await
            }
            CronSubcommand::Get { id } => get(client, format, quiet, id).await,
            CronSubcommand::Retry { id } => retry(client, format, quiet, id).await,
        }
    }
}

async fn status(client: &OrionClient, format: &OutputFormat, quiet: bool) -> Result<i32> {
    let resp: Value = client.get(paths::CRON_STATUS).await?;
    let schedules = resp["data"].as_array().cloned().unwrap_or_default();

    if quiet {
        for s in &schedules {
            if let Some(id) = s["channel_id"].as_str() {
                println!("{id}");
            }
        }
        return Ok(0);
    }
    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }
    if schedules.is_empty() {
        println!("{}", "No cron channels are active.".dimmed());
        return Ok(0);
    }

    let rows: Vec<ScheduleRow> = schedules
        .iter()
        .map(|r| ScheduleRow {
            channel: text(r, "channel_name"),
            schedule: text(r, "schedule"),
            timezone: text(r, "timezone"),
            next_fire: text(r, "next_fire_at"),
            last: text(r, "last_status"),
            pending: r["pending"].as_i64().unwrap_or(0).to_string(),
        })
        .collect();
    output::print_table(rows);
    Ok(0)
}

async fn list(client: &OrionClient, format: &OutputFormat, quiet: bool, qs: &str) -> Result<i32> {
    let resp: Value = client
        .get(&format!("{}{qs}", paths::CRON_OCCURRENCES))
        .await?;
    let occurrences = resp["data"].as_array().cloned().unwrap_or_default();

    if quiet {
        for o in &occurrences {
            if let Some(id) = o["id"].as_str() {
                println!("{id}");
            }
        }
        return Ok(0);
    }
    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }
    if occurrences.is_empty() {
        println!("{}", "No occurrences yet.".dimmed());
        return Ok(0);
    }

    let rows: Vec<OccurrenceRow> = occurrences
        .iter()
        .map(|r| OccurrenceRow {
            id: text(r, "id"),
            channel: text(r, "channel_name"),
            scheduled_for: text(r, "scheduled_for"),
            status: text(r, "status"),
            trigger: text(r, "trigger"),
            attempt: r["attempt"].as_i64().unwrap_or(0).to_string(),
        })
        .collect();
    output::print_table(rows);
    utils::print_list_footer(&resp, occurrences.len(), "occurrence(s)");
    Ok(0)
}

async fn get(client: &OrionClient, format: &OutputFormat, quiet: bool, id: &str) -> Result<i32> {
    let resp: Value = client.get(&paths::cron_occurrence(id)).await?;
    let occurrence = &resp["data"];

    if quiet {
        println!("{}", occurrence["status"].as_str().unwrap_or(""));
        return Ok(0);
    }
    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    println!("{}", "Occurrence".bold());
    println!("  ID:            {}", text(occurrence, "id"));
    println!("  Channel:       {}", text(occurrence, "channel_name"));
    println!("  Trigger:       {}", text(occurrence, "trigger"));
    // The two instants a scheduled run has, and the distinction between them:
    // what the work was for, and when it actually ran.
    println!("  Scheduled for: {}", text(occurrence, "scheduled_for"));
    println!("  Started:       {}", text(occurrence, "started_at"));
    println!("  Completed:     {}", text(occurrence, "completed_at"));
    println!("  Status:        {}", text(occurrence, "status"));
    println!(
        "  Attempt:       {}",
        occurrence["attempt"].as_i64().unwrap_or(0)
    );
    if let Some(key) = occurrence["singleton_key"].as_str() {
        println!("  Singleton:     {key}");
    }
    if let Some(trace) = occurrence["trace_id"].as_str() {
        println!("  Trace:         {trace}");
    }
    if let Some(error) = occurrence["error_message"].as_str() {
        println!("  Error:         {}", error.red());
    }
    Ok(0)
}

async fn retry(client: &OrionClient, format: &OutputFormat, quiet: bool, id: &str) -> Result<i32> {
    let resp: Value = client
        .post(&paths::cron_occurrence_retry(id), &Value::Null)
        .await?;
    if quiet {
        println!("{id}");
        return Ok(0);
    }
    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }
    println!(
        "{} Occurrence {} queued for another attempt at {}",
        "✓".green(),
        id,
        text(&resp["data"], "scheduled_for")
    );
    Ok(0)
}
