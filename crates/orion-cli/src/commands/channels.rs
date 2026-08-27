use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use orion_api::{STATUS_ACTIVE, STATUS_ARCHIVED};
use serde_json::Value;
use tabled::Tabled;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};
use crate::utils::{self, colorize_status, truncate};
use orion_client::paths;

#[derive(Args)]
#[command(
    long_about = "Manage channels -- service endpoints that receive data and route it to a workflow.\n\n\
        Channels define how data enters the system (REST routes, HTTP endpoints, Kafka topics).\n\
        Each channel links to a workflow that processes the incoming data.\n\
        Lifecycle: draft -> activate -> live (activation reloads the engine automatically)\n\n\
        With --quiet, list prints one ID per line, mutating commands print the resource ID."
)]
pub struct ChannelsCmd {
    #[command(subcommand)]
    command: ChannelsSubcommand,
}

#[derive(Subcommand)]
enum ChannelsSubcommand {
    /// List all channels
    #[command(
        after_help = "Examples:\n  orion-cli channels list\n  orion-cli channels list --status active --protocol rest"
    )]
    List {
        /// Filter by status (draft, active, archived)
        #[arg(long)]
        status: Option<String>,
        /// Filter by channel type (sync, async)
        #[arg(long)]
        channel_type: Option<String>,
        /// Filter by protocol (http, rest, kafka)
        #[arg(long)]
        protocol: Option<String>,
        /// Filter by tag
        #[arg(long)]
        tag: Option<String>,
        /// Page size (default: 50, max: 1000)
        #[arg(long)]
        limit: Option<i64>,
        /// Page offset
        #[arg(long)]
        offset: Option<i64>,
        /// Sort by column (priority, name, status, channel_type, protocol, created_at, updated_at)
        #[arg(long)]
        sort_by: Option<String>,
        /// Sort direction (asc, desc)
        #[arg(long)]
        sort_order: Option<String>,
    },
    /// Get a channel by ID
    Get {
        /// Channel ID
        id: String,
    },
    /// Create a new channel from JSON
    #[command(after_help = crate::help::CHANNEL_CREATE)]
    Create {
        /// Path to JSON file containing the channel definition
        #[arg(short, long)]
        file: Option<String>,
        /// Inline JSON string with the channel definition
        #[arg(short, long)]
        data: Option<String>,
        /// Read channel definition from stdin
        #[arg(long)]
        stdin: bool,
    },
    /// Update a channel with new JSON definition
    Update {
        /// Channel ID
        id: String,
        /// Path to JSON file containing the channel definition
        #[arg(short, long)]
        file: Option<String>,
        /// Inline JSON string with the channel definition
        #[arg(short, long)]
        data: Option<String>,
        /// Read channel definition from stdin
        #[arg(long)]
        stdin: bool,
    },
    /// Delete a channel (prompts for confirmation)
    Delete {
        /// Channel ID
        id: String,
    },
    /// Activate a draft channel (the engine reloads automatically)
    Activate {
        /// Channel ID
        id: String,
        /// Pre-flight only: report whether activation would succeed, change nothing
        #[arg(long)]
        dry_run: bool,
        /// Defer the engine reload (batch several changes, then 'engine reload' once)
        #[arg(long)]
        defer_reload: bool,
    },
    /// Archive an active channel (the engine reloads automatically)
    Archive {
        /// Channel ID
        id: String,
        /// Pre-flight only: report whether archiving would succeed, change nothing
        #[arg(long)]
        dry_run: bool,
        /// Defer the engine reload (batch several changes, then 'engine reload' once)
        #[arg(long)]
        defer_reload: bool,
    },
    /// List version history for a channel
    Versions {
        /// Channel ID
        id: String,
        /// Page size (default: 50, max: 1000)
        #[arg(long)]
        limit: Option<i64>,
        /// Page offset
        #[arg(long)]
        offset: Option<i64>,
    },
    /// Create a new draft version of a channel
    NewVersion {
        /// Channel ID
        id: String,
    },
    /// Validate a channel definition without creating it
    Validate {
        /// Path to JSON file with the channel definition
        #[arg(short, long)]
        file: Option<String>,
        /// Inline JSON string with the channel definition
        #[arg(short, long)]
        data: Option<String>,
        /// Read channel definition from stdin
        #[arg(long)]
        stdin: bool,
    },
    /// Export channels as JSON (pipe to file for backup)
    Export {
        /// Filter by status (draft, active, archived)
        #[arg(long)]
        status: Option<String>,
        /// Filter by tag
        #[arg(long)]
        tag: Option<String>,
        /// Filter by channel type (sync, async)
        #[arg(long)]
        channel_type: Option<String>,
        /// Filter by protocol (http, rest, kafka)
        #[arg(long)]
        protocol: Option<String>,
    },
    /// Bulk import channels from a JSON array file
    #[command(
        after_help = "Examples:\n  orion-cli channels import -f channels.json --dry-run\n  orion-cli channels import -f channels.json"
    )]
    Import {
        /// Path to a JSON file containing an array of channel definitions
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
struct ChannelRow {
    #[tabled(rename = "ID")]
    id: String,
    #[tabled(rename = "Name")]
    name: String,
    #[tabled(rename = "Type")]
    channel_type: String,
    #[tabled(rename = "Protocol")]
    protocol: String,
    #[tabled(rename = "Workflow")]
    workflow: String,
    #[tabled(rename = "Status")]
    status: String,
    #[tabled(rename = "Version")]
    version: i64,
}

#[derive(Tabled)]
struct VersionRow {
    #[tabled(rename = "Version")]
    version: i64,
    #[tabled(rename = "Status")]
    status: String,
    #[tabled(rename = "Updated")]
    updated: String,
}

impl ChannelsCmd {
    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
        yes: bool,
    ) -> Result<i32> {
        match &self.command {
            ChannelsSubcommand::List {
                status,
                channel_type,
                protocol,
                tag,
                limit,
                offset,
                sort_by,
                sort_order,
            } => {
                let qs = utils::build_query_string(&[
                    ("status", status.clone()),
                    ("channel_type", channel_type.clone()),
                    ("protocol", protocol.clone()),
                    ("tag", tag.clone()),
                    ("limit", limit.map(|l| l.to_string())),
                    ("offset", offset.map(|o| o.to_string())),
                    ("sort_by", sort_by.clone()),
                    ("sort_order", sort_order.clone()),
                ]);
                list(client, format, quiet, &qs).await
            }
            ChannelsSubcommand::Get { id } => get_channel(client, format, quiet, id).await,
            ChannelsSubcommand::Create { file, data, stdin } => {
                let body = utils::read_json_input(file.as_deref(), data.as_deref(), *stdin)?;
                create(client, format, quiet, &body).await
            }
            ChannelsSubcommand::Update {
                id,
                file,
                data,
                stdin,
            } => {
                let body = utils::read_json_input(file.as_deref(), data.as_deref(), *stdin)?;
                update(client, format, quiet, id, &body).await
            }
            ChannelsSubcommand::Delete { id } => delete(client, quiet, yes, id).await,
            ChannelsSubcommand::Activate {
                id,
                dry_run,
                defer_reload,
            } => {
                change_status(
                    client,
                    format,
                    quiet,
                    id,
                    STATUS_ACTIVE,
                    *dry_run,
                    *defer_reload,
                )
                .await
            }
            ChannelsSubcommand::Archive {
                id,
                dry_run,
                defer_reload,
            } => {
                change_status(
                    client,
                    format,
                    quiet,
                    id,
                    STATUS_ARCHIVED,
                    *dry_run,
                    *defer_reload,
                )
                .await
            }
            ChannelsSubcommand::Validate { file, data, stdin } => {
                let body = utils::read_json_input(file.as_deref(), data.as_deref(), *stdin)?;
                validate(client, format, quiet, &body).await
            }
            ChannelsSubcommand::Export {
                status,
                tag,
                channel_type,
                protocol,
            } => {
                let qs = utils::build_query_string(&[
                    ("status", status.clone()),
                    ("tag", tag.clone()),
                    ("channel_type", channel_type.clone()),
                    ("protocol", protocol.clone()),
                ]);
                export(client, &qs).await
            }
            ChannelsSubcommand::Versions { id, limit, offset } => {
                let qs = utils::build_query_string(&[
                    ("limit", limit.map(|l| l.to_string())),
                    ("offset", offset.map(|o| o.to_string())),
                ]);
                versions(client, format, quiet, id, &qs).await
            }
            ChannelsSubcommand::NewVersion { id } => new_version(client, format, quiet, id).await,
            ChannelsSubcommand::Import {
                file,
                dry_run,
                on_conflict,
            } => {
                utils::run_import(
                    client,
                    format,
                    quiet,
                    utils::ImportRequest {
                        base_path: paths::CHANNELS_IMPORT,
                        label: "channel",
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
    let resp: Value = client.get(&format!("{}{qs}", paths::CHANNELS)).await?;
    let channels = resp["data"].as_array().cloned().unwrap_or_default();

    if quiet {
        for ch in &channels {
            if let Some(id) = ch["channel_id"].as_str() {
                println!("{id}");
            }
        }
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    if channels.is_empty() {
        println!("{}", "No channels found.".dimmed());
        return Ok(0);
    }

    let rows: Vec<ChannelRow> = channels
        .iter()
        .map(|c| ChannelRow {
            id: truncate(c["channel_id"].as_str().unwrap_or(""), 12),
            name: truncate(c["name"].as_str().unwrap_or(""), 25),
            channel_type: c["channel_type"].as_str().unwrap_or("").to_string(),
            protocol: c["protocol"].as_str().unwrap_or("").to_string(),
            workflow: truncate(c["workflow_id"].as_str().unwrap_or("(none)"), 12),
            status: colorize_status(c["status"].as_str().unwrap_or("")),
            version: c["version"].as_i64().unwrap_or(0),
        })
        .collect();

    output::print_table(rows);
    utils::print_list_footer(&resp, channels.len(), "channel(s)");
    Ok(0)
}

async fn get_channel(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    id: &str,
) -> Result<i32> {
    let resp: Value = client.get(&paths::channel(id)).await?;

    if quiet {
        println!("{id}");
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    let ch = &resp["data"];

    println!("{}", "Channel Details".bold());
    println!(
        "  ID:           {}",
        ch["channel_id"].as_str().unwrap_or("")
    );
    println!("  Name:         {}", ch["name"].as_str().unwrap_or(""));
    println!(
        "  Description:  {}",
        ch["description"].as_str().unwrap_or("(none)")
    );
    println!(
        "  Type:         {}",
        ch["channel_type"].as_str().unwrap_or("")
    );
    println!("  Protocol:     {}", ch["protocol"].as_str().unwrap_or(""));
    println!(
        "  Workflow:     {}",
        ch["workflow_id"].as_str().unwrap_or("(none)")
    );
    println!(
        "  Status:       {}",
        colorize_status(ch["status"].as_str().unwrap_or(""))
    );
    println!("  Version:      {}", ch["version"].as_i64().unwrap_or(0));
    println!("  Priority:     {}", ch["priority"].as_i64().unwrap_or(0));

    if let Some(methods) = ch.get("methods").and_then(|m| m.as_array()) {
        let methods_str: Vec<&str> = methods.iter().filter_map(|m| m.as_str()).collect();
        if !methods_str.is_empty() {
            println!("  Methods:      {}", methods_str.join(", "));
        }
    }
    if let Some(route) = ch["route_pattern"].as_str()
        && !route.is_empty()
    {
        println!("  Route:        {route}");
    }
    if let Some(topic) = ch["topic"].as_str()
        && !topic.is_empty()
    {
        println!("  Topic:        {topic}");
    }

    println!("  Created:      {}", ch["created_at"]);
    println!("  Updated:      {}", ch["updated_at"]);

    if let Some(config) = ch.get("config")
        && !config.is_null()
    {
        println!("\n{}", "Config:".bold());
        println!("{}", serde_json::to_string_pretty(config)?);
    }

    Ok(0)
}

async fn create(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    body: &Value,
) -> Result<i32> {
    let resp: Value = client.post(paths::CHANNELS, body).await?;
    let ch = &resp["data"];

    if quiet {
        println!("{}", ch["channel_id"].as_str().unwrap_or(""));
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    println!(
        "{} Channel created: {} ({})",
        "OK".green().bold(),
        ch["name"].as_str().unwrap_or(""),
        ch["channel_id"].as_str().unwrap_or("")
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
    let resp: Value = client.put(&paths::channel(id), body).await?;
    let ch = &resp["data"];

    if quiet {
        println!("{id}");
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    println!(
        "{} Channel updated: {} (v{})",
        "OK".green().bold(),
        ch["name"].as_str().unwrap_or(""),
        ch["version"].as_i64().unwrap_or(0)
    );
    Ok(0)
}

async fn delete(client: &OrionClient, quiet: bool, yes: bool, id: &str) -> Result<i32> {
    if !utils::confirm(&format!("Delete channel {id}?"), yes)? {
        println!("Cancelled.");
        return Ok(0);
    }

    client.delete_request(&paths::channel(id)).await?;

    if !quiet {
        println!("{} Channel {id} deleted", "OK".green().bold());
    }
    Ok(0)
}

async fn change_status(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    id: &str,
    status: &str,
    dry_run: bool,
    defer_reload: bool,
) -> Result<i32> {
    let qs = utils::build_query_string(&[
        ("dry_run", dry_run.then(|| "true".to_string())),
        ("reload", defer_reload.then(|| "defer".to_string())),
    ]);
    let body = serde_json::json!({ "status": status });
    let resp: Value = client
        .patch(&format!("{}{qs}", paths::channel_status(id)), &body)
        .await?;

    // A dry run answers with the `/validate` envelope, not the entity — and a
    // transition that would be refused is reported as `valid: false` inside a
    // 200. Channels have the most to say here: activation needs an active
    // workflow, a route that collides with nothing, and a config that still
    // builds, none of which a client can check for itself.
    if dry_run {
        return utils::print_validation_envelope(
            &resp,
            format,
            quiet,
            "DRY RUN",
            &format!("Channel {id} can change to {status} (nothing written)"),
            &format!("Channel {id} cannot change to {status}"),
        );
    }

    if !quiet {
        let ch = &resp["data"];
        println!(
            "{} Channel {} status changed to {}",
            "OK".green().bold(),
            ch["name"].as_str().unwrap_or(id),
            colorize_status(status)
        );
        if defer_reload {
            println!("  Reload deferred -- run 'orion-cli engine reload' to apply.");
        }
    }
    Ok(0)
}

async fn versions(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    id: &str,
    qs: &str,
) -> Result<i32> {
    let resp: Value = client
        .get(&format!("{}{qs}", paths::channel_versions(id)))
        .await?;
    let vers = resp["data"].as_array().cloned().unwrap_or_default();

    if quiet {
        for v in &vers {
            println!("{}", v["version"].as_i64().unwrap_or(0));
        }
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    if vers.is_empty() {
        println!("{}", "No versions found.".dimmed());
        return Ok(0);
    }

    let rows: Vec<VersionRow> = vers
        .iter()
        .map(|v| VersionRow {
            version: v["version"].as_i64().unwrap_or(0),
            status: colorize_status(v["status"].as_str().unwrap_or("")),
            updated: v["updated_at"].as_str().unwrap_or("").to_string(),
        })
        .collect();

    output::print_table(rows);
    utils::print_list_footer(&resp, vers.len(), "version(s)");
    Ok(0)
}

async fn new_version(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    id: &str,
) -> Result<i32> {
    let resp: Value = client.post_empty(&paths::channel_versions(id)).await?;
    let ch = &resp["data"];

    if quiet {
        println!("{}", ch["version"].as_i64().unwrap_or(0));
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    println!(
        "{} New draft version {} created for channel {}",
        "OK".green().bold(),
        ch["version"].as_i64().unwrap_or(0),
        ch["name"].as_str().unwrap_or(id)
    );
    Ok(0)
}

async fn validate(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    body: &Value,
) -> Result<i32> {
    let resp: Value = client.post(paths::CHANNELS_VALIDATE, body).await?;
    utils::print_validation_envelope(
        &resp,
        format,
        quiet,
        "OK",
        "Channel definition is valid",
        "Channel definition has issues",
    )
}

async fn export(client: &OrionClient, qs: &str) -> Result<i32> {
    let resp: Value = client
        .get(&format!("{}{qs}", paths::CHANNELS_EXPORT))
        .await?;
    let channels = resp.get("data").unwrap_or(&resp);
    println!("{}", serde_json::to_string_pretty(channels)?);
    Ok(0)
}
