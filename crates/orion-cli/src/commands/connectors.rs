use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use serde_json::Value;
use tabled::Tabled;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};
use crate::utils;
use orion_client::paths;

/// What a connector is, in endpoint terms — the value every shared CRUD
/// helper in `utils` is driven by.
///
/// There is no `VersionedEntityKind` beside this one, and that is the point:
/// connectors are unversioned, so they must not reach the status-transition
/// or version commands. There is no `paths::connector_status` to name either,
/// so the mistake cannot be written.
static KIND: utils::EntityKind = utils::EntityKind {
    title: "Connector",
    label: "connector",
    collection: paths::CONNECTORS,
    export: paths::CONNECTORS_EXPORT,
    validate: paths::CONNECTORS_VALIDATE,
    item: paths::connector,
    // Not `connector_id`: connectors answer with a bare `id`.
    id_field: "id",
};

#[derive(Args)]
#[command(
    long_about = "Manage connectors -- external service connections (HTTP APIs, Kafka) used by workflow tasks.\n\n\
        Connectors are referenced by name in http_call and publish_kafka tasks within workflows.\n\
        They can be enabled/disabled without deletion. Circuit breakers protect against failing connectors.\n\n\
        With --quiet, list prints one ID per line, mutating commands print the resource ID."
)]
pub struct ConnectorsCmd {
    #[command(subcommand)]
    command: ConnectorsSubcommand,
}

#[derive(Subcommand)]
enum ConnectorsSubcommand {
    /// List all connectors
    List {
        /// Filter by tag
        #[arg(long)]
        tag: Option<String>,
        /// Page size (default: 50, max: 1000)
        #[arg(long)]
        limit: Option<i64>,
        /// Page offset
        #[arg(long)]
        offset: Option<i64>,
        /// Sort by column (name, connector_type, created_at, updated_at)
        #[arg(long)]
        sort_by: Option<String>,
        /// Sort direction (asc, desc)
        #[arg(long)]
        sort_order: Option<String>,
    },
    /// Get a connector by ID
    Get {
        /// Connector ID
        id: String,
    },
    /// Create a new connector from JSON
    #[command(after_help = crate::help::CONNECTOR_CREATE)]
    Create {
        /// Path to JSON file containing the connector definition
        #[arg(short, long)]
        file: Option<String>,
        /// Inline JSON string with the connector definition
        #[arg(short, long)]
        data: Option<String>,
        /// Read connector definition from stdin
        #[arg(long)]
        stdin: bool,
    },
    /// Update a connector with new JSON definition
    Update {
        /// Connector ID
        id: String,
        /// Path to JSON file containing the connector definition
        #[arg(short, long)]
        file: Option<String>,
        /// Inline JSON string with the connector definition
        #[arg(short, long)]
        data: Option<String>,
        /// Read connector definition from stdin
        #[arg(long)]
        stdin: bool,
    },
    /// Delete a connector (prompts for confirmation)
    Delete {
        /// Connector ID
        id: String,
    },
    /// Enable a disabled connector
    Enable {
        /// Connector ID
        id: String,
    },
    /// Disable a connector without deleting it
    Disable {
        /// Connector ID
        id: String,
    },
    /// Probe a connector's target: is it reachable with the stored config?
    Test {
        /// Connector ID
        id: String,
    },
    /// Validate a connector definition without creating it
    Validate {
        /// Path to JSON file with the connector definition
        #[arg(short, long)]
        file: Option<String>,
        /// Inline JSON string with the connector definition
        #[arg(short, long)]
        data: Option<String>,
        /// Read connector definition from stdin
        #[arg(long)]
        stdin: bool,
    },
    /// Export connectors as JSON (pipe to file for backup; secrets stay masked)
    Export {
        /// Filter by tag
        #[arg(long)]
        tag: Option<String>,
    },
    /// List circuit breaker states for all connectors
    #[command(
        after_help = "Shows the state (closed, open, half_open) of each circuit breaker.\nClosed = healthy, Open = failing (requests blocked), Half-open = testing recovery."
    )]
    CircuitBreakers,
    /// Reset a tripped circuit breaker to closed state
    #[command(after_help = "Examples:\n  orion-cli connectors reset-breaker my_api:orders-channel")]
    ResetBreaker {
        /// Circuit breaker key in connector:channel format
        key: String,
    },
    /// Bulk import connectors from a JSON array file
    #[command(
        after_help = "Examples:\n  orion-cli connectors import -f connectors.json --dry-run\n  orion-cli connectors import -f connectors.json"
    )]
    Import {
        /// Path to a JSON file containing an array of connector definitions
        #[arg(short, long)]
        file: String,
        /// Validate on the server without writing any changes
        #[arg(long)]
        dry_run: bool,
        /// On ID conflict: fail (default), skip the item, or write a new version
        #[arg(long, value_parser = ["fail", "skip", "new_version"])]
        on_conflict: Option<String>,
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
            ConnectorsSubcommand::List {
                tag,
                limit,
                offset,
                sort_by,
                sort_order,
            } => {
                let qs = utils::build_query_string(&[
                    ("tag", tag.clone()),
                    ("limit", limit.map(|l| l.to_string())),
                    ("offset", offset.map(|o| o.to_string())),
                    ("sort_by", sort_by.clone()),
                    ("sort_order", sort_order.clone()),
                ]);
                list(client, format, quiet, &qs).await
            }
            ConnectorsSubcommand::Get { id } => get(client, format, quiet, id).await,
            ConnectorsSubcommand::Create { file, data, stdin } => {
                let body = utils::read_json_input(file.as_deref(), data.as_deref(), *stdin)?;
                utils::create_entity(client, &KIND, format, quiet, &body).await
            }
            ConnectorsSubcommand::Update {
                id,
                file,
                data,
                stdin,
            } => {
                let body = utils::read_json_input(file.as_deref(), data.as_deref(), *stdin)?;
                utils::update_entity(client, &KIND, format, quiet, id, &body).await
            }
            ConnectorsSubcommand::Delete { id } => {
                utils::delete_entity(client, &KIND, quiet, yes, id).await
            }
            ConnectorsSubcommand::Test { id } => test(client, format, quiet, id).await,
            ConnectorsSubcommand::Validate { file, data, stdin } => {
                let body = utils::read_json_input(file.as_deref(), data.as_deref(), *stdin)?;
                utils::validate_entity(client, &KIND, format, quiet, &body).await
            }
            ConnectorsSubcommand::Export { tag } => {
                let qs = utils::build_query_string(&[("tag", tag.clone())]);
                utils::export_entities(client, &KIND, &qs).await
            }
            ConnectorsSubcommand::Enable { id } => toggle(client, quiet, id, true).await,
            ConnectorsSubcommand::Disable { id } => toggle(client, quiet, id, false).await,
            ConnectorsSubcommand::CircuitBreakers => circuit_breakers(client, format, quiet).await,
            ConnectorsSubcommand::ResetBreaker { key } => reset_breaker(client, quiet, key).await,
            ConnectorsSubcommand::Import {
                file,
                dry_run,
                on_conflict,
            } => {
                utils::run_import(
                    client,
                    format,
                    quiet,
                    utils::ImportRequest {
                        base_path: paths::CONNECTORS_IMPORT,
                        label: "connector",
                        file,
                        dry_run: *dry_run,
                        on_conflict: on_conflict.as_deref(),
                    },
                )
                .await
            }
        }
    }
}

async fn list(client: &OrionClient, format: &OutputFormat, quiet: bool, qs: &str) -> Result<i32> {
    let resp: Value = client.get(&format!("{}{qs}", paths::CONNECTORS)).await?;
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
            id: utils::truncate(c["id"].as_str().unwrap_or(""), 12),
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
    utils::print_list_footer(&resp, connectors.len(), "connector(s)");
    Ok(0)
}

async fn get(client: &OrionClient, format: &OutputFormat, quiet: bool, id: &str) -> Result<i32> {
    let resp: Value = client.get(&paths::connector(id)).await?;

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

    if let Some(config_str) = conn["config_json"].as_str()
        && let Ok(config) = serde_json::from_str::<Value>(config_str)
    {
        println!("\n{}", "Config:".bold());
        println!("{}", serde_json::to_string_pretty(&config)?);
    }

    Ok(0)
}

async fn toggle(client: &OrionClient, quiet: bool, id: &str, enabled: bool) -> Result<i32> {
    let body = serde_json::json!({ "enabled": enabled });
    let resp: Value = client.put(&paths::connector(id), &body).await?;

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

async fn circuit_breakers(client: &OrionClient, format: &OutputFormat, quiet: bool) -> Result<i32> {
    let resp: Value = client.get(paths::CIRCUIT_BREAKERS).await?;

    if quiet {
        let enabled = resp["enabled"].as_bool().unwrap_or(false);
        println!("{enabled}");
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    let enabled = resp["enabled"].as_bool().unwrap_or(false);
    println!(
        "{} Circuit breakers: {}",
        "INFO".bold(),
        if enabled {
            "enabled".green().to_string()
        } else {
            "disabled".red().to_string()
        }
    );

    if let Some(breakers) = resp.get("breakers").and_then(|b| b.as_object()) {
        if breakers.is_empty() {
            println!("{}", "  No active circuit breakers.".dimmed());
        } else {
            for (key, state) in breakers {
                let state_owned = state.to_string();
                let state_str = state.as_str().unwrap_or(&state_owned);
                let colored_state = match state_str {
                    "closed" => state_str.green().to_string(),
                    "open" => state_str.red().to_string(),
                    "half_open" | "half-open" => state_str.yellow().to_string(),
                    _ => state_str.to_string(),
                };
                println!("  {key}: {colored_state}");
            }
        }
    }

    Ok(0)
}

async fn reset_breaker(client: &OrionClient, quiet: bool, key: &str) -> Result<i32> {
    let _: Value = client.post_empty(&paths::circuit_breaker(key)).await?;

    if !quiet {
        println!("{} Circuit breaker '{key}' reset", "OK".green().bold());
    }
    Ok(0)
}

async fn test(client: &OrionClient, format: &OutputFormat, quiet: bool, id: &str) -> Result<i32> {
    let resp: Value = client.post_empty(&paths::connector_test(id)).await?;
    let probe = &resp["data"];
    let reachable = probe["reachable"].as_bool().unwrap_or(false);
    let supported = probe["supported"].as_bool().unwrap_or(false);
    let exit = if reachable || !supported { 0 } else { 1 };

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(exit);
    }

    if quiet {
        println!(
            "{}",
            if !supported {
                "unsupported"
            } else if reachable {
                "reachable"
            } else {
                "unreachable"
            }
        );
        return Ok(exit);
    }

    let kind = probe["connector_type"].as_str().unwrap_or("unknown");
    if !supported {
        println!(
            "{} Probing is not supported for '{kind}' connectors",
            "SKIP".yellow().bold()
        );
    } else if reachable {
        println!(
            "{} Connector {id} ({kind}) is reachable [{}]",
            "OK".green().bold(),
            probe["probe"].as_str().unwrap_or("")
        );
    } else {
        println!(
            "{} Connector {id} ({kind}) is unreachable: {}",
            "FAIL".red().bold(),
            probe["error"].as_str().unwrap_or("unknown error")
        );
    }
    Ok(exit)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As for the other two kinds — and note what is absent: connectors have
    /// no versioned descriptor, so there is nothing here to assert about a
    /// status or versions endpoint. That is the type error doing its job.
    #[test]
    fn the_connector_kind_points_at_the_connector_endpoints() {
        assert_eq!(KIND.collection, "/api/v1/admin/connectors");
        assert_eq!(KIND.export, "/api/v1/admin/connectors/export");
        assert_eq!(KIND.validate, "/api/v1/admin/connectors/validate");
        assert_eq!((KIND.item)("c1"), "/api/v1/admin/connectors/c1");
        assert_eq!(
            KIND.id_field, "id",
            "connectors answer with a bare id, not a prefixed one"
        );
    }
}
