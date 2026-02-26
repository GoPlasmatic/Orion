use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use serde_json::Value;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};

#[derive(Args)]
pub struct JobsCmd {
    #[command(subcommand)]
    command: JobsSubcommand,
}

#[derive(Subcommand)]
enum JobsSubcommand {
    /// Get job status
    Get {
        /// Job ID
        id: String,
    },
    /// Wait for a job to complete
    Wait {
        /// Job ID
        id: String,
        /// Poll interval in seconds
        #[arg(long, default_value = "1")]
        interval: u64,
        /// Timeout in seconds
        #[arg(long, default_value = "60")]
        timeout: u64,
    },
}

impl JobsCmd {
    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
    ) -> Result<i32> {
        match &self.command {
            JobsSubcommand::Get { id } => get(client, format, quiet, id).await,
            JobsSubcommand::Wait {
                id,
                interval,
                timeout,
            } => wait(client, format, quiet, id, *interval, *timeout).await,
        }
    }
}

async fn get(client: &OrionClient, format: &OutputFormat, quiet: bool, id: &str) -> Result<i32> {
    let resp: Value = client.get(&format!("/api/v1/data/jobs/{id}")).await?;

    let status = resp["status"].as_str().unwrap_or("unknown");

    if quiet {
        println!("{status}");
        return Ok(status_exit_code(status));
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(status_exit_code(status));
    }

    println!("{}", "Job Details".bold());
    println!("  ID:        {}", resp["id"].as_str().unwrap_or(""));
    println!("  Status:    {}", colorize_job_status(status));
    println!("  Created:   {}", resp["created_at"]);

    if let Some(started) = resp["started_at"].as_str() {
        println!("  Started:   {started}");
    }
    if let Some(completed) = resp["completed_at"].as_str() {
        println!("  Completed: {completed}");
    }

    if status == "completed" {
        if let Some(result) = resp.get("result") {
            println!("\n{}", "Result:".bold());
            println!("{}", serde_json::to_string_pretty(result)?);
        }
    }

    if status == "failed" {
        if let Some(err) = resp["error"].as_str() {
            println!("\n{} {err}", "Error:".red().bold());
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
        eprint!("Waiting for job {id}...");
    }

    loop {
        let resp: Value = client.get(&format!("/api/v1/data/jobs/{id}")).await?;

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
                        println!("{} Job completed", "OK".green().bold());
                        if let Some(result) = resp.get("result") {
                            println!("{}", serde_json::to_string_pretty(result)?);
                        }
                    }
                    "failed" => {
                        let err = resp["error"].as_str().unwrap_or("Unknown error");
                        println!("{} Job failed: {err}", "ERR".red().bold());
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

fn colorize_job_status(status: &str) -> String {
    match status {
        "pending" => "pending".yellow().to_string(),
        "running" => "running".blue().to_string(),
        "completed" => "completed".green().to_string(),
        "failed" => "failed".red().to_string(),
        other => other.to_string(),
    }
}

fn status_exit_code(status: &str) -> i32 {
    match status {
        "completed" => 0,
        "failed" => 1,
        _ => 0,
    }
}
