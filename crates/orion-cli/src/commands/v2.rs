//! V2 CLI commands (`V2-07.2`–`V2-07.5`, `V2-16.4`):
//! - `orion-cli deployments` (`capabilities`, `plan`, `apply`, `get`, `wait`, `rollback`, `migration-report`, `edition-preflight`)
//! - `orion-cli provider` (`list`, `upload`, `admit`, `revoke`, `scaffold`, `lint`, `pack`)
//! - `orion-cli connections` (`list`, `create`, `probe`)

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use colored::Colorize;
use serde_json::{Value, json};
use tabled::Tabled;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};
use crate::utils;
use orion_client::paths;

fn load_file_or_stdin(file: &str) -> Result<Value> {
    if file == "-" {
        utils::read_json_input(None, None, true)
    } else {
        utils::read_json_input(Some(file), None, false)
    }
}

// ============================================================================
// 1. `orion-cli deployments` (V2-07.1, V2-07.4, V2-07.5, V2-16.4)
// ============================================================================

#[derive(Args)]
#[command(long_about = "Manage immutable v2 deployments, convergence, rollback, and migration preflight (`V2-07.4`, `V2-07.5`, `V2-16.4`).")]
pub struct DeploymentsCmd {
    #[command(subcommand)]
    command: DeploymentsSubcommand,
}

#[derive(Subcommand)]
enum DeploymentsSubcommand {
    /// Inspect v2 runtime capabilities and Community/Enterprise assembly boundary
    Capabilities,
    /// Create a candidate deployment plan from a JSON manifest file
    Plan {
        /// Path to candidate manifest JSON file (or '-' for stdin)
        #[arg(short, long)]
        file: String,
        /// Base revision expected by the plan (defaults to 0 for initial revision)
        #[arg(long, default_value_t = 0)]
        base_revision: u64,
        /// Optional human-readable reason for the deployment
        #[arg(long)]
        reason: Option<String>,
    },
    /// Commit an idempotent CAS deployment revision
    Apply {
        /// Path to candidate manifest JSON file (or '-' for stdin)
        #[arg(short, long)]
        file: String,
        /// Expected current deployment head revision
        #[arg(long)]
        base_revision: u64,
        /// Reviewed manifest digest (`sha256:...`) produced by `deployments plan`
        #[arg(long)]
        reviewed_digest: String,
        /// Client-generated idempotency key for safe retries
        #[arg(long)]
        idempotency_key: String,
        /// Optional wait timeout in milliseconds (0 = do not wait for convergence)
        #[arg(long, default_value_t = 0)]
        wait_ms: u64,
        /// Convergence wait target (`this-node` or `observed`)
        #[arg(long, default_value = "this-node")]
        wait_target: String,
    },
    /// Show a committed deployment revision
    Get {
        /// Deployment revision number
        revision: u64,
    },
    /// Wait for node/cluster convergence on a committed deployment revision
    Wait {
        /// Committed deployment revision number
        revision: u64,
        /// Convergence target (`this-node` or `observed`)
        #[arg(long, default_value = "this-node")]
        target: String,
        /// Maximum wait time in milliseconds (default 60000ms)
        #[arg(long, default_value_t = 60_000)]
        timeout_ms: u64,
    },
    /// Roll back to a historical deployment revision by committing a new monotonic head revision
    Rollback {
        /// Historical revision to restore
        #[arg(long)]
        target_revision: u64,
        /// Expected current head revision
        #[arg(long)]
        expected_head_revision: u64,
        /// Client-generated idempotency key
        #[arg(long)]
        idempotency_key: String,
        /// Optional rollback reason
        #[arg(long)]
        reason: Option<String>,
    },
    /// Run a legacy v1 -> v2 conversion & migration report
    MigrationReport {
        /// Path to JSON file containing legacy connectors/channels snapshot (or '-' for stdin)
        #[arg(short, long)]
        file: String,
    },
    /// Run Enterprise-to-Community edition transition preflight check (`V2-16.4`)
    EditionPreflight {
        /// Optional revision to inspect (defaults to active head revision)
        #[arg(long)]
        revision: Option<u64>,
    },
}

impl DeploymentsCmd {
    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
    ) -> Result<i32> {
        match &self.command {
            DeploymentsSubcommand::Capabilities => {
                let resp: Value = client.get(paths::V2_RUNTIME_CAPABILITIES).await?;
                if quiet {
                    println!(
                        "{}",
                        resp["data"]["edition"].as_str().unwrap_or("community")
                    );
                    return Ok(0);
                }
                output::print_value(format, &resp)?;
                Ok(0)
            }
            DeploymentsSubcommand::Plan {
                file,
                base_revision,
                reason,
            } => {
                let manifest = load_file_or_stdin(file)?;
                let body = json!({
                    "base_revision": base_revision,
                    "manifest": manifest,
                    "reason": reason,
                });
                let resp: Value = client.post(paths::V2_DEPLOYMENT_PLANS, &body).await?;
                if quiet {
                    if let Some(digest) = resp["data"]["reviewed_manifest_digest"].as_str() {
                        println!("{digest}");
                    }
                    return Ok(0);
                }
                output::print_value(format, &resp)?;
                Ok(0)
            }
            DeploymentsSubcommand::Apply {
                file,
                base_revision,
                reviewed_digest,
                idempotency_key,
                wait_ms,
                wait_target,
            } => {
                let manifest = load_file_or_stdin(file)?;
                let body = json!({
                    "base_revision": base_revision,
                    "reviewed_manifest_digest": reviewed_digest,
                    "idempotency_key": idempotency_key,
                    "manifest": manifest,
                });
                let resp: Value = client.post(paths::V2_DEPLOYMENTS, &body).await?;
                let committed_rev = resp["data"]["committed_revision"].as_u64().unwrap_or(0);
                if quiet && *wait_ms == 0 {
                    println!("{committed_rev}");
                    return Ok(0);
                }
                if !quiet {
                    output::print_value(format, &resp)?;
                }
                if *wait_ms > 0 && committed_rev > 0 {
                    return wait_for_convergence(
                        client,
                        format,
                        quiet,
                        committed_rev,
                        wait_target,
                        *wait_ms,
                    )
                    .await;
                }
                Ok(0)
            }
            DeploymentsSubcommand::Get { revision } => {
                let resp: Value = client.get(&paths::v2_deployment(*revision)).await?;
                if quiet {
                    if let Some(digest) = resp["data"]["manifest_digest"].as_str() {
                        println!("{digest}");
                    }
                    return Ok(0);
                }
                output::print_value(format, &resp)?;
                Ok(0)
            }
            DeploymentsSubcommand::Wait {
                revision,
                target,
                timeout_ms,
            } => wait_for_convergence(client, format, quiet, *revision, target, *timeout_ms).await,
            DeploymentsSubcommand::Rollback {
                target_revision,
                expected_head_revision,
                idempotency_key,
                reason,
            } => {
                let body = json!({
                    "target_revision": target_revision,
                    "expected_head_revision": expected_head_revision,
                    "idempotency_key": idempotency_key,
                    "reason": reason,
                });
                let resp: Value = client.post(paths::V2_DEPLOYMENTS_ROLLBACK, &body).await?;
                if quiet {
                    if let Some(rev) = resp["data"]["committed_revision"].as_u64() {
                        println!("{rev}");
                    }
                    return Ok(0);
                }
                output::print_value(format, &resp)?;
                Ok(0)
            }
            DeploymentsSubcommand::MigrationReport { file } => {
                let payload = load_file_or_stdin(file)?;
                let resp: Value = client.post(paths::V2_MIGRATION_REPORT, &payload).await?;
                let can_cutover = resp["data"]["can_cutover"].as_bool().unwrap_or(false);
                output::print_value(format, &resp)?;
                Ok(if can_cutover { 0 } else { 2 })
            }
            DeploymentsSubcommand::EditionPreflight { revision } => {
                let body = json!({ "revision": revision });
                let resp: Value = client
                    .post(paths::V2_EDITION_TRANSITION_PREFLIGHT, &body)
                    .await?;
                let allowed = resp["data"]["transition_allowed"]
                    .as_bool()
                    .unwrap_or(false);
                output::print_value(format, &resp)?;
                Ok(if allowed { 0 } else { 2 })
            }
        }
    }
}

async fn wait_for_convergence(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    revision: u64,
    target: &str,
    timeout_ms: u64,
) -> Result<i32> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(50));
    let path = paths::v2_deployment_convergence(revision, Some(target));
    loop {
        let resp: Value = client.get(&path).await?;
        let converged = resp["data"]["converged"].as_bool().unwrap_or(false);
        if converged {
            if quiet {
                println!("{revision}");
            } else {
                output::print_value(format, &resp)?;
            }
            return Ok(0);
        }
        if Instant::now() >= deadline {
            // V2-07.4: wait timeout exits nonzero while printing committed revision.
            eprintln!(
                "{} Convergence wait timed out for committed revision {revision}",
                "Timeout:".yellow().bold()
            );
            println!("{revision}");
            return Ok(3);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// ============================================================================
// 2. `orion-cli provider` (V2-07.2, V2-07.3)
// ============================================================================

#[derive(Args)]
#[command(long_about = "Author, lint, pack, upload, admit, and revoke v2 connector provider packages (`V2-07.2`, `V2-07.3`).")]
pub struct ProviderCmd {
    #[command(subcommand)]
    command: ProviderSubcommand,
}

#[derive(Subcommand)]
enum ProviderSubcommand {
    /// List uploaded and admitted connector provider packages
    List,
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
    /// Generate a deterministic v2 provider package scaffold (`V2-07.2`)
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
        /// Implementation kind (`rust`, `workflow`, or `wasm`)
        #[arg(long, default_value = "wasm")]
        kind: String,
    },
    /// Lint a provider package manifest and schema bundle (`V2-07.2`)
    Lint {
        /// Path to package bundle JSON file (or '-' for stdin)
        #[arg(short, long)]
        file: String,
    },
    /// Pack a provider package into a deterministic content-addressed archive (`V2-07.2`)
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
    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
    ) -> Result<i32> {
        match &self.command {
            ProviderSubcommand::List => {
                let resp: Value = client.get(paths::V2_CONNECTOR_PACKAGES).await?;
                let items = resp["data"].as_array().cloned().unwrap_or_default();
                if quiet {
                    for item in &items {
                        if let Some(d) = item["digest"].as_str() {
                            println!("{d}");
                        }
                    }
                    return Ok(0);
                }
                if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
                    output::print_value(format, &resp)?;
                    return Ok(0);
                }
                if items.is_empty() {
                    println!("{}", "No v2 connector packages found.".dimmed());
                    return Ok(0);
                }
                let rows: Vec<ProviderRow> = items
                    .iter()
                    .map(|p| ProviderRow {
                        publisher: p["publisher"].as_str().unwrap_or("").to_string(),
                        provider_id: p["provider_id"].as_str().unwrap_or("").to_string(),
                        version: p["version"].as_str().unwrap_or("").to_string(),
                        implementation: p["implementation"].as_str().unwrap_or("").to_string(),
                        admitted: p["admitted"].as_bool().unwrap_or(false).to_string(),
                        digest: p["digest"].as_str().unwrap_or("").to_string(),
                    })
                    .collect();
                output::print_table(rows);
                Ok(0)
            }
            ProviderSubcommand::Upload { file } => {
                let body = load_file_or_stdin(file)?;
                let resp: Value = client.post(paths::V2_CONNECTOR_PACKAGES, &body).await?;
                if quiet {
                    if let Some(d) = resp["data"]["digest"].as_str() {
                        println!("{d}");
                    }
                    return Ok(0);
                }
                output::print_value(format, &resp)?;
                Ok(0)
            }
            ProviderSubcommand::Admit { digest } => {
                let resp: Value = client
                    .post_empty(&paths::v2_connector_package_admit(digest))
                    .await?;
                if quiet {
                    println!("{digest}");
                    return Ok(0);
                }
                output::print_value(format, &resp)?;
                Ok(0)
            }
            ProviderSubcommand::Revoke { digest, reason } => {
                let body = json!({ "reason": reason });
                let resp: Value = client
                    .post(&paths::v2_connector_package_revoke(digest), &body)
                    .await?;
                if quiet {
                    println!("{digest}");
                    return Ok(0);
                }
                output::print_value(format, &resp)?;
                Ok(0)
            }
            ProviderSubcommand::Scaffold {
                publisher,
                provider_id,
                version,
                kind,
            } => {
                let body = json!({
                    "publisher": publisher,
                    "provider_id": provider_id,
                    "version": version,
                    "implementation": kind,
                });
                let resp: Value = client.post(paths::V2_AUTHORING_SCAFFOLD, &body).await?;
                output::print_value(format, &resp)?;
                Ok(0)
            }
            ProviderSubcommand::Lint { file } => {
                let body = load_file_or_stdin(file)
                    .with_context(|| format!("failed to read provider bundle '{file}'"))?;
                let resp: Value = client.post(paths::V2_AUTHORING_LINT, &body).await?;
                let valid = resp["data"]["valid"].as_bool().unwrap_or(false);
                output::print_value(format, &resp)?;
                Ok(if valid { 0 } else { 2 })
            }
            ProviderSubcommand::Pack { file } => {
                let body = load_file_or_stdin(file)?;
                let resp: Value = client.post(paths::V2_AUTHORING_PACK, &body).await?;
                if quiet {
                    if let Some(d) = resp["data"]["package_digest"].as_str() {
                        println!("{d}");
                    }
                    return Ok(0);
                }
                output::print_value(format, &resp)?;
                Ok(0)
            }
        }
    }
}

// ============================================================================
// 3. `orion-cli connections` (V2-07.3)
// ============================================================================

#[derive(Args)]
#[command(long_about = "Manage immutable v2 connection revisions and execute audited health probes (`V2-07.3`).")]
pub struct ConnectionsCmd {
    #[command(subcommand)]
    command: ConnectionsSubcommand,
}

#[derive(Subcommand)]
enum ConnectionsSubcommand {
    /// List immutable connection revisions
    List,
    /// Create a new immutable connection revision
    Create {
        /// Path to connection definition JSON file (or '-' for stdin)
        #[arg(short, long)]
        file: String,
    },
    /// Execute a short audited health probe against a connection
    Probe {
        /// Connection identifier
        id: String,
        /// Optional revision to probe (defaults to latest revision)
        #[arg(long)]
        revision: Option<u64>,
    },
}

impl ConnectionsCmd {
    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
    ) -> Result<i32> {
        match &self.command {
            ConnectionsSubcommand::List => {
                let resp: Value = client.get(paths::V2_CONNECTIONS).await?;
                if quiet {
                    if let Some(arr) = resp["data"].as_array() {
                        for item in arr {
                            if let Some(id) = item["connection_id"].as_str() {
                                println!("{id}");
                            }
                        }
                    }
                    return Ok(0);
                }
                output::print_value(format, &resp)?;
                Ok(0)
            }
            ConnectionsSubcommand::Create { file } => {
                let body = load_file_or_stdin(file)?;
                let resp: Value = client.post(paths::V2_CONNECTIONS, &body).await?;
                if quiet {
                    if let Some(rev) = resp["data"]["revision"].as_u64() {
                        println!("{rev}");
                    }
                    return Ok(0);
                }
                output::print_value(format, &resp)?;
                Ok(0)
            }
            ConnectionsSubcommand::Probe { id, revision } => {
                let body = json!({ "revision": revision });
                let resp: Value = client.post(&paths::v2_connection_probe(id), &body).await?;
                if quiet {
                    if let Some(s) = resp["data"]["status"].as_str() {
                        println!("{s}");
                    }
                    return Ok(0);
                }
                output::print_value(format, &resp)?;
                Ok(0)
            }
        }
    }
}
