//! `orion-cli deployments`: `capabilities`, `plan`, `apply`, `list`, `get`,
//! `wait`, `rollback`, `edition-preflight`. Every call goes through
//! `orion_client::V2Admin`: the request and response types are
//! `orion_api::v2`'s, and no command builds a body or a path.

use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use orion_api::v2::{
    ApplyDeploymentRequest, ConvergenceStatusResponse, CreateDeploymentPlanRequest,
    CreateDeploymentRequest, DeploymentPlanResponse, EditionTransitionPreflightRequest,
    RollbackDeploymentRequest,
};
use orion_client::V2Admin;
use tabled::Tabled;

use super::v2::load_request;
use crate::client::OrionClient;
use crate::output::{self, OutputFormat};

#[derive(Args)]
#[command(
    long_about = "Manage immutable v2 deployments, candidate plans, CAS apply, convergence polling, rollback, and edition preflight."
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
        /// A directory `orion-cli compile --format deployment` wrote. Its
        /// definitions and connection revisions are stored on the target and
        /// its members assigned revisions against the target's head, which is
        /// the base revision
        #[arg(short, long, value_name = "DIR")]
        dir: Option<String>,
        /// Expected current head revision (0 for initial deployment). With
        /// -d, the plan is refused unless the head is this revision
        #[arg(long)]
        base_revision: Option<u64>,
    },
    /// Atomically apply a reviewed candidate deployment under CAS + idempotency:
    /// a manifest file (-f), or a compiled deployment directory (-d)
    Apply {
        /// Path to candidate manifest JSON file (or '-' for stdin)
        #[arg(short, long, required_unless_present = "dir", conflicts_with = "dir")]
        file: Option<String>,
        /// A directory `orion-cli compile --format deployment` wrote:
        /// stored, planned against the target's head and applied with the
        /// plan's reviewed digest. A directory the head already deploys is
        /// not applied again
        #[arg(short, long, value_name = "DIR")]
        dir: Option<String>,
        /// Expected current head revision. Required with -f; with -d, the
        /// apply is refused unless the head is this revision
        #[arg(long, required_unless_present = "dir")]
        base_revision: Option<u64>,
        /// The reviewed manifest digest `deployments plan` printed. Required
        /// with -f; with -d the plan's own is sent, and this, when given, must
        /// equal it
        #[arg(long, required_unless_present = "dir")]
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
    /// List committed deployment revisions, newest first, without their manifests
    List {
        /// Revisions per page (the server clamps it to 1000)
        #[arg(long, default_value_t = 50)]
        limit: u64,
        /// The `next_cursor` a previous page printed
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Fetch a committed deployment revision, with its manifest
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
    /// Run Enterprise-to-Community edition transition preflight check
    EditionPreflight {
        /// Optional revision to inspect (defaults to active head revision)
        #[arg(long)]
        revision: Option<u64>,
    },
}

#[derive(Tabled)]
struct DeploymentRow {
    #[tabled(rename = "Revision")]
    revision: u64,
    #[tabled(rename = "Members")]
    members: String,
    #[tabled(rename = "Reason")]
    reason: String,
    #[tabled(rename = "By")]
    created_by: String,
    #[tabled(rename = "Created")]
    created_at: String,
    #[tabled(rename = "Digest")]
    digest: String,
}

impl DeploymentsCmd {
    #[cfg(test)]
    pub(super) fn new(command: DeploymentsSubcommand) -> Self {
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
                let resp = client.runtime_capabilities().await?;
                if quiet {
                    println!("{}", resp.edition);
                    return Ok(0);
                }
                output::print_value(format, &serde_json::to_value(&resp)?)?;
                Ok(0)
            }
            DeploymentsSubcommand::Plan {
                file,
                dir,
                base_revision,
            } => {
                if let Some(dir) = dir {
                    return plan_directory(client, format, quiet, dir, *base_revision, false)
                        .await
                        .map(|_| 0);
                }
                let manifest = load_request(file.as_deref().unwrap_or("-"), "deployment manifest")?;
                let resp = client
                    .plan_deployment(&CreateDeploymentPlanRequest {
                        base_revision: base_revision.unwrap_or(0),
                        manifest,
                    })
                    .await?;
                if quiet {
                    println!("{}", resp.reviewed_manifest_digest);
                    return Ok(0);
                }
                output::print_value(format, &serde_json::to_value(&resp)?)?;
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
                        let planned =
                            plan_directory(client, format, quiet, dir, *base_revision, true)
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
                            reviewed_manifest_digest: plan.reviewed_manifest_digest,
                        }
                    }
                    None => ApplyDeploymentRequest {
                        idempotency_key: idempotency_key.clone().unwrap_or_default(),
                        base_revision: base_revision.unwrap_or(0),
                        manifest: load_request(
                            file.as_deref().unwrap_or("-"),
                            "deployment manifest",
                        )?,
                        reason: reason.clone(),
                        reviewed_manifest_digest: reviewed_digest.clone().unwrap_or_default(),
                    },
                };
                let resp = client
                    .create_deployment(&CreateDeploymentRequest::Apply(body))
                    .await?;
                let committed_rev = resp.committed_revision;
                let wait = if *wait_ms > 0 && committed_rev > 0 {
                    Some(poll_convergence(client, committed_rev, wait_target, *wait_ms).await)
                } else {
                    None
                };
                // Even if polling fails, the apply committed: emit its receipt
                // before propagating the error through the normal stderr path.
                if quiet {
                    println!("{committed_rev}");
                } else {
                    let mut value = serde_json::to_value(&resp)?;
                    if let Some(Ok(report)) = &wait {
                        value["convergence"] = serde_json::to_value(report)?;
                    }
                    output::print_value(format, &value)?;
                }
                match wait {
                    Some(result) => Ok(convergence_exit_code(committed_rev, &result?)),
                    None => Ok(0),
                }
            }
            DeploymentsSubcommand::List { limit, cursor } => {
                let page = client
                    .list_deployments(Some(*limit), cursor.as_deref())
                    .await?;
                if quiet {
                    for revision in &page.data {
                        println!("{}", revision.revision);
                    }
                    return Ok(0);
                }
                if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
                    output::print_value(format, &serde_json::to_value(&page)?)?;
                    return Ok(0);
                }
                println!(
                    "Head revision {} (this node serves {})",
                    page.head_revision, page.applied_revision
                );
                if page.data.is_empty() {
                    println!("{}", "No deployment revisions.".dimmed());
                    return Ok(0);
                }
                let rows: Vec<DeploymentRow> = page
                    .data
                    .iter()
                    .map(|r| DeploymentRow {
                        revision: r.revision,
                        members: if r.members_compacted {
                            "compacted".to_string()
                        } else {
                            r.member_count.to_string()
                        },
                        reason: r.reason.clone(),
                        created_by: r.created_by.clone(),
                        created_at: r.created_at.clone(),
                        digest: r.manifest_digest.clone(),
                    })
                    .collect();
                output::print_table(rows);
                if let Some(next) = &page.next_cursor {
                    println!(
                        "{}",
                        format!("More revisions follow: --cursor {next}").dimmed()
                    );
                }
                Ok(0)
            }
            DeploymentsSubcommand::Get { revision } => {
                let resp = client.get_deployment(*revision).await?;
                if quiet {
                    println!("{}", resp.summary.manifest_digest);
                    return Ok(0);
                }
                output::print_value(format, &serde_json::to_value(&resp)?)?;
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
                let resp = client
                    .create_deployment(&CreateDeploymentRequest::Rollback(
                        RollbackDeploymentRequest {
                            idempotency_key: idempotency_key.clone(),
                            base_revision: *expected_head_revision,
                            rollback_to: *target_revision,
                            reason: reason.clone().unwrap_or_else(|| "rollback".to_string()),
                        },
                    ))
                    .await?;
                if quiet {
                    println!("{}", resp.committed_revision);
                    return Ok(0);
                }
                output::print_value(format, &serde_json::to_value(&resp)?)?;
                Ok(0)
            }
            DeploymentsSubcommand::EditionPreflight { revision } => {
                let resp = client
                    .edition_transition_preflight(&EditionTransitionPreflightRequest {
                        revision: *revision,
                    })
                    .await?;
                let allowed = resp.transition_allowed;
                output::print_value(format, &serde_json::to_value(&resp)?)?;
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
async fn plan_directory(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    dir: &str,
    expected_head: Option<u64>,
    for_apply: bool,
) -> Result<Option<(super::deployment_dir::Prepared, DeploymentPlanResponse)>> {
    let directory = super::deployment_dir::DeploymentDir::read(std::path::Path::new(dir))?;
    let prepared = super::deployment_dir::prepare(client, &directory, expected_head).await?;
    let plan = client
        .plan_deployment(&CreateDeploymentPlanRequest {
            base_revision: prepared.base_revision,
            manifest: prepared.manifest.clone(),
        })
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
                removed.kind,
                removed.id,
                removed.revision
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
    let report = poll_convergence(client, revision, target, timeout_ms).await?;
    if quiet {
        println!("{revision}");
    } else {
        output::print_value(format, &serde_json::to_value(&report)?)?;
    }
    Ok(convergence_exit_code(revision, &report))
}

fn convergence_exit_code(revision: u64, report: &ConvergenceStatusResponse) -> i32 {
    if report.converged {
        0
    } else {
        eprintln!(
            "{} Convergence wait timed out for committed revision {revision}",
            "Timeout:".yellow().bold()
        );
        3
    }
}

async fn poll_convergence(
    client: &OrionClient,
    revision: u64,
    target: &str,
    timeout_ms: u64,
) -> Result<ConvergenceStatusResponse> {
    let started = Instant::now();
    let timeout = Duration::from_millis(timeout_ms.max(50));
    loop {
        let resp = client
            .deployment_convergence(revision, Some(target))
            .await?;
        let elapsed = started.elapsed();
        if resp.converged || elapsed >= timeout {
            return Ok(resp);
        }
        tokio::time::sleep(Duration::from_millis(200).min(timeout.saturating_sub(elapsed))).await;
    }
}
