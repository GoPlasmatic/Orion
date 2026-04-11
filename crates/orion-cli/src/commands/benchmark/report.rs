use anyhow::Result;
use colored::Colorize;
use tabled::{Table, Tabled, settings::Style};

use crate::output::{self, OutputFormat};

use super::stats::BenchmarkReport;

pub fn print_reports(
    reports: &[BenchmarkReport],
    format: &OutputFormat,
    quiet: bool,
) -> Result<()> {
    if reports.is_empty() {
        return Ok(());
    }

    if quiet {
        for r in reports {
            println!("{}: {:.1} req/s", r.scenario, r.throughput_rps);
        }
        return Ok(());
    }

    match format {
        OutputFormat::Json | OutputFormat::Yaml => {
            let value = if reports.len() == 1 {
                serde_json::to_value(&reports[0])?
            } else {
                serde_json::to_value(reports)?
            };
            output::print_value(format, &value)?;
        }
        OutputFormat::Table => {
            if reports.len() == 1 {
                print_single_report(&reports[0]);
            } else {
                print_comparison_table(reports);
            }
        }
    }

    Ok(())
}

fn print_single_report(report: &BenchmarkReport) {
    println!();
    println!(
        "{} {}",
        "Benchmark Results —".bold(),
        report.scenario.cyan().bold()
    );
    println!();

    let success_color = if report.success_rate_pct >= 99.0 {
        "green"
    } else if report.success_rate_pct >= 95.0 {
        "yellow"
    } else {
        "red"
    };
    let rate_str = format!("{:.1}%", report.success_rate_pct);
    let rate_colored = match success_color {
        "green" => rate_str.green(),
        "yellow" => rate_str.yellow(),
        _ => rate_str.red(),
    };

    println!(
        "  {:<14} {} total, {} successful, {} failed ({})",
        "Requests:".dimmed(),
        report.total_requests,
        report.successful,
        report.failed,
        rate_colored,
    );
    println!("  {:<14} {}", "Concurrency:".dimmed(), report.concurrency);
    println!(
        "  {:<14} {:.1}s",
        "Duration:".dimmed(),
        report.total_duration_ms / 1000.0
    );
    println!(
        "  {:<14} {:.1} req/s",
        "Throughput:".dimmed(),
        report.throughput_rps
    );

    println!();
    println!("  {}", "Latency Distribution".bold());
    println!("  {:<8} {:.2}ms", "min".dimmed(), report.latency.min_ms);
    println!("  {:<8} {:.2}ms", "p50".dimmed(), report.latency.p50_ms);
    println!("  {:<8} {:.2}ms", "p90".dimmed(), report.latency.p90_ms);
    println!("  {:<8} {:.2}ms", "p95".dimmed(), report.latency.p95_ms);
    println!("  {:<8} {:.2}ms", "p99".dimmed(), report.latency.p99_ms);
    println!("  {:<8} {:.2}ms", "max".dimmed(), report.latency.max_ms);
    println!("  {:<8} {:.2}ms", "mean".dimmed(), report.latency.mean_ms);
    println!();
}

#[derive(Tabled)]
struct ComparisonRow {
    #[tabled(rename = "Scenario")]
    scenario: String,
    #[tabled(rename = "Req/sec")]
    rps: String,
    #[tabled(rename = "Avg (ms)")]
    avg_ms: String,
    #[tabled(rename = "P99 (ms)")]
    p99_ms: String,
    #[tabled(rename = "Errors")]
    errors: String,
}

fn print_comparison_table(reports: &[BenchmarkReport]) {
    let first = &reports[0];
    println!();
    println!(
        "{} ({} requests, concurrency {})",
        "Benchmark Comparison".bold().cyan(),
        first.total_requests,
        first.concurrency,
    );
    println!();

    let rows: Vec<ComparisonRow> = reports
        .iter()
        .map(|r| ComparisonRow {
            scenario: r.scenario.clone(),
            rps: format!("{:.1}", r.throughput_rps),
            avg_ms: format!("{:.2}", r.latency.mean_ms),
            p99_ms: format!("{:.2}", r.latency.p99_ms),
            errors: r.failed.to_string(),
        })
        .collect();

    let table = Table::new(rows).with(Style::rounded()).to_string();
    println!("{table}");
    println!();
}
