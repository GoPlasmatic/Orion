use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;
use serde_json::Value;
use tabled::Tabled;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};

#[derive(Args)]
pub struct ConnectorsCmd {
    #[command(subcommand)]
    command: ConnectorsSubcommand,
}

#[derive(Subcommand)]
enum ConnectorsSubcommand {
    /// List all connectors
    List,
    /// Get a connector by ID
    Get {
        /// Connector ID
        id: String,
    },
    /// Create a new connector
    Create {
        /// JSON file path
        #[arg(short, long)]
        file: Option<String>,
        /// Inline JSON data
        #[arg(short, long)]
        data: Option<String>,
    },
    /// Update a connector
    Update {
        /// Connector ID
        id: String,
        /// JSON file path
        #[arg(short, long)]
        file: Option<String>,
        /// Inline JSON data
        #[arg(short, long)]
        data: Option<String>,
    },
    /// Delete a connector
    Delete {
        /// Connector ID
        id: String,
    },
    /// Enable a connector
    Enable {
        /// Connector ID
        id: String,
    },
    /// Disable a connector
    Disable {
        /// Connector ID
        id: String,
    },
}

#[derive(Tabled)]
struct ConnectorRow {
    #[tabled(rename = "ID")]
    id: String,
    #[tabled(rename = "Name")]
    name: String,
    #[tabled(rename = "Type")]
    connector_type: String,
    #[tabled(rename = "Enabled")]
    enabled: String,
}

impl ConnectorsCmd {
    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
        yes: bool,
    ) -> Result<i32> {
        match &self.command {
            ConnectorsSubcommand::List => list(client, format, quiet).await,
            ConnectorsSubcommand::Get { id } => get(client, format, quiet, id).await,
            ConnectorsSubcommand::Create { file, data } => {
                let body = read_input(file.as_deref(), data.as_deref())?;
                create(client, format, quiet, &body).await
            }
            ConnectorsSubcommand::Update { id, file, data } => {
                let body = read_input(file.as_deref(), data.as_deref())?;
                update(client, format, quiet, id, &body).await
            }
            ConnectorsSubcommand::Delete { id } => delete(client, quiet, yes, id).await,
            ConnectorsSubcommand::Enable { id } => toggle(client, quiet, id, true).await,
            ConnectorsSubcommand::Disable { id } => toggle(client, quiet, id, false).await,
        }
    }
}

async fn list(client: &OrionClient, format: &OutputFormat, quiet: bool) -> Result<i32> {
    let resp: Value = client.get("/api/v1/admin/connectors").await?;
    let connectors = resp["data"].as_array().cloned().unwrap_or_default();

    if quiet {
        for c in &connectors {
            if let Some(id) = c["id"].as_str() {
                println!("{id}");
            }
        }
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    if connectors.is_empty() {
        println!("{}", "No connectors found.".dimmed());
        return Ok(0);
    }

    let rows: Vec<ConnectorRow> = connectors
        .iter()
        .map(|c| ConnectorRow {
            id: truncate(c["id"].as_str().unwrap_or(""), 12),
            name: c["name"].as_str().unwrap_or("").to_string(),
            connector_type: c["connector_type"].as_str().unwrap_or("").to_string(),
            enabled: if c["enabled"].as_bool().unwrap_or(false) {
                "yes".green().to_string()
            } else {
                "no".red().to_string()
            },
        })
        .collect();

    output::print_table(rows);
    println!("{}", format!("{} connector(s)", connectors.len()).dimmed());
    Ok(0)
}

async fn get(client: &OrionClient, format: &OutputFormat, quiet: bool, id: &str) -> Result<i32> {
    let resp: Value = client
        .get(&format!("/api/v1/admin/connectors/{id}"))
        .await?;

    if quiet {
        println!("{id}");
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    let conn = &resp["data"];
    println!("{}", "Connector Details".bold());
    println!("  ID:      {}", conn["id"].as_str().unwrap_or(""));
    println!("  Name:    {}", conn["name"].as_str().unwrap_or(""));
    println!(
        "  Type:    {}",
        conn["connector_type"].as_str().unwrap_or("")
    );
    println!(
        "  Enabled: {}",
        if conn["enabled"].as_bool().unwrap_or(false) {
            "yes".green().to_string()
        } else {
            "no".red().to_string()
        }
    );
    println!("  Created: {}", conn["created_at"]);
    println!("  Updated: {}", conn["updated_at"]);

    if let Some(config_str) = conn["config_json"].as_str() {
        if let Ok(config) = serde_json::from_str::<Value>(config_str) {
            println!("\n{}", "Config:".bold());
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
    }

    Ok(0)
}

async fn create(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    body: &Value,
) -> Result<i32> {
    let resp: Value = client.post("/api/v1/admin/connectors", body).await?;
    let conn = &resp["data"];

    if quiet {
        println!("{}", conn["id"].as_str().unwrap_or(""));
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    println!(
        "{} Connector created: {} ({})",
        "OK".green().bold(),
        conn["name"].as_str().unwrap_or(""),
        conn["id"].as_str().unwrap_or("")
    );
    Ok(0)
}

async fn update(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    id: &str,
    body: &Value,
) -> Result<i32> {
    let resp: Value = client
        .put(&format!("/api/v1/admin/connectors/{id}"), body)
        .await?;

    if quiet {
        println!("{id}");
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    let conn = &resp["data"];
    println!(
        "{} Connector updated: {}",
        "OK".green().bold(),
        conn["name"].as_str().unwrap_or(id)
    );
    Ok(0)
}

async fn delete(client: &OrionClient, quiet: bool, yes: bool, id: &str) -> Result<i32> {
    if !yes {
        eprint!("Delete connector {id}? [y/N] ");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(0);
        }
    }

    client
        .delete_request(&format!("/api/v1/admin/connectors/{id}"))
        .await?;

    if !quiet {
        println!("{} Connector {id} deleted", "OK".green().bold());
    }
    Ok(0)
}

async fn toggle(client: &OrionClient, quiet: bool, id: &str, enabled: bool) -> Result<i32> {
    let body = serde_json::json!({ "enabled": enabled });
    let resp: Value = client
        .put(&format!("/api/v1/admin/connectors/{id}"), &body)
        .await?;

    if !quiet {
        let conn = &resp["data"];
        let state = if enabled {
            "enabled".green()
        } else {
            "disabled".red()
        };
        println!(
            "{} Connector {} {state}",
            "OK".green().bold(),
            conn["name"].as_str().unwrap_or(id)
        );
    }
    Ok(0)
}

fn read_input(file: Option<&str>, data: Option<&str>) -> Result<Value> {
    if let Some(path) = file {
        let content = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&content)?)
    } else if let Some(json) = data {
        Ok(serde_json::from_str(json)?)
    } else {
        bail!("Provide input with -f <file> or -d '<json>'")
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}...", &s[..max - 3])
    } else {
        s.to_string()
    }
}
