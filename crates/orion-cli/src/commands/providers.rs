//! `orion-cli provider`: `list`, `get`, `upload`, `admit`, `revoke`,
//! `scaffold`, `lint`, `pack`. Every call goes through
//! `orion_client::V2Admin`.

use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use orion_api::v2::{
    AdmitPackageRequest, ImplementationKind, RevokePackageRequest, ScaffoldRequest,
    UploadPackageRequest,
};
use orion_client::V2Admin;
use tabled::Tabled;

use super::v2::load_request;
use crate::client::OrionClient;
use crate::output::{self, OutputFormat};

#[derive(Args)]
#[command(
    long_about = "Author, lint, pack, upload, admit, and revoke v2 connector provider packages."
)]
pub struct ProviderCmd {
    #[command(subcommand)]
    command: ProviderSubcommand,
}

#[derive(Subcommand)]
pub enum ProviderSubcommand {
    /// List uploaded and admitted connector provider packages
    List,
    /// Show one connector package, with its admission
    Get {
        /// Package digest (`sha256:<64 hex chars>`)
        digest: String,
    },
    /// Upload a verified connector package archive/bundle JSON
    Upload {
        /// Path to package upload JSON file (or '-' for stdin)
        #[arg(short, long)]
        file: String,
    },
    /// Admit an uploaded connector package by its SHA-256 digest
    Admit {
        /// Package digest (`sha256:<64 hex chars>`)
        digest: String,
    },
    /// Revoke a connector package by its SHA-256 digest
    Revoke {
        /// Package digest (`sha256:<64 hex chars>`)
        digest: String,
        /// Reason for emergency revocation
        #[arg(long)]
        reason: Option<String>,
    },
    /// Generate a deterministic v2 provider package scaffold
    Scaffold {
        /// Publisher namespace (e.g. `org.acme`)
        #[arg(long)]
        publisher: String,
        /// Provider identifier (e.g. `orders-api`)
        #[arg(long)]
        provider_id: String,
        /// Semantic version (e.g. `1.0.0`)
        #[arg(long, default_value = "1.0.0")]
        version: String,
        /// Implementation kind: `rust` (the default), `workflow` or `wasm`.
        ///
        /// A `rust` package names a native provider compiled into the node, and
        /// runs on a node that registers a backend under its provider id. A
        /// `workflow` package runs on every node once admitted: one dataflow
        /// workflow per operation, held to the hermetic rules (the dataflow
        /// built-ins, `crypto`, `jwt_sign`, `jwt_verify`, plugin functions, and
        /// `connector_call` only on its declared dependencies; no secrets, no
        /// helper workflows). A `wasm` package is accepted and can be admitted,
        /// but no Community node executes it yet. `orion-cli deployments
        /// capabilities` lists the registered backends.
        #[arg(long, default_value = "rust")]
        kind: ImplementationKind,
    },
    /// Lint a provider package manifest and schema bundle
    Lint {
        /// Path to package bundle JSON file (or '-' for stdin)
        #[arg(short, long)]
        file: String,
    },
    /// Pack a provider package into a deterministic content-addressed archive
    Pack {
        /// Path to package bundle JSON file (or '-' for stdin)
        #[arg(short, long)]
        file: String,
    },
}

#[derive(Tabled)]
struct ProviderRow {
    #[tabled(rename = "Publisher")]
    publisher: String,
    #[tabled(rename = "Provider ID")]
    provider_id: String,
    #[tabled(rename = "Version")]
    version: String,
    #[tabled(rename = "Kind")]
    implementation: String,
    #[tabled(rename = "Admitted")]
    admitted: String,
    #[tabled(rename = "Digest")]
    digest: String,
}

impl ProviderCmd {
    #[cfg(test)]
    pub(super) fn new(command: ProviderSubcommand) -> Self {
        Self { command }
    }

    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
    ) -> Result<i32> {
        match &self.command {
            ProviderSubcommand::List => {
                let resp = client.list_connector_packages().await?;
                if quiet {
                    for item in &resp.packages {
                        println!("{}", item.digest);
                    }
                    return Ok(0);
                }
                if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
                    output::print_value(format, &serde_json::to_value(&resp)?)?;
                    return Ok(0);
                }
                if resp.packages.is_empty() {
                    println!("{}", "No v2 connector packages found.".dimmed());
                    return Ok(0);
                }
                let rows: Vec<ProviderRow> = resp
                    .packages
                    .iter()
                    .map(|p| ProviderRow {
                        publisher: p.publisher.clone(),
                        provider_id: p.provider_id.clone(),
                        version: p.version.clone(),
                        implementation: p.implementation.to_string(),
                        admitted: p.admitted.to_string(),
                        digest: p.digest.clone(),
                    })
                    .collect();
                output::print_table(rows);
                Ok(0)
            }
            ProviderSubcommand::Get { digest } => {
                let resp = client.get_connector_package(digest).await?;
                if quiet {
                    println!("{}", resp.package.digest);
                    return Ok(0);
                }
                output::print_value(format, &serde_json::to_value(&resp)?)?;
                Ok(0)
            }
            ProviderSubcommand::Upload { file } => {
                let req: UploadPackageRequest = load_request(file, "package upload payload")?;
                let resp = client.upload_connector_package(&req).await?;
                if quiet {
                    println!("{}", resp.package.digest);
                    return Ok(0);
                }
                output::print_value(format, &serde_json::to_value(&resp)?)?;
                Ok(0)
            }
            ProviderSubcommand::Admit { digest } => {
                let resp = client
                    .admit_connector_package(digest, &AdmitPackageRequest::default())
                    .await?;
                if quiet {
                    println!("{digest}");
                    return Ok(0);
                }
                output::print_value(format, &serde_json::to_value(&resp)?)?;
                Ok(0)
            }
            ProviderSubcommand::Revoke { digest, reason } => {
                let resp = client
                    .revoke_connector_package(
                        digest,
                        &RevokePackageRequest {
                            reason: reason.clone(),
                        },
                    )
                    .await?;
                if quiet {
                    println!("{digest}");
                    return Ok(0);
                }
                output::print_value(format, &serde_json::to_value(&resp)?)?;
                Ok(0)
            }
            ProviderSubcommand::Scaffold {
                publisher,
                provider_id,
                version,
                kind,
            } => {
                let resp = client
                    .authoring_scaffold(&ScaffoldRequest {
                        publisher: publisher.clone(),
                        provider_id: provider_id.clone(),
                        version: version.clone(),
                        kind: *kind,
                    })
                    .await?;
                output::print_value(format, &serde_json::to_value(&resp)?)?;
                Ok(0)
            }
            ProviderSubcommand::Lint { file } => {
                let req: UploadPackageRequest = load_request(file, "provider bundle")?;
                let resp = client.authoring_lint(&req).await?;
                let valid = resp.valid;
                output::print_value(format, &serde_json::to_value(&resp)?)?;
                Ok(if valid { 0 } else { 2 })
            }
            ProviderSubcommand::Pack { file } => {
                let req: UploadPackageRequest = load_request(file, "provider bundle")?;
                let resp = client.authoring_pack(&req).await?;
                if quiet {
                    println!("{}", resp.package_digest);
                    return Ok(0);
                }
                output::print_value(format, &serde_json::to_value(&resp)?)?;
                Ok(0)
            }
        }
    }
}
