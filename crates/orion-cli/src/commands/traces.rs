use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use serde_json::Value;
use tabled::Tabled;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};
use crate::utils;

#[derive(Args)]
#[command(
    long_about = "View and monitor execution traces for processed data.\n\n\
        Every data send (sync or async) creates a trace with status, timing, and result details.\n\
        Use 'traces list' to find traces and 'traces wait' to poll until completion.\n\n\
        With --quiet, list prints one ID per line, get prints the status."
)]
pub struct TracesCmd {
    #[command(subcommand)]
    command: TracesSubcommand,
}

#[derive(Subcommand)]
enum TracesSubcommand {
    /// List traces with optional filters
    #[command(
        after_help = "Examples:\n  orion-cli traces list --status failed --channel orders\n  orion-cli traces list --mode async --sort-by created_at --sort-order desc --limit 10"
    )]
    List {
        /// Filter by status (pending, running, completed, failed)
        #[arg(long)]
        status: Option<String>,
        /// Filter by channel name
        #[arg(long)]
        channel: Option<String>,
        /// Filter by mode (sync, async)
        #[arg(long)]
        mode: Option<String>,
        /// Sort by column (created_at, updated_at, status, channel, mode)
        #[arg(long)]
        sort_by: Option<String>,
        /// Sort direction (asc, desc)
        #[arg(long)]
        sort_order: Option<String>,
        /// Page size
        #[arg(long)]
        limit: Option<i64>,
        /// Page offset
        #[arg(long)]
        offset: Option<i64>,
    },
    /// Get trace details including result or error
    Get {
        /// Trace ID
        id: String,
    },
    /// Wait for a trace to complete (polls until done or timeout)
    #[command(
        after_help = "Exit codes: 0 = completed, 1 = failed, 2 = timeout\n\nExamples:\n  orion-cli traces wait <trace-id>\n  orion-cli traces wait <trace-id> --timeout 120 --interval 5"
    )]
    Wait {
        /// Trace ID
        id: String,
        /// Poll interval in seconds
        #[arg(long, default_value = "1")]
        interval: u64,
        /// Timeout in seconds
        #[arg(long, default_value = "60")]
        timeout: u64,
    },
}

#[derive(Tabled)]
struct TraceRow {
    #[tabled(rename = "ID")]
    id: String,
    #[tabled(rename = "Channel")]
    channel: String,
    #[tabled(rename = "Mode")]
    mode: String,
    #[tabled(rename = "Status")]
    status: String,
    #[tabled(rename = "Duration")]
    duration: String,
    #[tabled(rename = "Created")]
    created: String,
}

impl TracesCmd {
    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
    ) -> Result<i32> {
        match &self.command {
            TracesSubcommand::List {
                status,
                channel,
                mode,
                sort_by,
                sort_order,
                limit,
                offset,
            } => {
                list(
                    client, format, quiet, status, channel, mode, sort_by, sort_order, limit,
                    offset,
                )
                .await
            }
            TracesSubcommand::Get { id } => get(client, format, quiet, id).await,
            TracesSubcommand::Wait {
                id,
                interval,
                timeout,
            } => wait(client, format, quiet, id, *interval, *timeout).await,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn list(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    status: &Option<String>,
    channel: &Option<String>,
    mode: &Option<String>,
    sort_by: &Option<String>,
    sort_order: &Option<String>,
    limit: &Option<i64>,
    offset: &Option<i64>,
) -> Result<i32> {
    let qs = utils::build_query_string(&[
        ("status", status.clone()),
        ("channel", channel.clone()),
        ("mode", mode.clone()),
        ("sort_by", sort_by.clone()),
        ("sort_order", sort_order.clone()),
        ("limit", limit.map(|l| l.to_string())),
        ("offset", offset.map(|o| o.to_string())),
    ]);

    let resp: Value = client.get(&format!("/api/v1/data/traces{qs}")).await?;
    let traces = resp["data"].as_array().cloned().unwrap_or_default();

    if quiet {
        for t in &traces {
            if let Some(id) = t["id"].as_str() {
                println!("{id}");
            }
        }
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    if traces.is_empty() {
        println!("{}", "No traces found.".dimmed());
        return Ok(0);
    }

    let rows: Vec<TraceRow> = traces
        .iter()
        .map(|t| {
            let duration = t["duration_ms"]
                .as_f64()
                .map(|d| format!("{:.1}ms", d))
                .unwrap_or_else(|| "-".to_string());
            TraceRow {
                id: utils::truncate(t["id"].as_str().unwrap_or(""), 12),
                channel: t["channel"].as_str().unwrap_or("").to_string(),
                mode: t["mode"].as_str().unwrap_or("").to_string(),
                status: utils::colorize_status(t["status"].as_str().unwrap_or("")),
                duration,
                created: t["created_at"].as_str().unwrap_or("").to_string(),
            }
        })
        .collect();

    output::print_table(rows);
    let total = resp["total"].as_i64().unwrap_or(traces.len() as i64);
    println!("{}", format!("{} trace(s)", total).dimmed());
    Ok(0)
}

async fn get(client: &OrionClient, format: &OutputFormat, quiet: bool, id: &str) -> Result<i32> {
    let resp: Value = client.get(&format!("/api/v1/data/traces/{id}")).await?;

    let status = resp["status"].as_str().unwrap_or("unknown");

    if quiet {
        println!("{status}");
        return Ok(status_exit_code(status));
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(status_exit_code(status));
    }

    println!("{}", "Trace Details".bold());
    println!("  ID:        {}", resp["id"].as_str().unwrap_or(""));
    if let Some(channel) = resp["channel"].as_str() {
        println!("  Channel:   {channel}");
    }
    println!("  Mode:      {}", resp["mode"].as_str().unwrap_or(""));
    println!("  Status:    {}", utils::colorize_status(status));
    println!("  Created:   {}", resp["created_at"]);

    if let Some(started) = resp["started_at"].as_str() {
        println!("  Started:   {started}");
    }
    if let Some(completed) = resp["completed_at"].as_str() {
        println!("  Completed: {completed}");
    }
    if let Some(duration) = resp["duration_ms"].as_f64() {
        println!("  Duration:  {:.1}ms", duration);
    }

    if status == "completed" {
        if let Some(msg) = resp.get("message") {
            println!("\n{}", "Result:".bold());
            println!("{}", serde_json::to_string_pretty(msg)?);
        } else if let Some(result) = resp.get("result_json").and_then(|r| r.as_str()) {
            if let Ok(parsed) = serde_json::from_str::<Value>(result) {
                println!("\n{}", "Result:".bold());
                println!("{}", serde_json::to_string_pretty(&parsed)?);
            }
        }
    }

    if status == "failed" {
        if let Some(err) = resp["error_message"].as_str() {
            println!("\n{} {err}", "Error:".red().bold());
        } else if let Some(err) = resp["error"].as_str() {
            println!("\n{} {err}", "Error:".red().bold());
        }
    }

    // Per-task execution trace, captured when the channel sets
    // config.tracing.task_details = true (v0.2). The server returns it as a
    // JSON object; tolerate a JSON-encoded string too.
    if let Some(task_trace) = resp.get("task_trace_json").filter(|t| !t.is_null()) {
        let parsed = match task_trace.as_str() {
            Some(s) => serde_json::from_str::<Value>(s).ok(),
            None => Some(task_trace.clone()),
        };
        if let Some(parsed) = parsed {
            println!("\n{}", "Task trace:".bold());
            println!("{}", serde_json::to_string_pretty(&parsed)?);
        }
    }

    Ok(status_exit_code(status))
}

async fn wait(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    id: &str,
    interval: u64,
    timeout: u64,
) -> Result<i32> {
    let start = std::time::Instant::now();
    let timeout_dur = std::time::Duration::from_secs(timeout);
    let interval_dur = std::time::Duration::from_secs(interval);

    if !quiet {
        eprint!("Waiting for trace {id}...");
    }

    loop {
        let resp: Value = client.get(&format!("/api/v1/data/traces/{id}")).await?;

        let status = resp["status"].as_str().unwrap_or("unknown");

        if status == "completed" || status == "failed" {
            if !quiet {
                eprintln!();
            }

            if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
                output::print_value(format, &resp)?;
            } else if !quiet {
                match status {
                    "completed" => {
                        println!("{} Trace completed", "OK".green().bold());
                        if let Some(msg) = resp.get("message") {
                            println!("{}", serde_json::to_string_pretty(msg)?);
                        } else if let Some(result) =
                            resp.get("result_json").and_then(|r| r.as_str())
                        {
                            if let Ok(parsed) = serde_json::from_str::<Value>(result) {
                                println!("{}", serde_json::to_string_pretty(&parsed)?);
                            }
                        }
                    }
                    "failed" => {
                        let err = resp["error_message"]
                            .as_str()
                            .or(resp["error"].as_str())
                            .unwrap_or("Unknown error");
                        println!("{} Trace failed: {err}", "ERR".red().bold());
                    }
                    _ => {}
                }
            }

            return Ok(status_exit_code(status));
        }

        if start.elapsed() >= timeout_dur {
            if !quiet {
                eprintln!();
                println!(
                    "{} Timed out after {timeout}s (status: {status})",
                    "TIMEOUT".yellow().bold()
                );
            }
            return Ok(2);
        }

        tokio::time::sleep(interval_dur).await;
    }
}

fn status_exit_code(status: &str) -> i32 {
    match status {
        "completed" => 0,
        "failed" => 1,
        _ => 0,
    }
}
