use anyhow::Result;
use clap::Args;
use colored::Colorize;
use serde_json::Value;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};
use crate::utils;
use orion_client::paths;

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

    /// Send the payload as the request body verbatim, with no {"data": ...}
    /// envelope. Required to reach a channel configured with
    /// request.body_mode = "payload".
    #[arg(long, conflicts_with = "metadata")]
    raw: bool,

    /// Request server-side execution profiling (sync only; requires the server's
    /// tracing.debug_profile_enabled flag). Adds an _orion.profile breakdown.
    #[arg(long)]
    profile: bool,
}

impl SendCmd {
    /// The request body carrying `payload`.
    ///
    /// The default wraps it in the Orion envelope, which is what an `auto`
    /// channel detects. `--raw` sends the payload verbatim, which is what a
    /// channel configured with `request.body_mode = "payload"` needs: that
    /// channel takes the whole body as `data`, so the envelope would arrive
    /// as a single key literally named `data` (#282).
    ///
    /// `--metadata` is refused alongside `--raw` at the clap level rather
    /// than here — a payload-mode channel stamps metadata server-side and
    /// accepts none from the caller, so there is nowhere for it to go, and
    /// silently dropping it would look like it had been sent.
    fn build_body(&self, payload: &Value) -> Result<Value> {
        if self.raw {
            return Ok(payload.clone());
        }
        let mut body = serde_json::json!({ "data": payload });
        if let Some(meta) = &self.metadata {
            body["metadata"] = serde_json::from_str(meta)?;
        }
        Ok(body)
    }

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
        let body = self.build_body(payload)?;

        let path = if self.profile {
            format!("{}?profile=1", paths::data(channel))
        } else {
            paths::data(channel)
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

        if verbose && let Some(data) = resp.get("data") {
            println!("\n{}", "Output:".bold());
            println!("{}", serde_json::to_string_pretty(data)?);
        }

        if let Some(errors) = resp.get("errors").and_then(|e| e.as_array())
            && !errors.is_empty()
        {
            for err in errors {
                println!("  {} {err}", "WARN".yellow());
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
        let body = self.build_body(payload)?;

        let resp: Value = client.post(&paths::data_async(channel), &body).await?;

        let trace_id = resp["trace_id"].as_str().unwrap_or("");
        // v1.0 always returns a trace_token alongside the id; polling with
        // ?token= works even when the admin API requires an API key.
        let trace_token = resp["trace_token"].as_str().unwrap_or("");

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
            if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
                // The full AsyncSubmitResponse — callers need trace_token to
                // read the trace back (v1.0).
                output::print_value(format, &resp)?;
            } else {
                println!(
                    "{} Trace submitted: {}",
                    "OK".green().bold(),
                    trace_id.cyan()
                );
            }
        }

        if self.wait {
            if !quiet {
                eprint!("Waiting for trace {trace_id}...");
            }
            let result = poll_trace(client, trace_id, trace_token, self.timeout).await?;

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
                            && let Ok(parsed) = serde_json::from_str::<Value>(result_json)
                        {
                            println!("{}", serde_json::to_string_pretty(&parsed)?);
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

    if let Some(handlers) = profile.get("handlers").and_then(|h| h.as_array())
        && !handlers.is_empty()
    {
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

async fn poll_trace(
    client: &OrionClient,
    trace_id: &str,
    trace_token: &str,
    timeout_secs: u64,
) -> Result<Value> {
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(timeout_secs);
    let path = if trace_token.is_empty() {
        paths::trace(trace_id)
    } else {
        format!("{}?token={trace_token}", paths::trace(trace_id))
    };

    loop {
        let resp: Value = client.get(&path).await?;
        // v1.0 wraps the TraceDetail in the {"data": …} admin envelope.
        let resp = resp.get("data").cloned().unwrap_or(resp);

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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// `SendCmd` is built by clap, so parse it the way the binary does rather
    /// than hand-constructing it — that way the `conflicts_with` wiring is
    /// under test too, not just `build_body`.
    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        send: SendCmd,
    }

    fn parse(args: &[&str]) -> SendCmd {
        TestCli::parse_from(args).send
    }

    #[test]
    fn the_default_wraps_the_payload_in_the_envelope() {
        let cmd = parse(&["send", "ch", "-d", "{}"]);
        let payload = serde_json::json!({"platform": "ios"});
        let body = cmd.build_body(&payload).expect("test");
        assert_eq!(body, serde_json::json!({"data": {"platform": "ios"}}));
    }

    #[test]
    fn metadata_rides_alongside_the_envelope() {
        let cmd = parse(&["send", "ch", "-d", "{}", "--metadata", r#"{"src":"cli"}"#]);
        let body = cmd.build_body(&serde_json::json!({"a": 1})).expect("test");
        assert_eq!(body["data"], serde_json::json!({"a": 1}));
        assert_eq!(body["metadata"], serde_json::json!({"src": "cli"}));
    }

    /// The point of the flag: a payload-mode channel reads the whole body as
    /// `data`, so the envelope would arrive as one key named `data`.
    #[test]
    fn raw_sends_the_payload_verbatim() {
        let cmd = parse(&["send", "ch", "-d", "{}", "--raw"]);
        let payload = serde_json::json!({"platform": "ios", "data": {"title": "Nested"}});
        let body = cmd.build_body(&payload).expect("test");
        assert_eq!(body, payload, "the body must be the payload itself");
        assert!(
            body.get("data").is_some_and(|d| d.get("title").is_some()),
            "the model's own 'data' key must survive untouched: {body}"
        );
    }

    /// Refused rather than silently dropped: a payload-mode channel stamps
    /// metadata server-side and accepts none from the caller, so honouring
    /// both is impossible and quietly ignoring one would look like it worked.
    #[test]
    fn raw_and_metadata_are_refused_together() {
        // Matched rather than `expect_err`: neither `SendCmd` nor the wrapper
        // derives `Debug`, which `expect_err` would require.
        let err = match TestCli::try_parse_from([
            "send",
            "ch",
            "-d",
            "{}",
            "--raw",
            "--metadata",
            "{}",
        ]) {
            Ok(_) => panic!("clap must refuse --raw with --metadata"),
            Err(e) => e,
        };
        let rendered = err.to_string();
        assert!(
            rendered.contains("--raw") && rendered.contains("--metadata"),
            "the error should name both flags: {rendered}"
        );
    }
}
