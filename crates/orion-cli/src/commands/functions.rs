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
    long_about = "Inspect the workflow task functions registered in the Orion engine.\n\n\
        Functions (e.g. http_call, cache_write, publish_kafka) are the building blocks of a \
        workflow's task sequence. Each exposes an input JSON Schema describing the fields it accepts.\n\n\
        Use --output json to see the full input schema for every function."
)]
pub struct FunctionsCmd {
    #[command(subcommand)]
    command: FunctionsSubcommand,
}

#[derive(Subcommand)]
enum FunctionsSubcommand {
    /// List registered workflow functions and their input schemas
    List,
}

#[derive(Tabled)]
struct FunctionRow {
    #[tabled(rename = "Function")]
    name: String,
    #[tabled(rename = "Category")]
    category: String,
    #[tabled(rename = "Description")]
    description: String,
}

impl FunctionsCmd {
    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
    ) -> Result<i32> {
        match &self.command {
            FunctionsSubcommand::List => list(client, format, quiet).await,
        }
    }
}

async fn list(client: &OrionClient, format: &OutputFormat, quiet: bool) -> Result<i32> {
    let resp: Value = client.get("/api/v1/admin/functions").await?;
    let functions = resp["data"].as_array().cloned().unwrap_or_default();

    if quiet {
        for f in &functions {
            if let Some(name) = f["name"].as_str() {
                println!("{name}");
            }
        }
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    if functions.is_empty() {
        println!("{}", "No functions registered.".dimmed());
        return Ok(0);
    }

    let rows: Vec<FunctionRow> = functions
        .iter()
        .map(|f| FunctionRow {
            name: f["name"].as_str().unwrap_or("").to_string(),
            category: f["category"].as_str().unwrap_or("").to_string(),
            description: utils::truncate(f["description"].as_str().unwrap_or(""), 60),
        })
        .collect();

    output::print_table(rows);
    println!("{}", format!("{} function(s)", functions.len()).dimmed());
    Ok(0)
}
