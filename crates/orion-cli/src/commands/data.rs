use anyhow::Result;
use clap::Args;
use colored::Colorize;
use serde_json::Value;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};
use crate::utils;

#[derive(Args)]
#[command(
    long_about = "Send data to a channel for processing.\n\n\
        By default, sends synchronously and returns the processed result.\n\
        Use --async-mode to submit for background processing (returns a trace ID).\n\
        Combine --async-mode with --wait to poll until the trace completes.\n\n\
        The data payload is the business data that workflows process.",
    after_help = crate::help::SEND,
)]
pub struct SendCmd {
    /// Channel name to send data to
    channel: String,

    /// Path to JSON file with the data payload
    #[arg(short, long)]
    file: Option<String>,

    /// Inline JSON string with the data payload
    #[arg(short, long)]
    data: Option<String>,

    /// Read data payload from stdin
    #[arg(long)]
    stdin: bool,

    /// Submit for async processing (returns trace ID instead of result)
    #[arg(long = "async-mode", alias = "async")]
    async_mode: bool,

    /// Wait for async trace to complete (use with --async-mode)
    #[arg(long)]
    wait: bool,

    /// Timeout in seconds for --wait
    #[arg(long, default_value = "60")]
    timeout: u64,

    /// Optional metadata JSON string attached to the request
    #[arg(long)]
    metadata: Option<String>,

    /// Request server-side execution profiling (sync only; requires the server's
    /// tracing.debug_profile_enabled flag). Adds an _orion.profile breakdown.
    #[arg(long)]
    profile: bool,
}

impl SendCmd {
    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
        verbose: bool,
    ) -> Result<i32> {
        let payload =
            utils::read_json_input(self.file.as_deref(), self.data.as_deref(), self.stdin)?;

        if self.async_mode {
            if self.profile && !quiet {
                eprintln!(
                    "{} --profile is sync-only and is ignored with --async-mode",
                    "WARN".yellow()
                );
            }
            self.run_async(client, format, quiet, &self.channel, &payload)
                .await
        } else {
            self.run_sync(client, format, quiet, verbose, &self.channel, &payload)
                .await
        }
    }

    async fn run_sync(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
        verbose: bool,
        channel: &str,
        payload: &Value,
    ) -> Result<i32> {
        let mut body = serde_json::json!({ "data": payload });
        if let Some(meta) = &self.metadata {
            body["metadata"] = serde_json::from_str(meta)?;
        }

        let path = if self.profile {
            format!("/api/v1/data/{channel}?profile=1")
        } else {
            format!("/api/v1/data/{channel}")
        };
        let resp: Value = client.post(&path, &body).await?;

        let status = resp["status"].as_str().unwrap_or("unknown");

        if quiet {
            println!("{}", resp["id"].as_str().unwrap_or(""));
            return Ok(if status == "ok" { 0 } else { 1 });
        }

        if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
            output::print_value(format, &resp)?;
            return Ok(if status == "ok" { 0 } else { 1 });
        }

        let status_display = if status == "ok" {
            "OK".green().bold()
        } else {
            "ERROR".red().bold()
        };
        println!(
            "{status_display} Processed on channel '{channel}' ({})",
            resp["id"].as_str().unwrap_or("")
        );

        if verbose {
            if let Some(data) = resp.get("data") {
                println!("\n{}", "Output:".bold());
                println!("{}", serde_json::to_string_pretty(data)?);
            }
        }

        if let Some(errors) = resp.get("errors").and_then(|e| e.as_array()) {
            if !errors.is_empty() {
                for err in errors {
                    println!("  {} {err}", "WARN".yellow());
                }
            }
        }

        if self.profile {
            match resp.get("_orion").and_then(|o| o.get("profile")) {
                Some(profile) => render_profile(profile),
                None => println!(
                    "{}",
                    "  Profiling requested but not returned -- enable tracing.debug_profile_enabled on the server.".dimmed()
                ),
            }
        }

        Ok(if status == "ok" { 0 } else { 1 })
    }

    async fn run_async(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
        channel: &str,
        payload: &Value,
    ) -> Result<i32> {
        let mut body = serde_json::json!({ "data": payload });
        if let Some(meta) = &self.metadata {
            body["metadata"] = serde_json::from_str(meta)?;
        }

        let resp: Value = client
            .post(&format!("/api/v1/data/{channel}/async"), &body)
            .await?;

        let trace_id = resp["trace_id"].as_str().unwrap_or("");

        // When the channel's trace storage mode is "off", the server accepts the
        // request but returns a null trace_id (with a 299 warning header). There
        // is nothing to poll, so report and return success.
        if trace_id.is_empty() {
            if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
                output::print_value(format, &resp)?;
            } else if !quiet {
                println!(
                    "{} Submitted to '{channel}' (no trace -- tracing disabled for this channel)",
                    "OK".green().bold()
                );
            }
            return Ok(0);
        }

        if quiet {
            println!("{trace_id}");
        } else if !self.wait {
            println!(
                "{} Trace submitted: {}",
                "OK".green().bold(),
                trace_id.cyan()
            );
        }

        if self.wait {
            if !quiet {
                eprint!("Waiting for trace {trace_id}...");
            }
            let result = poll_trace(client, trace_id, self.timeout).await?;

            if !quiet {
                eprintln!();
            }

            let status = result["status"].as_str().unwrap_or("unknown");

            if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
                output::print_value(format, &result)?;
                return Ok(if status == "completed" { 0 } else { 1 });
            }

            match status {
                "completed" => {
                    if !quiet {
                        println!("{} Trace completed", "OK".green().bold());
                        if let Some(msg) = result.get("message") {
                            println!("{}", serde_json::to_string_pretty(msg)?);
                        } else if let Some(result_json) =
                            result.get("result_json").and_then(|r| r.as_str())
                        {
                            if let Ok(parsed) = serde_json::from_str::<Value>(result_json) {
                                println!("{}", serde_json::to_string_pretty(&parsed)?);
                            }
                        }
                    }
                    Ok(0)
                }
                "failed" => {
                    if !quiet {
                        let err = result["error_message"]
                            .as_str()
                            .or(result["error"].as_str())
                            .unwrap_or("Unknown error");
                        println!("{} Trace failed: {err}", "ERR".red().bold());
                    }
                    Ok(1)
                }
                _ => {
                    if !quiet {
                        println!("{} Timed out (status: {status})", "TIMEOUT".yellow().bold());
                    }
                    Ok(2)
                }
            }
        } else {
            Ok(0)
        }
    }
}

/// Render the `_orion.profile` block (v0.2 profiling output) as a compact
/// human-readable summary: total time, per-phase split, and the slowest handlers.
fn render_profile(profile: &Value) {
    println!("\n{}", "Profile:".bold());

    let total = profile
        .get("request_total_ms")
        .or_else(|| profile.get("totals_ms"))
        .and_then(|v| v.as_f64());
    if let Some(total) = total {
        println!("  Total: {total:.3} ms");
    }

    if let Some(phases) = profile.get("phases").and_then(|p| p.as_array()) {
        for phase in phases {
            let name = phase["name"].as_str().unwrap_or("");
            let ms = phase["ms"].as_f64().unwrap_or(0.0);
            let pct = phase["pct"].as_f64().unwrap_or(0.0);
            println!("    {name:<20} {ms:>8.3} ms  ({pct:.1}%)");
        }
    }

    if let Some(handlers) = profile.get("handlers").and_then(|h| h.as_array()) {
        if !handlers.is_empty() {
            println!("  {}", "Handlers:".bold());
            for h in handlers {
                let function = h["function"].as_str().unwrap_or("");
                let connector = h["connector"].as_str().unwrap_or("");
                let ms = h["duration_ms"].as_f64().unwrap_or(0.0);
                let target = if connector.is_empty() {
                    function.to_string()
                } else {
                    format!("{function} -> {connector}")
                };
                println!("    {target:<32} {ms:>8.3} ms");
            }
        }
    }
}

async fn poll_trace(client: &OrionClient, trace_id: &str, timeout_secs: u64) -> Result<Value> {
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(timeout_secs);

    loop {
        let resp: Value = client
            .get(&format!("/api/v1/data/traces/{trace_id}"))
            .await?;

        let status = resp["status"].as_str().unwrap_or("");
        if status == "completed" || status == "failed" {
            return Ok(resp);
        }

        if start.elapsed() >= timeout {
            return Ok(resp);
        }

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}
