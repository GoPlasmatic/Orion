//! `orion-cli connections`: `list`, `get`, `create`, `probe`. Every call goes
//! through `orion_client::V2Admin`.

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use orion_api::v2::CreateConnectionRevisionRequest;
use orion_client::V2Admin;

use super::v2::load_request;
use crate::client::OrionClient;
use crate::output::{self, OutputFormat};

#[derive(Args)]
#[command(
    long_about = "Manage immutable v2 connection revisions and execute audited health probes."
)]
pub struct ConnectionsCmd {
    #[command(subcommand)]
    command: ConnectionsSubcommand,
}

#[derive(Subcommand)]
pub enum ConnectionsSubcommand {
    /// List immutable connection revisions: every connection's, or one's
    List {
        /// Only this connection's revisions
        #[arg(long)]
        id: Option<String>,
    },
    /// Show one connection revision (the latest when no --revision)
    Get {
        /// Connection identifier
        id: String,
        /// Revision to show (defaults to the latest)
        #[arg(long)]
        revision: Option<u64>,
    },
    /// Create a new immutable connection revision
    Create {
        /// Connection identifier
        id: String,
        /// Path to the revision JSON (`revision`, `provider_digest`,
        /// `public_config`, `secret_locators`, `bindings`), or '-' for stdin
        #[arg(short, long)]
        file: String,
    },
    /// Run an audited health probe against a connection revision; exits 2
    /// when the backend is unhealthy
    Probe {
        /// Connection identifier
        id: String,
        /// Revision to probe (defaults to the latest)
        #[arg(long)]
        revision: Option<u64>,
    },
}

/// `revision`, or the highest one the connection has.
async fn resolve_revision(
    client: &OrionClient,
    connection_id: &str,
    revision: Option<u64>,
) -> Result<u64> {
    if let Some(revision) = revision {
        return Ok(revision);
    }
    client
        .list_connection_revisions(connection_id)
        .await?
        .connections
        .iter()
        .map(|c| c.revision)
        .max()
        .with_context(|| format!("connection '{connection_id}' has no revisions"))
}

impl ConnectionsCmd {
    #[cfg(test)]
    pub(super) fn new(command: ConnectionsSubcommand) -> Self {
        Self { command }
    }

    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
    ) -> Result<i32> {
        match &self.command {
            ConnectionsSubcommand::List { id } => {
                let resp = match id {
                    Some(id) => client.list_connection_revisions(id).await?,
                    None => client.list_connections().await?,
                };
                if quiet {
                    for item in &resp.connections {
                        println!("{}@{}", item.connection_id, item.revision);
                    }
                    return Ok(0);
                }
                output::print_value(format, &serde_json::to_value(&resp)?)?;
                Ok(0)
            }
            ConnectionsSubcommand::Get { id, revision } => {
                let revision = resolve_revision(client, id, *revision).await?;
                let resp = client.get_connection_revision(id, revision).await?;
                if quiet {
                    println!("{}", resp.configuration_digest);
                    return Ok(0);
                }
                output::print_value(format, &serde_json::to_value(&resp)?)?;
                Ok(0)
            }
            ConnectionsSubcommand::Create { id, file } => {
                let req: CreateConnectionRevisionRequest =
                    load_request(file, "connection revision")?;
                let resp = client.create_connection_revision(id, &req).await?;
                if quiet {
                    println!("{}", resp.connection.revision);
                    return Ok(0);
                }
                output::print_value(format, &serde_json::to_value(&resp)?)?;
                Ok(0)
            }
            ConnectionsSubcommand::Probe { id, revision } => {
                let revision = resolve_revision(client, id, *revision).await?;
                let resp = client.probe_connection(id, revision).await?;
                if quiet {
                    println!("{}", if resp.healthy { "healthy" } else { "unhealthy" });
                } else {
                    output::print_value(format, &serde_json::to_value(&resp)?)?;
                }
                Ok(if resp.healthy { 0 } else { 2 })
            }
        }
    }
}
