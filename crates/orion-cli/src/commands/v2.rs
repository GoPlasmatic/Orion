//! Orion v2 CLI commands.
//!
//! Exposes first-class CLI subcommands for:
//! - `deployments`: `capabilities`, `plan`, `apply`, `get`, `wait`, `rollback`, `migration-report`, `edition-preflight`
//! - `provider`: `list`, `upload`, `admit`, `revoke`, `scaffold`, `lint`, `pack`
//! - `connections`: `list`, `create`, `probe`

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use colored::Colorize;
use orion_api::v2::{
    AdmitPackageResponse, ApplyDeploymentRequest, ApplyDeploymentResponse, ConnectionProbeResponse,
    ConnectionsListResponse, ConnectorPackagesListResponse, ConvergenceStatusResponse,
    CreateConnectionRevisionRequest, CreateConnectionRevisionResponse, CreateDeploymentPlanRequest,
    DeploymentPlanResponse, DeploymentRevisionResponse, EditionTransitionPreflightRequest,
    EditionTransitionPreflightResponse, LintResponse, MigrationReportRequest,
    MigrationReportResponse, PackResponse, ProbeConnectionRequest, RevokePackageRequest,
    RevokePackageResponse, RollbackDeploymentRequest, RollbackDeploymentResponse,
    RuntimeCapabilitiesResponse, ScaffoldRequest, ScaffoldResponse, UploadPackageRequest,
    UploadPackageResponse,
};
use orion_client::paths;
use tabled::Tabled;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};

fn load_file_or_stdin(file: &str) -> Result<serde_json::Value> {
    if file == "-" {
        crate::utils::read_json_input(None, None, true)
    } else {
        crate::utils::read_json_input(Some(file), None, false)
    }
}

// ============================================================================
// 1. `orion-cli deployments` (V2-07.4, V2-07.5, V2-16.4)
// ============================================================================

#[derive(Args)]
#[command(
    long_about = "Manage immutable v2 deployments, candidate plans, CAS apply, convergence polling, rollback, and migration/edition preflight."
)]
pub struct DeploymentsCmd {
    #[command(subcommand)]
    command: DeploymentsSubcommand,
}

#[derive(Subcommand)]
pub enum DeploymentsSubcommand {
    /// Discover server runtime capabilities and Community/Enterprise assembly boundary
    Capabilities,
    /// Plan and validate a candidate deployment against a base revision: a
    /// manifest file (-f), or a compiled deployment directory (-d)
    Plan {
        /// Path to candidate manifest JSON file (or '-' for stdin)
        #[arg(short, long, required_unless_present = "dir", conflicts_with = "dir")]
        file: Option<String>,
        /// A directory `orion-server compile --format deployment` wrote. Its
        /// definitions and connection revisions are stored on the target and
        /// its members assigned revisions against the target's head, which is
        /// the base revision
        #[arg(short, long, value_name = "DIR")]
        dir: Option<String>,
        /// Expected current head revision (0 for initial deployment). With
        /// -d, the plan is refused unless the head is this revision
        #[arg(long)]
        base_revision: Option<u64>,
        /// Optional change reason
        #[arg(long)]
        reason: Option<String>,
    },
    /// Atomically apply a reviewed candidate deployment under CAS + idempotency:
    /// a manifest file (-f), or a compiled deployment directory (-d)
    Apply {
        /// Path to candidate manifest JSON file (or '-' for stdin)
        #[arg(short, long, required_unless_present = "dir", conflicts_with = "dir")]
        file: Option<String>,
        /// A directory `orion-server compile --format deployment` wrote:
        /// stored, planned against the target's head and applied with the
        /// plan's reviewed digest. A directory the head already deploys is
        /// not applied again
        #[arg(short, long, value_name = "DIR")]
        dir: Option<String>,
        /// Expected current head revision. Required with -f; with -d, the
        /// apply is refused unless the head is this revision
        #[arg(long, required_unless_present = "dir")]
        base_revision: Option<u64>,
        /// Reviewed manifest SHA-256 digest returned by `deployments plan`
        #[arg(long)]
        reviewed_digest: Option<String>,
        /// Client-generated idempotency key. Required with -f; with -d it
        /// defaults to one derived from the head and the manifest digest, so
        /// a retry of the same apply replays it
        #[arg(long, required_unless_present = "dir")]
        idempotency_key: Option<String>,
        /// Audit reason for applying this deployment
        #[arg(long, default_value = "cli apply")]
        reason: String,
        /// Poll convergence after commit until timeout in milliseconds (0 disables wait)
        #[arg(long, default_value_t = 0)]
        wait_ms: u64,
        /// Convergence wait target (`this-node` or `observed`)
        #[arg(long, default_value = "this-node")]
        wait_target: String,
    },
    /// Fetch a committed deployment revision by revision number
    Get {
        /// Deployment revision number
        revision: u64,
    },
    /// Poll node/cluster convergence for a committed deployment revision
    Wait {
        /// Committed deployment revision number
        revision: u64,
        /// Convergence target (`this-node` or `observed`)
        #[arg(long, default_value = "this-node")]
        target: String,
        /// Maximum wait time in milliseconds
        #[arg(long, default_value_t = 10_000)]
        timeout_ms: u64,
    },
    /// Roll back by creating a new forward deployment revision from a historical target revision
    Rollback {
        /// Historical revision to restore
        #[arg(long)]
        target_revision: u64,
        /// Expected current head revision for CAS validation
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
    /// Run Enterprise-to-Community edition transition preflight check
    EditionPreflight {
        /// Optional revision to inspect (defaults to active head revision)
        #[arg(long)]
        revision: Option<u64>,
    },
}

impl DeploymentsCmd {
    #[cfg(test)]
    fn new(command: DeploymentsSubcommand) -> Self {
        Self { command }
    }

    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
    ) -> Result<i32> {
        match &self.command {
            DeploymentsSubcommand::Capabilities => {
                let resp: RuntimeCapabilitiesResponse =
                    client.get(paths::V2_RUNTIME_CAPABILITIES).await?;
                if quiet {
                    let ed = if resp.edition.is_empty() {
                        "community"
                    } else {
                        resp.edition.as_str()
                    };
                    println!("{ed}");
                    return Ok(0);
                }
                let val = serde_json::to_value(&resp)?;
                output::print_value(format, &val)?;
                Ok(0)
            }
            DeploymentsSubcommand::Plan {
                file,
                dir,
                base_revision,
                reason,
            } => {
                if let Some(dir) = dir {
                    return plan_directory(
                        client,
                        format,
                        quiet,
                        dir,
                        *base_revision,
                        reason,
                        false,
                    )
                    .await
                    .map(|_| 0);
                }
                let manifest = load_file_or_stdin(file.as_deref().unwrap_or("-"))?;
                let body = CreateDeploymentPlanRequest {
                    base_revision: base_revision.unwrap_or(0),
                    manifest,
                    reason: reason.clone(),
                };
                let resp: DeploymentPlanResponse =
                    client.post_dto(paths::V2_DEPLOYMENT_PLANS, &body).await?;
                if quiet {
                    let digest = if !resp.reviewed_manifest_digest.is_empty() {
                        &resp.reviewed_manifest_digest
                    } else {
                        &resp.candidate_digest
                    };
                    if !digest.is_empty() {
                        println!("{digest}");
                    }
                    return Ok(0);
                }
                let val = serde_json::to_value(&resp)?;
                output::print_value(format, &val)?;
                Ok(0)
            }
            DeploymentsSubcommand::Apply {
                file,
                dir,
                base_revision,
                reviewed_digest,
                idempotency_key,
                reason,
                wait_ms,
                wait_target,
            } => {
                let body = match dir {
                    Some(dir) => {
                        let planned = plan_directory(
                            client,
                            format,
                            quiet,
                            dir,
                            *base_revision,
                            &Some(reason.clone()),
                            true,
                        )
                        .await?;
                        let Some((prepared, plan)) = planned else {
                            // The head deploys it already; nothing to apply.
                            return Ok(0);
                        };
                        if let Some(reviewed) = reviewed_digest
                            && reviewed != &plan.reviewed_manifest_digest
                        {
                            anyhow::bail!(
                                "--reviewed-digest {reviewed} is not the plan's {}: the \
                                 directory or the target changed since it was reviewed",
                                plan.reviewed_manifest_digest
                            );
                        }
                        ApplyDeploymentRequest {
                            idempotency_key: idempotency_key.clone().unwrap_or_else(|| {
                                super::deployment_dir::default_idempotency_key(
                                    prepared.base_revision,
                                    &plan.reviewed_manifest_digest,
                                )
                            }),
                            base_revision: prepared.base_revision,
                            manifest: prepared.manifest,
                            reason: reason.clone(),
                            reviewed_manifest_digest: Some(plan.reviewed_manifest_digest),
                        }
                    }
                    None => ApplyDeploymentRequest {
                        idempotency_key: idempotency_key.clone().unwrap_or_default(),
                        base_revision: base_revision.unwrap_or(0),
                        manifest: load_file_or_stdin(file.as_deref().unwrap_or("-"))?,
                        reason: reason.clone(),
                        reviewed_manifest_digest: reviewed_digest.clone(),
                    },
                };
                let resp: ApplyDeploymentResponse =
                    client.post_dto(paths::V2_DEPLOYMENTS, &body).await?;
                let committed_rev = if resp.committed_revision > 0 {
                    resp.committed_revision
                } else {
                    resp.receipt.committed_revision
                };
                if quiet && *wait_ms == 0 {
                    println!("{committed_rev}");
                    return Ok(0);
                }
                if !quiet {
                    let val = serde_json::to_value(&resp)?;
                    output::print_value(format, &val)?;
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
                let resp: DeploymentRevisionResponse =
                    client.get(&paths::v2_deployment(*revision)).await?;
                if quiet {
                    if !resp.manifest_digest.is_empty() {
                        println!("{}", resp.manifest_digest);
                    }
                    return Ok(0);
                }
                let val = serde_json::to_value(&resp)?;
                output::print_value(format, &val)?;
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
                let body = RollbackDeploymentRequest {
                    idempotency_key: idempotency_key.clone(),
                    expected_head_revision: *expected_head_revision,
                    target_revision: *target_revision,
                    reason: reason.clone().unwrap_or_else(|| "rollback".to_string()),
                };
                let resp: RollbackDeploymentResponse = client
                    .post_dto(paths::V2_DEPLOYMENTS_ROLLBACK, &body)
                    .await?;
                let committed_rev = if resp.committed_revision > 0 {
                    resp.committed_revision
                } else {
                    resp.receipt.committed_revision
                };
                if quiet {
                    if committed_rev > 0 {
                        println!("{committed_rev}");
                    }
                    return Ok(0);
                }
                let val = serde_json::to_value(&resp)?;
                output::print_value(format, &val)?;
                Ok(0)
            }
            DeploymentsSubcommand::MigrationReport { file } => {
                let payload = load_file_or_stdin(file)?;
                let req: MigrationReportRequest = serde_json::from_value(payload)
                    .with_context(|| format!("invalid migration report input in '{file}'"))?;
                let resp: MigrationReportResponse =
                    client.post_dto(paths::V2_MIGRATION_REPORT, &req).await?;
                let can_cutover = resp.can_cutover || resp.compatible;
                let val = serde_json::to_value(&resp)?;
                output::print_value(format, &val)?;
                Ok(if can_cutover { 0 } else { 2 })
            }
            DeploymentsSubcommand::EditionPreflight { revision } => {
                let body = EditionTransitionPreflightRequest {
                    revision: *revision,
                };
                let resp: EditionTransitionPreflightResponse = client
                    .post_dto(paths::V2_EDITION_TRANSITION_PREFLIGHT, &body)
                    .await?;
                let allowed = resp.transition_allowed;
                let val = serde_json::to_value(&resp)?;
                output::print_value(format, &val)?;
                Ok(if allowed { 0 } else { 2 })
            }
        }
    }
}

/// Store a deployment directory on the target and plan it against the
/// head, printing what would change. `None` when the head deploys exactly
/// this already; otherwise what was prepared and the plan.
///
/// The summary goes to stderr, so stdout carries only the plan (or, for
/// `apply`, the receipt that follows) in the chosen format.
#[allow(clippy::too_many_arguments)]
async fn plan_directory(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    dir: &str,
    expected_head: Option<u64>,
    reason: &Option<String>,
    for_apply: bool,
) -> Result<Option<(super::deployment_dir::Prepared, DeploymentPlanResponse)>> {
    let directory = super::deployment_dir::DeploymentDir::read(std::path::Path::new(dir))?;
    let prepared = super::deployment_dir::prepare(client, &directory, expected_head).await?;
    let plan: DeploymentPlanResponse = client
        .post_dto(
            paths::V2_DEPLOYMENT_PLANS,
            &CreateDeploymentPlanRequest {
                base_revision: prepared.base_revision,
                manifest: prepared.manifest.clone(),
                reason: reason.clone(),
            },
        )
        .await?;

    let up_to_date = prepared.base_revision > 0
        && prepared.head_manifest_digest.as_deref() == Some(plan.reviewed_manifest_digest.as_str());
    if quiet {
        // Plan prints what to review; apply prints the revision it ends at,
        // here only when there is nothing to apply.
        if up_to_date {
            println!("{}", prepared.base_revision);
        } else if !for_apply {
            println!("{}", plan.reviewed_manifest_digest);
        }
    } else {
        eprintln!(
            "{} {dir} against head revision {}",
            "Plan:".bold(),
            prepared.base_revision
        );
        for member in &prepared.assigned {
            let mark = if member.unchanged {
                "=".dimmed()
            } else {
                "+".green()
            };
            eprintln!("  {mark} {} {}@{}", member.kind, member.id, member.revision);
        }
        for removed in &plan.removed_entities {
            eprintln!(
                "  {} {} {}@{}",
                "-".red(),
                removed["kind"].as_str().unwrap_or_default(),
                removed["id"].as_str().unwrap_or_default(),
                removed["revision"]
            );
        }
        let changed = prepared.assigned.iter().filter(|m| !m.unchanged).count();
        eprintln!(
            "  {changed} to deploy, {} unchanged, {} removed; reviewed manifest digest {}",
            prepared.assigned.len() - changed,
            plan.removed_entities.len(),
            plan.reviewed_manifest_digest
        );
        if up_to_date {
            eprintln!(
                "{} the head (revision {}) deploys this already",
                "Up to date:".green().bold(),
                prepared.base_revision
            );
        }
        if !for_apply {
            output::print_value(format, &serde_json::to_value(&plan)?)?;
        }
    }
    Ok((!up_to_date).then_some((prepared, plan)))
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
        let resp: ConvergenceStatusResponse = client.get(&path).await?;
        if resp.converged {
            if quiet {
                println!("{revision}");
            } else {
                let val = serde_json::to_value(&resp)?;
                output::print_value(format, &val)?;
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
        /// Implementation kind (`rust`, `workflow`, or `wasm`)
        #[arg(long, default_value = "wasm")]
        kind: String,
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
    fn new(command: ProviderSubcommand) -> Self {
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
                let resp: ConnectorPackagesListResponse =
                    client.get(paths::V2_CONNECTOR_PACKAGES).await?;
                if quiet {
                    for item in &resp.packages {
                        let d = if !item.digest.is_empty() {
                            &item.digest
                        } else {
                            &item.package_digest
                        };
                        if !d.is_empty() {
                            println!("{d}");
                        }
                    }
                    return Ok(0);
                }
                if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
                    let val = serde_json::to_value(&resp)?;
                    output::print_value(format, &val)?;
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
                        implementation: p.implementation.clone(),
                        admitted: p.admitted.to_string(),
                        digest: if !p.digest.is_empty() {
                            p.digest.clone()
                        } else {
                            p.package_digest.clone()
                        },
                    })
                    .collect();
                output::print_table(rows);
                Ok(0)
            }
            ProviderSubcommand::Upload { file } => {
                let body = load_file_or_stdin(file)?;
                let req: UploadPackageRequest = serde_json::from_value(body)
                    .with_context(|| format!("invalid package upload payload in '{file}'"))?;
                let resp: UploadPackageResponse =
                    client.post_dto(paths::V2_CONNECTOR_PACKAGES, &req).await?;
                if quiet {
                    let d = if !resp.digest.is_empty() {
                        &resp.digest
                    } else if !resp.package.digest.is_empty() {
                        &resp.package.digest
                    } else {
                        &resp.package.package_digest
                    };
                    if !d.is_empty() {
                        println!("{d}");
                    }
                    return Ok(0);
                }
                let val = serde_json::to_value(&resp)?;
                output::print_value(format, &val)?;
                Ok(0)
            }
            ProviderSubcommand::Admit { digest } => {
                let resp: AdmitPackageResponse = client
                    .post_empty(&paths::v2_connector_package_admit(digest))
                    .await?;
                if quiet {
                    println!("{digest}");
                    return Ok(0);
                }
                let val = serde_json::to_value(&resp)?;
                output::print_value(format, &val)?;
                Ok(0)
            }
            ProviderSubcommand::Revoke { digest, reason } => {
                let body = RevokePackageRequest {
                    reason: reason.clone(),
                };
                let resp: RevokePackageResponse = client
                    .post_dto(&paths::v2_connector_package_revoke(digest), &body)
                    .await?;
                if quiet {
                    println!("{digest}");
                    return Ok(0);
                }
                let val = serde_json::to_value(&resp)?;
                output::print_value(format, &val)?;
                Ok(0)
            }
            ProviderSubcommand::Scaffold {
                publisher,
                provider_id,
                version,
                kind,
            } => {
                let body = ScaffoldRequest {
                    publisher: publisher.clone(),
                    provider_id: provider_id.clone(),
                    version: version.clone(),
                    kind: kind.clone(),
                };
                let resp: ScaffoldResponse =
                    client.post_dto(paths::V2_AUTHORING_SCAFFOLD, &body).await?;
                let val = serde_json::to_value(&resp)?;
                output::print_value(format, &val)?;
                Ok(0)
            }
            ProviderSubcommand::Lint { file } => {
                let body = load_file_or_stdin(file)
                    .with_context(|| format!("failed to read provider bundle '{file}'"))?;
                let req: UploadPackageRequest = serde_json::from_value(body)
                    .with_context(|| format!("invalid provider bundle in '{file}'"))?;
                let resp: LintResponse = client.post_dto(paths::V2_AUTHORING_LINT, &req).await?;
                let valid = resp.valid;
                let val = serde_json::to_value(&resp)?;
                output::print_value(format, &val)?;
                Ok(if valid { 0 } else { 2 })
            }
            ProviderSubcommand::Pack { file } => {
                let body = load_file_or_stdin(file)?;
                let req: UploadPackageRequest = serde_json::from_value(body)
                    .with_context(|| format!("invalid provider bundle in '{file}'"))?;
                let resp: PackResponse = client.post_dto(paths::V2_AUTHORING_PACK, &req).await?;
                if quiet {
                    if !resp.package_digest.is_empty() {
                        println!("{}", resp.package_digest);
                    }
                    return Ok(0);
                }
                let val = serde_json::to_value(&resp)?;
                output::print_value(format, &val)?;
                Ok(0)
            }
        }
    }
}

// ============================================================================
// 3. `orion-cli connections` (V2-07.3)
// ============================================================================

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
    #[cfg(test)]
    fn new(command: ConnectionsSubcommand) -> Self {
        Self { command }
    }

    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
    ) -> Result<i32> {
        match &self.command {
            ConnectionsSubcommand::List => {
                let resp: ConnectionsListResponse = client.get(paths::V2_CONNECTIONS).await?;
                if quiet {
                    for item in &resp.connections {
                        if !item.connection_id.is_empty() {
                            println!("{}", item.connection_id);
                        }
                    }
                    return Ok(0);
                }
                let val = serde_json::to_value(&resp)?;
                output::print_value(format, &val)?;
                Ok(0)
            }
            ConnectionsSubcommand::Create { file } => {
                let body = load_file_or_stdin(file)?;
                let req: CreateConnectionRevisionRequest = serde_json::from_value(body)
                    .with_context(|| format!("invalid connection payload in '{file}'"))?;
                let resp: CreateConnectionRevisionResponse =
                    client.post_dto(paths::V2_CONNECTIONS, &req).await?;
                if quiet {
                    println!("{}", resp.connection.revision);
                    return Ok(0);
                }
                let val = serde_json::to_value(&resp)?;
                output::print_value(format, &val)?;
                Ok(0)
            }
            ConnectionsSubcommand::Probe { id, revision } => {
                let body = ProbeConnectionRequest {
                    revision: *revision,
                    driver_id: None,
                    kind: None,
                };
                let resp: ConnectionProbeResponse = client
                    .post_dto(&paths::v2_connection_probe(id), &body)
                    .await?;
                if quiet {
                    let s = if !resp.status.is_empty() {
                        resp.status.as_str()
                    } else if resp.healthy {
                        "healthy"
                    } else {
                        "unhealthy"
                    };
                    println!("{s}");
                    return Ok(0);
                }
                let val = serde_json::to_value(&resp)?;
                output::print_value(format, &val)?;
                Ok(0)
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn handle_v2_request(method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let body_val = body.cloned().unwrap_or_else(|| json!({}));
        match (method, path) {
            ("GET", "/api/v2/admin/runtime-capabilities") => Ok(json!({
                "api_version": "v2",
                "edition": "community",
                "community_boundary_compliant": true,
            })),
            ("POST", "/api/v2/admin/authoring/scaffold") => {
                let req: ScaffoldRequest = serde_json::from_value(body_val)?;
                assert_eq!(req.kind, "rust");
                Ok(json!({
                    "publisher": req.publisher,
                    "provider_id": req.provider_id,
                    "version": req.version,
                    "implementation": "rust",
                    "package_digest": "sha256:1111",
                    "files": {"manifest.json": "{}"},
                }))
            }
            ("POST", "/api/v2/admin/authoring/lint") => Ok(json!({"valid": true})),
            ("POST", "/api/v2/admin/authoring/pack") => Ok(json!({
                "package_digest": "sha256:1111",
                "inventory": {},
                "files": {"manifest.json": "{}", "inventory.json": "{}"},
            })),
            ("POST", p)
                if p.starts_with("/api/v2/admin/connector-packages/") && p.ends_with("/admit") =>
            {
                Ok(json!({
                    "status": "admitted",
                    "admission": {
                        "package_digest": "sha256:1111",
                        "policy_id": "community",
                        "admission_fingerprint": "fp1",
                        "revoked": false,
                    }
                }))
            }
            ("POST", "/api/v2/admin/connector-packages") => Ok(json!({
                "status": "uploaded",
                "digest": "sha256:1111",
                "package": {
                    "publisher": "org.plasmatic.foundation",
                    "provider_id": "memory-cache",
                    "version": "2.0.0",
                    "digest": "sha256:1111",
                    "package_digest": "sha256:1111",
                    "implementation": "rust",
                    "admitted": false,
                }
            })),
            ("POST", p) if p.starts_with("/api/v2/admin/connections/") && p.ends_with("/probe") => {
                Ok(json!({
                    "connection_id": "cache-1",
                    "revision": 1,
                    "driver_id": "memory-cache",
                    "healthy": true,
                    "status": "healthy",
                    "latency_ms": 1,
                }))
            }
            ("POST", "/api/v2/admin/connections") => Ok(json!({
                "status": "created",
                "connection": {
                    "connection_id": "cache-1",
                    "revision": 1,
                    "provider_digest": "sha256:1111",
                    "configuration_digest": "sha256:2222",
                }
            })),
            ("POST", "/api/v2/admin/deployment-plans") => Ok(json!({
                "base_revision": 0,
                "candidate_digest": "sha256:3333",
                "reviewed_manifest_digest": "sha256:3333",
            })),
            ("POST", "/api/v2/admin/deployments/rollback") => {
                let req: RollbackDeploymentRequest = serde_json::from_value(body_val)?;
                assert_eq!(req.target_revision, 1);
                assert_eq!(req.expected_head_revision, 1);
                assert!(!req.reason.is_empty());
                Ok(json!({
                    "status": "accepted",
                    "committed_revision": 2,
                    "receipt": {
                        "committed_revision": 2,
                        "manifest_digest": "sha256:3333",
                    }
                }))
            }
            ("POST", "/api/v2/admin/deployments") => {
                let req: ApplyDeploymentRequest = serde_json::from_value(body_val)?;
                assert!(!req.reason.is_empty(), "apply must send required reason");
                assert_eq!(req.reviewed_manifest_digest.as_deref(), Some("sha256:3333"));
                Ok(json!({
                    "status": "accepted",
                    "committed_revision": 1,
                    "receipt": {
                        "committed_revision": 1,
                        "manifest_digest": "sha256:3333",
                    }
                }))
            }
            ("GET", p) if p.starts_with("/api/v2/admin/deployments/1/convergence") => Ok(json!({
                "converged": true,
                "target_revision": 1,
                "converged_nodes": ["node-1"],
            })),
            ("GET", "/api/v2/admin/deployments/1") => Ok(json!({
                "revision": 1,
                "manifest_digest": "sha256:3333",
                "reason": "cli apply",
            })),
            _ => anyhow::bail!("unexpected {method} {path}"),
        }
    }

    #[tokio::test]
    async fn cli_v2_commands_exercise_full_flow_against_http_server() {
        let client = OrionClient::with_mock_transport(handle_v2_request).unwrap();
        let fmt = OutputFormat::Json;

        // 1. Capabilities
        let code = DeploymentsCmd::new(DeploymentsSubcommand::Capabilities)
            .run(&client, &fmt, true)
            .await
            .unwrap();
        assert_eq!(code, 0);

        // 2. Scaffold
        let code = ProviderCmd::new(ProviderSubcommand::Scaffold {
            publisher: "org.plasmatic.foundation".to_string(),
            provider_id: "memory-cache".to_string(),
            version: "2.0.0".to_string(),
            kind: "rust".to_string(),
        })
        .run(&client, &fmt, true)
        .await
        .unwrap();
        assert_eq!(code, 0);

        // Write temp files for lint, pack, upload, connection create, plan, apply
        let dir = tempfile::tempdir().unwrap();
        let pkg_file = dir.path().join("pkg.json");
        std::fs::write(&pkg_file, r#"{"files":{"manifest.json":"{}"}}"#).unwrap();

        let code = ProviderCmd::new(ProviderSubcommand::Lint {
            file: pkg_file.to_string_lossy().to_string(),
        })
        .run(&client, &fmt, true)
        .await
        .unwrap();
        assert_eq!(code, 0);

        let code = ProviderCmd::new(ProviderSubcommand::Pack {
            file: pkg_file.to_string_lossy().to_string(),
        })
        .run(&client, &fmt, true)
        .await
        .unwrap();
        assert_eq!(code, 0);

        let code = ProviderCmd::new(ProviderSubcommand::Upload {
            file: pkg_file.to_string_lossy().to_string(),
        })
        .run(&client, &fmt, true)
        .await
        .unwrap();
        assert_eq!(code, 0);

        let code = ProviderCmd::new(ProviderSubcommand::Admit {
            digest: "sha256:1111".to_string(),
        })
        .run(&client, &fmt, true)
        .await
        .unwrap();
        assert_eq!(code, 0);

        let conn_file = dir.path().join("conn.json");
        std::fs::write(
            &conn_file,
            r#"{"connection_id":"cache-1","revision":1,"provider_digest":"sha256:1111","public_config":{}}"#,
        )
        .unwrap();
        let code = ConnectionsCmd::new(ConnectionsSubcommand::Create {
            file: conn_file.to_string_lossy().to_string(),
        })
        .run(&client, &fmt, true)
        .await
        .unwrap();
        assert_eq!(code, 0);

        let code = ConnectionsCmd::new(ConnectionsSubcommand::Probe {
            id: "cache-1".to_string(),
            revision: Some(1),
        })
        .run(&client, &fmt, true)
        .await
        .unwrap();
        assert_eq!(code, 0);

        let manifest_file = dir.path().join("manifest.json");
        std::fs::write(
            &manifest_file,
            r#"{"schema_version":"2.0","edition":"community","providers":[],"connections":[],"members":[]}"#,
        )
        .unwrap();

        let code = DeploymentsCmd::new(DeploymentsSubcommand::Plan {
            file: Some(manifest_file.to_string_lossy().to_string()),
            dir: None,
            base_revision: Some(0),
            reason: Some("plan".to_string()),
        })
        .run(&client, &fmt, true)
        .await
        .unwrap();
        assert_eq!(code, 0);

        let code = DeploymentsCmd::new(DeploymentsSubcommand::Apply {
            file: Some(manifest_file.to_string_lossy().to_string()),
            dir: None,
            base_revision: Some(0),
            reviewed_digest: Some("sha256:3333".to_string()),
            idempotency_key: Some("idem-1".to_string()),
            reason: "cli apply".to_string(),
            wait_ms: 1000,
            wait_target: "this-node".to_string(),
        })
        .run(&client, &fmt, true)
        .await
        .unwrap();
        assert_eq!(code, 0);

        let code = DeploymentsCmd::new(DeploymentsSubcommand::Get { revision: 1 })
            .run(&client, &fmt, true)
            .await
            .unwrap();
        assert_eq!(code, 0);

        let code = DeploymentsCmd::new(DeploymentsSubcommand::Rollback {
            target_revision: 1,
            expected_head_revision: 1,
            idempotency_key: "idem-rb".to_string(),
            reason: Some("rollback".to_string()),
        })
        .run(&client, &fmt, true)
        .await
        .unwrap();
        assert_eq!(code, 0);
    }

    /// `apply -d`: every definition uploaded and checked against its file
    /// name, a connection revision created past the stored ones, members
    /// given revisions against the head, and the plan's digest applied.
    #[tokio::test]
    async fn apply_dir_assigns_revisions_against_the_head() {
        use std::sync::{Arc, Mutex};
        let dir = tempfile::tempdir().unwrap();
        let wf_hex = "a".repeat(64);
        let ch_hex = "b".repeat(64);
        std::fs::create_dir_all(dir.path().join("definitions")).unwrap();
        std::fs::create_dir_all(dir.path().join("connections")).unwrap();
        std::fs::write(
            dir.path()
                .join("definitions")
                .join(format!("{wf_hex}.json")),
            r#"{"kind":"workflow","definition":{"name":"w","tasks":[]}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path()
                .join("definitions")
                .join(format!("{ch_hex}.json")),
            r#"{"kind":"channel","definition":{"name":"c","protocol":"rest"}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("connections/db.json"),
            r#"{"connection_id":"db","driver":"org.orion.sqlite","provider_digest":"sha256:p",
                "public_config":{"connection_string":"sqlite::memory:"},"secret_locators":{}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("deployment.json"),
            format!(
                r#"{{"schema_version":1,"members":[
                {{"kind":"channel","id":"c","definition":"sha256:{ch_hex}",
                  "requires":[{{"kind":"workflow","id":"w"}}]}},
                {{"kind":"connection","id":"db","connection":"db","requires":[]}},
                {{"kind":"workflow","id":"w","definition":"sha256:{wf_hex}",
                  "requires":[{{"kind":"connection","id":"db"}}]}}]}}"#
            ),
        )
        .unwrap();

        let applied: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
        let seen = applied.clone();
        let client = OrionClient::with_mock_transport(move |method, path, body| {
            let body = body.cloned().unwrap_or(Value::Null);
            match (method, path) {
                // Head 4 deploys the channel as it is, the workflow at an
                // older definition.
                ("GET", "/api/v2/admin/deployments") => Ok(json!({
                    "head_revision": 4,
                    "revisions": [{
                        "revision": 4,
                        "manifest_digest": "sha256:head",
                        "manifest": {"members": [
                            {"entity": {"kind": "channel", "id": "c", "revision": 2},
                             "definition_digest": format!("sha256:{}", "b".repeat(64))},
                            {"entity": {"kind": "workflow", "id": "w", "revision": 5},
                             "definition_digest": "sha256:old"},
                        ]},
                    }],
                })),
                ("POST", "/api/v2/admin/definitions") => {
                    let hex = if body["kind"] == "workflow" { "a" } else { "b" };
                    Ok(json!({"digest": format!("sha256:{}", hex.repeat(64)),
                              "kind": body["kind"], "size_bytes": 1}))
                }
                ("GET", "/api/v2/admin/runtime-capabilities") => Ok(json!({
                    "provider_backends": [
                        {"driver_id": "org.orion.sqlite", "package_digest": "sha256:p"}],
                })),
                ("GET", "/api/v2/admin/connections") => Ok(json!({"connections": [
                    {"connection_id": "db", "revision": 2, "provider_digest": "sha256:p",
                     "public_config": {"connection_string": "sqlite:/other.db"}},
                ]})),
                ("POST", "/api/v2/admin/connections") => {
                    assert_eq!(body["revision"], 3, "one past the stored revisions");
                    Ok(json!({"connection": {"connection_id": "db", "revision": 3,
                              "configuration_digest": "sha256:cfg"}}))
                }
                ("POST", "/api/v2/admin/deployment-plans") => {
                    assert_eq!(body["base_revision"], 4);
                    Ok(json!({"reviewed_manifest_digest": "sha256:next",
                              "candidate_digest": "sha256:next"}))
                }
                ("POST", "/api/v2/admin/deployments") => {
                    *seen.lock().unwrap() = Some(body);
                    Ok(json!({"status": "accepted", "committed_revision": 5}))
                }
                _ => anyhow::bail!("unexpected {method} {path}"),
            }
        })
        .unwrap();

        let code = DeploymentsCmd::new(DeploymentsSubcommand::Apply {
            file: None,
            dir: Some(dir.path().to_string_lossy().to_string()),
            base_revision: None,
            reviewed_digest: None,
            idempotency_key: None,
            reason: "cli apply".to_string(),
            wait_ms: 0,
            wait_target: "this-node".to_string(),
        })
        .run(&client, &OutputFormat::Json, true)
        .await
        .unwrap();
        assert_eq!(code, 0);

        let request = applied.lock().unwrap().clone().expect("applied");
        assert_eq!(request["base_revision"], 4);
        assert_eq!(request["reviewed_manifest_digest"], "sha256:next");
        assert_eq!(request["idempotency_key"], "deploy-4-next");
        let members = request["manifest"]["members"].as_array().unwrap();
        let revision_of = |kind: &str| {
            members
                .iter()
                .find(|m| m["entity"]["kind"] == kind)
                .map(|m| m["entity"]["revision"].clone())
                .unwrap()
        };
        assert_eq!(revision_of("channel"), 2, "unchanged keeps its revision");
        assert_eq!(revision_of("workflow"), 6, "changed takes the next");
        assert_eq!(revision_of("connection"), 3);
        let workflow = members
            .iter()
            .find(|m| m["entity"]["kind"] == "workflow")
            .unwrap();
        assert_eq!(
            workflow["requires"],
            json!([{"kind": "connection", "id": "db", "revision": 3}])
        );
        let channel = members
            .iter()
            .find(|m| m["entity"]["kind"] == "channel")
            .unwrap();
        assert_eq!(
            channel["requires"],
            json!([{"kind": "workflow", "id": "w", "revision": 6}])
        );
    }
}
