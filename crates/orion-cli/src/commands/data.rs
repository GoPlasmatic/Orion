use anyhow::{Result, bail};
use clap::Args;
use colored::Colorize;
use serde_json::Value;
use std::io::Read;
use tabled::Tabled;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};

#[derive(Args)]
pub struct SendCmd {
    /// Channel name (required for sync/async, not for batch)
    channel: Option<String>,

    /// JSON file with payload
    #[arg(short, long)]
    file: Option<String>,

    /// Inline JSON data
    #[arg(short, long)]
    data: Option<String>,

    /// Read payload from stdin
    #[arg(long)]
    stdin: bool,

    /// Submit for async processing
    #[arg(long, name = "async")]
    async_mode: bool,

    /// Wait for async job to complete
    #[arg(long)]
    wait: bool,

    /// Timeout for --wait (e.g., 30)
    #[arg(long, default_value = "60")]
    timeout: u64,

    /// Batch mode: process multiple messages
    #[arg(long)]
    batch: bool,

    /// Optional metadata JSON
    #[arg(long)]
    metadata: Option<String>,
}

#[derive(Tabled)]
struct BatchResultRow {
    #[tabled(rename = "#")]
    index: usize,
    #[tabled(rename = "ID")]
    id: String,
    #[tabled(rename = "Status")]
    status: String,
}

impl SendCmd {
    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
        verbose: bool,
    ) -> Result<i32> {
        if self.batch {
            return self.run_batch(client, format, quiet).await;
        }

        let channel = self
            .channel
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Channel name is required"))?;

        let payload = self.read_payload()?;

        if self.async_mode {
            self.run_async(client, format, quiet, channel, &payload)
                .await
        } else {
            self.run_sync(client, format, quiet, verbose, channel, &payload)
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

        let resp: Value = client
            .post(&format!("/api/v1/data/{channel}"), &body)
            .await?;

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

        let job_id = resp["job_id"].as_str().unwrap_or("");

        if quiet {
            println!("{job_id}");
        } else if !self.wait {
            println!("{} Job submitted: {}", "OK".green().bold(), job_id.cyan());
        }

        if self.wait {
            if !quiet {
                eprint!("Waiting for job {job_id}...");
            }
            let result = poll_job(client, job_id, self.timeout).await?;

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
                        println!("{} Job completed", "OK".green().bold());
                        if let Some(result_data) = result.get("result") {
                            println!("{}", serde_json::to_string_pretty(result_data)?);
                        }
                    }
                    Ok(0)
                }
                "failed" => {
                    if !quiet {
                        let err = result["error"].as_str().unwrap_or("Unknown error");
                        println!("{} Job failed: {err}", "ERR".red().bold());
                    }
                    Ok(1)
                }
                _ => {
                    if !quiet {
                        println!(
                            "{} Job timed out (status: {status})",
                            "TIMEOUT".yellow().bold()
                        );
                    }
                    Ok(2)
                }
            }
        } else {
            Ok(0)
        }
    }

    async fn run_batch(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
    ) -> Result<i32> {
        let payload = self.read_payload()?;

        // Payload should be the full batch request or an array of messages
        let body = if payload.get("messages").is_some() {
            payload
        } else if payload.is_array() {
            serde_json::json!({ "messages": payload })
        } else {
            bail!("Batch payload must be an array of messages or {{\"messages\": [...]}}");
        };

        let resp: Value = client.post("/api/v1/data/batch", &body).await?;

        if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
            output::print_value(format, &resp)?;
            return Ok(0);
        }

        let results = resp["results"].as_array().cloned().unwrap_or_default();

        if quiet {
            let ok = results
                .iter()
                .filter(|r| r["status"].as_str() == Some("ok"))
                .count();
            let fail = results.len() - ok;
            println!("{ok} ok, {fail} failed");
            return Ok(if fail > 0 { 1 } else { 0 });
        }

        let rows: Vec<BatchResultRow> = results
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let status = r["status"].as_str().unwrap_or("unknown");
                BatchResultRow {
                    index: i + 1,
                    id: r["id"].as_str().unwrap_or("").chars().take(12).collect(),
                    status: if status == "ok" {
                        "ok".green().to_string()
                    } else {
                        "error".red().to_string()
                    },
                }
            })
            .collect();

        output::print_table(rows);

        let ok = results
            .iter()
            .filter(|r| r["status"].as_str() == Some("ok"))
            .count();
        let fail = results.len() - ok;
        println!("{}", format!("{ok} succeeded, {fail} failed").dimmed());

        Ok(if fail > 0 { 1 } else { 0 })
    }

    fn read_payload(&self) -> Result<Value> {
        if let Some(path) = &self.file {
            let content = std::fs::read_to_string(path)?;
            Ok(serde_json::from_str(&content)?)
        } else if let Some(json) = &self.data {
            Ok(serde_json::from_str(json)?)
        } else if self.stdin {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            Ok(serde_json::from_str(&buf)?)
        } else {
            bail!("Provide input with -f <file>, -d '<json>', or --stdin")
        }
    }
}

async fn poll_job(client: &OrionClient, job_id: &str, timeout_secs: u64) -> Result<Value> {
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(timeout_secs);

    loop {
        let resp: Value = client.get(&format!("/api/v1/data/jobs/{job_id}")).await?;

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
