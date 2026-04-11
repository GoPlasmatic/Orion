mod cleanup;
pub mod fixtures;
mod report;
mod runner;
mod setup;
mod stats;

use anyhow::{Result, bail};
use clap::Args;
use colored::Colorize;
use tokio_util::sync::CancellationToken;

use crate::client::OrionClient;
use crate::output::OutputFormat;
use crate::utils;

use fixtures::Scenario;
use stats::BenchmarkReport;

#[derive(Args)]
#[command(
    long_about = "Run a performance benchmark against the Orion server.\n\n\
        By default, runs three built-in scenarios (simple, complex, multi) using\n\
        standard Orion benchmark fixtures, then prints a comparison table.\n\n\
        Use --workflow to benchmark your own workflow with a custom payload.\n\n\
        Built-in scenarios:\n  \
        simple    1 log task — baseline pipeline overhead\n  \
        complex   4-task ecommerce workflow — conditional + enrichment\n  \
        multi     12 workflows on same channel — fan-out at scale",
    after_help = "Examples:\n  \
        orion-cli benchmark\n  \
        orion-cli benchmark --scenario simple -n 500 -c 20\n  \
        orion-cli benchmark --workflow my-workflow --channel orders -d '{\"amount\":100}'\n  \
        orion-cli benchmark --output json\n  \
        orion-cli benchmark --cleanup-only"
)]
pub struct BenchmarkCmd {
    /// Number of requests to send per scenario
    #[arg(short = 'n', long, default_value = "100")]
    requests: usize,

    /// Number of concurrent requests
    #[arg(short, long, default_value = "10")]
    concurrency: usize,

    /// Timeout per request in seconds
    #[arg(long, default_value = "30")]
    timeout: u64,

    /// Built-in scenario to run
    #[arg(long, default_value = "all")]
    scenario: Scenario,

    /// Benchmark an existing workflow by ID (skips built-in scenarios)
    #[arg(long)]
    workflow: Option<String>,

    /// Channel name to send data to (required with --workflow)
    #[arg(long)]
    channel: Option<String>,

    /// Path to JSON file with the data payload (for --workflow)
    #[arg(short, long)]
    file: Option<String>,

    /// Inline JSON string with the data payload (for --workflow)
    #[arg(short, long)]
    data: Option<String>,

    /// Only clean up leftover benchmark resources (skip benchmark)
    #[arg(long)]
    cleanup_only: bool,
}

impl BenchmarkCmd {
    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
    ) -> Result<i32> {
        if self.cleanup_only {
            cleanup::cleanup_all_bench_resources(client, quiet).await?;
            return Ok(0);
        }

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("\n{}", "Interrupted, cleaning up...".yellow());
                cancel_clone.cancel();
            }
        });

        if let Some(workflow_id) = &self.workflow {
            self.run_user_workflow(client, workflow_id, format, quiet, &cancel)
                .await
        } else {
            self.run_builtin_scenarios(client, format, quiet, &cancel)
                .await
        }
    }

    async fn run_user_workflow(
        &self,
        client: &OrionClient,
        workflow_id: &str,
        format: &OutputFormat,
        quiet: bool,
        cancel: &CancellationToken,
    ) -> Result<i32> {
        let channel = self
            .channel
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--workflow requires --channel <name>"))?;

        if self.data.is_none() && self.file.is_none() {
            bail!("--workflow requires a data payload via -d or -f");
        }

        let payload = utils::read_json_input(self.file.as_deref(), self.data.as_deref(), false)?;

        if !quiet {
            eprintln!(
                "\n{} Benchmarking workflow '{}' on channel '{}'",
                "Benchmark".bold().cyan(),
                workflow_id,
                channel,
            );
            eprintln!(
                "{} {} requests, concurrency {}\n",
                "Config:".dimmed(),
                self.requests,
                self.concurrency,
            );
        }

        setup::verify_workflow(client, workflow_id).await?;

        let (results, elapsed) = runner::run_benchmark(
            client,
            channel,
            &payload,
            self.requests,
            self.concurrency,
            self.timeout,
            cancel,
            quiet,
        )
        .await?;

        let report =
            BenchmarkReport::from_results(&results, elapsed, workflow_id, self.concurrency);
        report::print_reports(&[report], format, quiet)?;
        Ok(0)
    }

    async fn run_builtin_scenarios(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
        cancel: &CancellationToken,
    ) -> Result<i32> {
        let scenarios = fixtures::get_scenarios(&self.scenario);

        if !quiet {
            eprintln!(
                "\n{} Running {} scenario(s)",
                "Benchmark".bold().cyan(),
                scenarios.len(),
            );
            eprintln!(
                "{} {} requests, concurrency {}\n",
                "Config:".dimmed(),
                self.requests,
                self.concurrency,
            );
            eprintln!("{}", "Setup".bold());
        }

        // Phase 1: Setup
        let resources = match setup::create_resources(client, &scenarios, quiet).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{} Setup failed: {e}", "Error:".red().bold());
                // Try cleanup even if setup partially succeeded
                let _ = cleanup::cleanup_all_bench_resources(client, quiet).await;
                return Ok(1);
            }
        };

        // Phase 2+3: Run benchmarks per scenario
        if !quiet {
            eprintln!("\n{}", "Benchmark".bold());
        }

        let mut all_reports = Vec::new();
        for res in &resources {
            if cancel.is_cancelled() {
                break;
            }

            if !quiet {
                eprintln!("  {} {}...", "Running:".dimmed(), res.scenario_name);
            }

            match runner::run_benchmark(
                client,
                &res.channel_name,
                &res.payload,
                self.requests,
                self.concurrency,
                self.timeout,
                cancel,
                quiet,
            )
            .await
            {
                Ok((results, elapsed)) => {
                    let report = BenchmarkReport::from_results(
                        &results,
                        elapsed,
                        &res.scenario_name,
                        self.concurrency,
                    );
                    all_reports.push(report);
                }
                Err(e) => {
                    eprintln!(
                        "  {} Scenario '{}' failed: {e}",
                        "Error:".red(),
                        res.scenario_name
                    );
                }
            }
        }

        // Phase 4: Cleanup (always runs)
        if !quiet {
            eprintln!("\n{}", "Cleanup".bold());
        }
        if let Err(e) = cleanup::remove_resources(client, &resources, quiet).await {
            eprintln!("  {} Cleanup failed: {e}", "Warning:".yellow());
        }

        // Phase 5: Report
        report::print_reports(&all_reports, format, quiet)?;

        Ok(0)
    }
}
