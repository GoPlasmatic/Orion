use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use serde_json::Value;
use tabled::Tabled;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};
use crate::utils;

#[derive(Args)]
#[command(long_about = "Inspect package promotion receipts (v1.0).\n\n\
        A package is the channels, workflows, and connectors of one service,\n\
        promoted between instances as a versioned unit. Receipts record which\n\
        package versions were staged or applied on this instance by\n\
        'orion-server package plan/apply'. Applied versions are\n\
        content-immutable: re-recording the same version with a different\n\
        content hash is refused by the server.")]
pub struct PackagesCmd {
    #[command(subcommand)]
    command: PackagesSubcommand,
}

#[derive(Subcommand)]
enum PackagesSubcommand {
    /// List package receipts
    List {
        /// Page size
        #[arg(long)]
        limit: Option<i64>,
        /// Page offset
        #[arg(long)]
        offset: Option<i64>,
    },
    /// Show a package's current receipt and version history
    Get {
        /// Package name
        name: String,
    },
}

#[derive(Tabled)]
struct PackageRow {
    #[tabled(rename = "Name")]
    name: String,
    #[tabled(rename = "Version")]
    version: String,
    #[tabled(rename = "State")]
    state: String,
    #[tabled(rename = "Principal")]
    principal: String,
    #[tabled(rename = "Updated")]
    updated: String,
}

impl PackagesCmd {
    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
    ) -> Result<i32> {
        match &self.command {
            PackagesSubcommand::List { limit, offset } => {
                list(client, format, quiet, limit, offset).await
            }
            PackagesSubcommand::Get { name } => get(client, format, quiet, name).await,
        }
    }
}

async fn list(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    limit: &Option<i64>,
    offset: &Option<i64>,
) -> Result<i32> {
    let qs = utils::build_query_string(&[
        ("limit", limit.map(|l| l.to_string())),
        ("offset", offset.map(|o| o.to_string())),
    ]);
    let resp: Value = client.get(&format!("/api/v1/admin/packages{qs}")).await?;
    let packages = resp["data"].as_array().cloned().unwrap_or_default();

    if quiet {
        for p in &packages {
            if let Some(name) = p["name"].as_str() {
                println!("{name}");
            }
        }
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    if packages.is_empty() {
        println!("{}", "No package receipts found.".dimmed());
        return Ok(0);
    }

    let rows: Vec<PackageRow> = packages
        .iter()
        .map(|p| PackageRow {
            name: p["name"].as_str().unwrap_or("").to_string(),
            version: p["version"].as_str().unwrap_or("").to_string(),
            state: utils::colorize_status(p["state"].as_str().unwrap_or("")),
            principal: p["principal"].as_str().unwrap_or("").to_string(),
            updated: p["updated_at"].as_str().unwrap_or("").to_string(),
        })
        .collect();

    output::print_table(rows);
    println!("{}", format!("{} package(s)", packages.len()).dimmed());
    Ok(0)
}

async fn get(client: &OrionClient, format: &OutputFormat, quiet: bool, name: &str) -> Result<i32> {
    let resp: Value = client
        .get(&format!("/api/v1/admin/packages/{name}"))
        .await?;
    let detail = &resp["data"];

    if quiet {
        let version = detail["current"]["version"].as_str().unwrap_or("");
        println!("{version}");
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    println!("{}", "Package".bold());
    println!("  Name:    {}", detail["name"].as_str().unwrap_or(name));
    let current = &detail["current"];
    if !current.is_null() {
        println!(
            "  Current: {} ({})",
            current["version"].as_str().unwrap_or(""),
            utils::colorize_status(current["state"].as_str().unwrap_or(""))
        );
        println!(
            "  Hash:    {}",
            current["content_hash"].as_str().unwrap_or("")
        );
        println!(
            "  By:      {} at {}",
            current["principal"].as_str().unwrap_or(""),
            current["updated_at"].as_str().unwrap_or("")
        );
    }

    if let Some(versions) = detail["versions"].as_array()
        && !versions.is_empty()
    {
        println!("\n{}", "History:".bold());
        for v in versions {
            println!(
                "  {} {} ({}, {})",
                v["version"].as_str().unwrap_or(""),
                utils::colorize_status(v["state"].as_str().unwrap_or("")),
                v["principal"].as_str().unwrap_or(""),
                v["updated_at"].as_str().unwrap_or("")
            );
        }
    }

    Ok(0)
}
