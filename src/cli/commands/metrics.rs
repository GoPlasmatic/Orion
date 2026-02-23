use anyhow::Result;
use clap::Args;
use colored::Colorize;

use crate::client::OrionClient;

#[derive(Args)]
pub struct MetricsCmd {
    /// Output raw Prometheus format
    #[arg(long)]
    raw: bool,
}

impl MetricsCmd {
    pub async fn run(&self, client: &OrionClient) -> Result<i32> {
        let text = client.get_text("/metrics").await?;

        if self.raw {
            print!("{text}");
            return Ok(0);
        }

        println!("{}", "Metrics".bold());
        println!();

        for line in text.lines() {
            if line.starts_with('#') {
                continue;
            }
            if line.is_empty() {
                continue;
            }

            // Parse metric lines like: metric_name{labels} value
            if let Some((name_labels, value)) = line.rsplit_once(' ') {
                let display_name = name_labels.replace('{', " (").replace('}', ")");
                println!("  {:<60} {}", display_name, value.cyan());
            }
        }

        Ok(0)
    }
}
