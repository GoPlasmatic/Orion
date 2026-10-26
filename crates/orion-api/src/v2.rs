//! Orion v2 Admin API wire contract.
//!
//! Versioned request and response DTOs shared by `orion-server` and `orion-cli` for `/api/v2/admin/*` endpoints:
//! - Runtime capabilities & edition discovery (`/runtime-capabilities`)
//! - Connector packages, their admission and revocations (`/connector-packages`)
//! - Connection revisions and their probes (`/connections/{id}/revisions`)
//! - Candidate deployment plans (`/deployment-plans`), and deployments — an
//!   apply or a rollback, both under CAS — with their convergence (`/deployments`)
//! - Enterprise-to-Community edition transition preflight (`/edition-transition-preflight`)
//! - Provider package authoring scaffold, lint & pack (`/authoring/*`)

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Runtime capabilities: edition, registered drivers and providers, deployment head (`GET /api/v2/admin/runtime-capabilities`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct RuntimeCapabilitiesResponse {
    #[serde(default)]
    pub api_version: String,
    #[serde(default)]
    pub edition: String,
    #[serde(default)]
    pub admission_policy_id: String,
    #[serde(default, alias = "channel_drivers")]
    pub registered_channel_drivers: Vec<String>,
    #[serde(default)]
    pub schema_profiles: Vec<String>,
    #[serde(default)]
    pub wasm_worlds: Vec<String>,
    #[serde(default)]
    pub provider_backends: Vec<Value>,
    #[serde(default)]
    pub registered_rust_providers: Vec<String>,
    #[serde(default)]
    pub registered_workflow_providers: Vec<String>,
    #[serde(default)]
    pub deployment_head: Value,
    #[serde(default)]
    pub reconciler: Value,
    #[serde(default)]
    pub native_scheduler: Value,
    #[serde(default)]
    pub active_deployment_revision: u64,
    #[serde(default)]
    pub node_status: Value,
}

/// A single file entry inside a provider package archive.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ArchiveEntryDto {
    pub path: String,
    pub content: String,
}

/// Summary of an uploaded/admitted connector provider package (`GET /api/v2/admin/connector-packages`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ConnectorPackageSummary {
    #[serde(default)]
    pub publisher: String,
    #[serde(default)]
    pub provider_id: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub digest: String,
    #[serde(default)]
    pub package_digest: String,
    #[serde(default)]
    pub implementation: String,
    #[serde(default)]
    pub tier: String,
    #[serde(default)]
    pub admitted: bool,
    #[serde(default)]
    pub revoked: bool,
    #[serde(default)]
    pub artifact_state: String,
    #[serde(default, alias = "manifest_json")]
    pub manifest: Value,
    #[serde(default, alias = "distribution_json")]
    pub distribution: Value,
    #[serde(default, alias = "inventory_json")]
    pub inventory: Value,
}

/// Provider package admission record (`PUT /api/v2/admin/connector-packages/{digest}/admission`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ProviderAdmissionDto {
    #[serde(default)]
    pub package_digest: String,
    #[serde(default)]
    pub policy_id: String,
    #[serde(default)]
    pub admission_fingerprint: String,
    #[serde(default)]
    pub revoked: bool,
}

/// Response listing connector provider packages (`GET /api/v2/admin/connector-packages`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ConnectorPackagesListResponse {
    #[serde(default)]
    pub count: usize,
    #[serde(default, alias = "data")]
    pub packages: Vec<ConnectorPackageSummary>,
    #[serde(default)]
    pub admissions: Vec<ProviderAdmissionDto>,
}

/// One connector provider package and its admission record
/// (`GET /api/v2/admin/connector-packages/{digest}`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ConnectorPackageResponse {
    #[serde(default)]
    pub package: ConnectorPackageSummary,
    /// The admission recorded for the package; absent while it was never
    /// admitted.
    #[serde(default)]
    pub admission: Option<ProviderAdmissionDto>,
}

/// Request body for uploading, linting, or packing a provider package (`POST /api/v2/admin/connector-packages`, `/authoring/lint`, `/authoring/pack`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct UploadPackageRequest {
    /// Map of relative path -> UTF-8 or `base64:<...>` file content.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// Optional list of `{path, content}` entries (converted into `files` if `files` is empty).
    #[serde(default)]
    pub entries: Vec<ArchiveEntryDto>,
    #[serde(default)]
    pub admit: bool,
    #[serde(default)]
    pub lock: Option<Value>,
}

impl UploadPackageRequest {
    pub fn merged_files(&self) -> BTreeMap<String, String> {
        let mut map = self.files.clone();
        for entry in &self.entries {
            map.entry(entry.path.clone())
                .or_insert_with(|| entry.content.clone());
        }
        map
    }
}

/// Response from uploading a verified provider package (`POST /api/v2/admin/connector-packages`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct UploadPackageResponse {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub digest: String,
    #[serde(default)]
    pub package: ConnectorPackageSummary,
    #[serde(default)]
    pub admission: Option<ProviderAdmissionDto>,
}

/// Request body for admitting an uploaded provider package (`PUT /api/v2/admin/connector-packages/{digest}/admission`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct AdmitPackageRequest {
    #[serde(default)]
    pub lock: Option<Value>,
}

/// Response from admitting a provider package (`PUT /api/v2/admin/connector-packages/{digest}/admission`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct AdmitPackageResponse {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub admission: ProviderAdmissionDto,
}

/// Request body for revoking an admitted provider package (`POST /api/v2/admin/connector-packages/{digest}/revocations`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct RevokePackageRequest {
    #[serde(default)]
    pub reason: Option<String>,
}

/// Response from revoking a provider package (`POST /api/v2/admin/connector-packages/{digest}/revocations`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct RevokePackageResponse {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub package_digest: String,
    #[serde(default)]
    pub revoked: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

/// An immutable connection revision (`GET /api/v2/admin/connections/{id}/revisions/{revision}`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ConnectionRevisionResponse {
    #[serde(default)]
    pub connection_id: String,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub provider_digest: String,
    #[serde(default)]
    pub provider_ref: Value,
    #[serde(default)]
    pub config: Value,
    #[serde(default)]
    pub public_config: Value,
    /// Config path to secret reference (`env://`, `vault://`, …).
    #[serde(default)]
    pub secret_locators: BTreeMap<String, String>,
    /// Over a `workflow` provider package: the connection each of the
    /// package's dependency aliases calls (`alias -> connection_id`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bindings: BTreeMap<String, String>,
    #[serde(default)]
    pub config_fingerprint: String,
    #[serde(default)]
    pub configuration_digest: String,
}

/// Response listing connection revisions (`GET /api/v2/admin/connections`, `GET /api/v2/admin/connections/{id}/revisions`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ConnectionsListResponse {
    #[serde(default)]
    pub count: usize,
    #[serde(default, alias = "data")]
    pub connections: Vec<ConnectionRevisionResponse>,
}

/// Request body to create an immutable connection revision
/// (`POST /api/v2/admin/connections/{id}/revisions`). The connection is the
/// one the path names.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct CreateConnectionRevisionRequest {
    /// The revision to create: one past the highest stored, for a change.
    /// Creating a revision that is stored with the same content answers it
    /// again; with different content, `IMMUTABLE_CONNECTION_CONFLICT`.
    pub revision: u64,
    pub provider_digest: String,
    /// The connection's settings. Never a secret: a credential-shaped key or
    /// value is refused.
    #[serde(default)]
    pub public_config: Value,
    /// Where each credential comes from: a config path (`connection_string`,
    /// `auth.token`, `auth.client_secret`, …) to a secret reference such as
    /// `env://ORDERS_DB_URL`. Placed at that path when the connection is used.
    #[serde(default)]
    pub secret_locators: BTreeMap<String, String>,
    /// Over a `workflow` provider package: which connection each of the
    /// package's declared dependency aliases calls (`alias -> connection_id`).
    /// Every alias must be bound, and each target must be a stored
    /// connection; a deployment member for this connection must require each
    /// bound connection. Empty for any other provider.
    #[serde(default)]
    pub bindings: BTreeMap<String, String>,
}

/// Response from creating an immutable connection revision (`POST /api/v2/admin/connections/{id}/revisions`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct CreateConnectionRevisionResponse {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub connection: ConnectionRevisionResponse,
}

/// One health probe of a connection revision
/// (`POST /api/v2/admin/connections/{id}/revisions/{revision}/probes`). A probe
/// that ran answers `200` whatever it found: `healthy` is the finding.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ConnectionProbeResponse {
    #[serde(default)]
    pub connection_id: String,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub driver_id: String,
    #[serde(default)]
    pub healthy: bool,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub latency_ms: u64,
    #[serde(default, alias = "error")]
    pub details: Value,
}

/// Request body for planning a candidate deployment (`POST /api/v2/admin/deployment-plans`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct CreateDeploymentPlanRequest {
    /// The head the candidate is planned on: `0` before the first apply.
    pub base_revision: u64,
    #[serde(alias = "candidate_manifest")]
    pub manifest: Value,
}

/// A deployment member by identity, as a plan names it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DeploymentEntityRef {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub revision: u64,
}

/// A member the base deploys at one revision and the candidate at another.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DeploymentEntityUpdate {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub from_revision: u64,
    #[serde(default)]
    pub to_revision: u64,
}

/// A candidate deployment, checked against its base
/// (`POST /api/v2/admin/deployment-plans`). Nothing is stored: the plan is a
/// function of the request and the store, and `reviewed_manifest_digest` is
/// what an apply must name to commit exactly this candidate.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DeploymentPlanResponse {
    #[serde(default)]
    pub base_revision: u64,
    /// The candidate's canonical manifest digest. An apply names it as its
    /// `reviewed_manifest_digest`.
    #[serde(default)]
    pub reviewed_manifest_digest: String,
    /// Members the base does not deploy at all.
    #[serde(default)]
    pub added_entities: Vec<DeploymentEntityRef>,
    /// Members the base deploys at another revision.
    #[serde(default)]
    pub updated_entities: Vec<DeploymentEntityUpdate>,
    /// Members the base deploys and the candidate drops: they stop serving.
    #[serde(default)]
    pub removed_entities: Vec<DeploymentEntityRef>,
}

/// Request body for `POST /api/v2/admin/deployments`: commit the next
/// revision on top of `base_revision`, under compare-and-swap and an
/// idempotency key. Either a reviewed manifest (an apply) or an earlier
/// revision to deploy again (a rollback, `rollback_to`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(untagged, try_from = "RawCreateDeployment")]
pub enum CreateDeploymentRequest {
    Apply(ApplyDeploymentRequest),
    Rollback(RollbackDeploymentRequest),
}

/// Commit a reviewed candidate manifest.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ApplyDeploymentRequest {
    pub idempotency_key: String,
    /// The head the candidate was planned on; anything else is
    /// `DEPLOYMENT_HEAD_CONFLICT`.
    pub base_revision: u64,
    pub manifest: Value,
    /// The audit reason the revision records.
    pub reason: String,
    /// The digest `POST /deployment-plans` answered for this candidate on
    /// this base. A candidate that plans to anything else is
    /// `REVIEWED_DIGEST_MISMATCH`, and nothing is committed.
    pub reviewed_manifest_digest: String,
}

/// Commit an earlier revision's manifest again, as a new revision. It is
/// re-checked as of now: a provider revoked since is refused.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct RollbackDeploymentRequest {
    pub idempotency_key: String,
    /// The current head; anything else is `DEPLOYMENT_HEAD_CONFLICT`.
    pub base_revision: u64,
    /// The committed revision whose manifest to deploy again.
    pub rollback_to: u64,
    #[serde(default = "default_rollback_reason")]
    pub reason: String,
}

fn default_rollback_reason() -> String {
    "rollback".to_string()
}

/// What a `POST /deployments` body is read as before it is told apart, so a
/// body that is neither an apply nor a rollback is refused by name.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCreateDeployment {
    idempotency_key: String,
    base_revision: u64,
    #[serde(default, alias = "candidate_manifest")]
    manifest: Option<Value>,
    #[serde(default)]
    reviewed_manifest_digest: Option<String>,
    #[serde(default)]
    rollback_to: Option<u64>,
    #[serde(default)]
    reason: Option<String>,
}

impl TryFrom<RawCreateDeployment> for CreateDeploymentRequest {
    type Error = String;

    fn try_from(raw: RawCreateDeployment) -> Result<Self, Self::Error> {
        match (raw.manifest, raw.rollback_to) {
            (Some(manifest), None) => {
                let reviewed_manifest_digest = raw.reviewed_manifest_digest.ok_or(
                    "an apply needs `reviewed_manifest_digest`: plan the manifest with \
                     POST /deployment-plans and send the digest it answers",
                )?;
                let reason = raw.reason.ok_or("an apply needs a `reason`")?;
                Ok(Self::Apply(ApplyDeploymentRequest {
                    idempotency_key: raw.idempotency_key,
                    base_revision: raw.base_revision,
                    manifest,
                    reason,
                    reviewed_manifest_digest,
                }))
            }
            (None, Some(rollback_to)) => {
                if raw.reviewed_manifest_digest.is_some() {
                    return Err(
                        "a rollback names `rollback_to`, not `reviewed_manifest_digest`".into(),
                    );
                }
                Ok(Self::Rollback(RollbackDeploymentRequest {
                    idempotency_key: raw.idempotency_key,
                    base_revision: raw.base_revision,
                    rollback_to,
                    reason: raw.reason.unwrap_or_else(default_rollback_reason),
                }))
            }
            (Some(_), Some(_)) => Err(
                "send either `manifest` (an apply) or `rollback_to` (a rollback), not both".into(),
            ),
            (None, None) => Err(
                "send `manifest` with its `reviewed_manifest_digest` (an apply), or \
                 `rollback_to` (a rollback)"
                    .into(),
            ),
        }
    }
}

/// The durable receipt of a committed apply or rollback (`POST /api/v2/admin/deployments`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DeploymentReceiptResponse {
    #[serde(default)]
    pub operation_id: String,
    #[serde(default)]
    pub idempotency_key: String,
    #[serde(default)]
    pub principal_id: String,
    #[serde(default)]
    pub principal: String,
    #[serde(default)]
    pub request_hash: String,
    #[serde(default)]
    pub committed_revision: u64,
    #[serde(default)]
    pub manifest_digest: String,
    #[serde(default)]
    pub epoch: u64,
    #[serde(default)]
    pub receiving_node_id: String,
    #[serde(default)]
    pub replayed: bool,
    #[serde(default)]
    pub created_at: String,
}

/// Node/cluster convergence report for a committed deployment revision (`GET /api/v2/admin/deployments/{revision}/convergence`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ConvergenceStatusResponse {
    #[serde(default)]
    pub converged: bool,
    #[serde(default)]
    pub target_revision: u64,
    #[serde(default)]
    pub converged_nodes: Vec<String>,
    #[serde(default)]
    pub pending_nodes: Vec<String>,
    #[serde(default)]
    pub degraded_nodes: Vec<String>,
}

/// A committed apply or rollback (`POST /api/v2/admin/deployments`, `202`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct CreateDeploymentResponse {
    /// Always `accepted`: the revision is committed; nodes converge on it.
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub committed_revision: u64,
    #[serde(default)]
    pub receipt: DeploymentReceiptResponse,
    /// What committing this revision does not do — a rollback does not
    /// reverse an external effect already made.
    #[serde(default)]
    pub warnings: Vec<String>,
    /// This node's convergence on the committed revision, once it tried to
    /// build it.
    #[serde(default)]
    pub evaluation: Option<ConvergenceStatusResponse>,
    /// Why this node could not build the committed revision, when it could
    /// not. The revision stays committed; the node keeps serving what it had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_error: Option<String>,
}

/// A committed deployment revision without its manifest: one row of
/// `GET /api/v2/admin/deployments`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DeploymentRevisionSummary {
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub parent_revision: Option<u64>,
    #[serde(default)]
    pub manifest_digest: String,
    /// Members in the manifest; `0` once compacted.
    #[serde(default)]
    pub member_count: usize,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub created_by: String,
    /// The collector dropped this revision's members: it can no longer be
    /// rolled back to.
    #[serde(default)]
    pub members_compacted: bool,
    #[serde(default)]
    pub created_at: String,
}

/// A committed deployment revision with its manifest
/// (`GET /api/v2/admin/deployments/{revision}`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DeploymentRevisionResponse {
    #[serde(flatten)]
    pub summary: DeploymentRevisionSummary,
    #[serde(default)]
    pub manifest: Value,
}

/// One page of committed revisions, newest first, with the head they were
/// read against (`GET /api/v2/admin/deployments`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DeploymentsPage {
    #[serde(default)]
    pub head_revision: u64,
    #[serde(default)]
    pub epoch: u64,
    /// The revision this node serves.
    #[serde(default)]
    pub applied_revision: u64,
    #[serde(default)]
    pub data: Vec<DeploymentRevisionSummary>,
    #[serde(default)]
    pub limit: u64,
    /// Pass back as `?cursor=` for the next, older page; absent on the last.
    /// Opaque: do not parse it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

fn default_scaffold_version() -> String {
    "1.0.0".to_string()
}

/// Request body to scaffold a new provider package (`POST /api/v2/admin/authoring/scaffold`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ScaffoldRequest {
    pub publisher: String,
    pub provider_id: String,
    #[serde(default = "default_scaffold_version")]
    pub version: String,
    #[serde(alias = "implementation")]
    pub kind: String,
}

/// Response from scaffolding a provider package (`POST /api/v2/admin/authoring/scaffold`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ScaffoldResponse {
    #[serde(default)]
    pub publisher: String,
    #[serde(default)]
    pub provider_id: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub implementation: String,
    #[serde(default)]
    pub package_digest: String,
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

/// Response from linting a provider package (`POST /api/v2/admin/authoring/lint`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct LintResponse {
    #[serde(default)]
    pub valid: bool,
    #[serde(default)]
    pub publisher: Option<String>,
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub implementation: Option<String>,
    #[serde(default)]
    pub package_digest: Option<String>,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// Response from deterministically packing a provider package (`POST /api/v2/admin/authoring/pack`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct PackResponse {
    #[serde(default)]
    pub package_digest: String,
    #[serde(default)]
    pub inventory: Value,
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

/// Request body for Enterprise-to-Community edition transition preflight (`POST /api/v2/admin/edition-transition-preflight`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct EditionTransitionPreflightRequest {
    #[serde(default)]
    pub revision: Option<u64>,
}

/// Enterprise-to-Community edition transition preflight report (`POST /api/v2/admin/edition-transition-preflight`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct EditionTransitionPreflightResponse {
    #[serde(default)]
    pub target_edition: String,
    #[serde(default)]
    pub transition_allowed: bool,
    #[serde(default)]
    pub inspected_revision: u64,
    #[serde(default)]
    pub incompatible_providers: Vec<String>,
    #[serde(default)]
    pub incompatible_channels: Vec<String>,
    #[serde(default)]
    pub findings: Vec<Value>,
    #[serde(default)]
    pub backup_retention_checklist: Vec<String>,
}

// ============================================================
// Definitions: the deployable documents a manifest member names
// ============================================================

/// One deployable definition, as `POST /api/v2/admin/definitions` takes it
/// and the artifact store holds it: its kind and the document of that kind.
/// The artifact's digest covers the whole value, so a manifest member's kind
/// is bound to the definition it names. Identity is not part of it: a
/// member's `id` and `revision` say which entity this definition is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(
    tag = "kind",
    content = "definition",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Definition {
    Workflow(WorkflowDefinition),
    Channel(ChannelDefinition),
    Plugin(PluginDefinition),
}

impl Definition {
    /// The kind, as a manifest member spells it.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Workflow(_) => "workflow",
            Self::Channel(_) => "channel",
            Self::Plugin(_) => "plugin",
        }
    }
}

/// A workflow: the dataflow-rs workflow document without what the deployment
/// supplies (`id`, `version`, `channel`, `status` and `rollout`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct WorkflowDefinition {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Higher is evaluated first among the workflows of one channel.
    #[serde(default)]
    pub priority: i64,
    /// JSONLogic deciding whether the workflow matches a message.
    #[serde(default = "condition_true")]
    pub condition: Value,
    /// The steps: tasks and task groups.
    pub tasks: Value,
    #[serde(default)]
    pub tags: Vec<String>,
    /// The workflow loop, when the steps repeat.
    #[serde(default, rename = "loop")]
    pub loop_config: Option<Value>,
    /// What a failing task does to the run.
    #[serde(default)]
    pub on_failure: WorkflowOnFailure,
}

fn condition_true() -> Value {
    Value::Bool(true)
}

/// A workflow's failure policy, the engine's own vocabulary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum WorkflowOnFailure {
    /// The run stops at the failing task.
    #[default]
    Fail,
    /// The failure is recorded and the next workflow, or the next sweep of a
    /// loop, runs.
    Continue,
}

/// A channel: everything but its identity and its workflow, which is the
/// Workflow its manifest member `requires`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ChannelDefinition {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "channel_type_sync")]
    pub channel_type: crate::enums::ChannelType,
    /// The channel driver that serves it (`rest`, `http`, `cron` in
    /// Community), lowercased.
    #[serde(deserialize_with = "crate::enums::deserialize_protocol")]
    pub protocol: String,
    #[serde(default)]
    pub methods: Option<Vec<String>>,
    #[serde(default)]
    pub route_pattern: Option<String>,
    #[serde(default = "empty_object")]
    pub transport_config: Value,
    #[serde(default = "empty_object")]
    pub config: Value,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub tags: Vec<String>,
}

fn channel_type_sync() -> crate::enums::ChannelType {
    crate::enums::ChannelType::Sync
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

/// A WebAssembly plugin: its `plugin.toml` manifest as JSON and the
/// component itself, so one digest covers both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct PluginDefinition {
    pub manifest: Value,
    /// The component, base64.
    pub component: String,
    /// A detached Ed25519 signature over the component digest, base64.
    #[serde(default)]
    pub signature: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// A stored definition (`POST`/`GET /api/v2/admin/definitions`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DefinitionResponse {
    /// `sha256:…` of the canonical definition: what a manifest member's
    /// `definition_digest` names.
    pub digest: String,
    pub kind: String,
    pub size_bytes: u64,
    /// What the engine reports about a workflow and does not refuse — a
    /// `$`-prefixed key it emits with one `$` stripped, a `validation` whose
    /// failure stops nothing. Informational: the definition was stored.
    /// Empty for a channel or a plugin.
    #[serde(default)]
    pub advisories: Vec<DefinitionAdvisory>,
}

/// One advisory on an uploaded definition ([`DefinitionResponse::advisories`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DefinitionAdvisory {
    /// A stable id for the kind of finding (`engine.unguarded_validation`,
    /// `logic.escaped_template_key`, `engine.advisory`).
    #[serde(default)]
    pub check: String,
    /// Where in the definition it was found, as the engine names it.
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub message: String,
}

/// `POST /api/v2/admin/definitions/test`: run a workflow definition once on
/// the node's engine, against live connectors, without deploying it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct TestDefinitionRequest {
    /// The workflow to run, as `POST /definitions` would take it.
    pub definition: WorkflowDefinition,
    /// The message's `data`.
    pub data: Value,
    /// The message's metadata, normalised the way an ingress builds it.
    #[serde(default)]
    pub metadata: Value,
}

/// One run of a workflow definition (`POST /api/v2/admin/definitions/test`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct TestDefinitionResponse {
    /// Whether the workflow's condition matched the supplied input.
    #[serde(default)]
    pub matched: bool,
    /// The run's trace: the context it started from (`initial`), then each
    /// step with its `task_id`, `result` (`executed` | `skipped` | `failed`)
    /// and the changes it made.
    #[serde(default)]
    pub trace: Value,
    /// The message data after the pipeline ran.
    #[serde(default)]
    pub output: Value,
    #[serde(default)]
    pub errors: Vec<Value>,
    /// Why the run stopped short, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ============================================================
// Compiled deployments: what `orion-server compile --format deployment`
// writes and `orion-cli deployments apply -d` reads
// ============================================================

/// The `schema_version` a compiled deployment directory's
/// `deployment.json` carries.
pub const COMPILED_DEPLOYMENT_SCHEMA_VERSION: u64 = 1;

/// `deployment.json` of a compiled deployment directory: the members a
/// deployment deploys, with no revisions. Only the target knows which
/// revision a changed definition must get, so the deployer assigns them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledDeployment {
    #[serde(default)]
    pub schema_version: u64,
    #[serde(default)]
    pub members: Vec<CompiledMember>,
}

/// One member of a compiled deployment. A workflow, channel or plugin names
/// its `definition` by digest (the file `definitions/<hex>.json`); a
/// connection names its `connection` file (`connections/<id>.json`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledMember {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    #[serde(default)]
    pub requires: Vec<CompiledRequirement>,
}

/// A member another member depends on, by kind and id. The deployer
/// resolves it to the revision it assigns that member.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CompiledRequirement {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub id: String,
}

/// `connections/<id>.json` of a compiled deployment directory: a connection
/// over a built-in driver, without a revision. Its fields are those of a
/// [`CreateConnectionRevisionRequest`] for the connection it names, plus the
/// driver the provider digest names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledConnection {
    #[serde(default)]
    pub connection_id: String,
    /// The built-in driver, such as `org.orion.postgresql`.
    #[serde(default)]
    pub driver: String,
    /// The driver's provider digest, as `GET /runtime-capabilities` reports
    /// it under `provider_backends[].package_digest`.
    #[serde(default)]
    pub provider_digest: String,
    #[serde(default)]
    pub public_config: Value,
    #[serde(default)]
    pub secret_locators: BTreeMap<String, String>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn v2_dtos_tolerate_empty_json_objects() {
        let cap: RuntimeCapabilitiesResponse = serde_json::from_str("{}").expect("default decode");
        assert!(cap.api_version.is_empty());
        assert!(cap.wasm_worlds.is_empty());

        let receipt: DeploymentReceiptResponse =
            serde_json::from_str(r#"{"committed_revision": 3}"#).expect("partial decode");
        assert_eq!(receipt.committed_revision, 3);
        assert!(!receipt.replayed);
    }

    /// A `POST /deployments` body is an apply or a rollback, never both, and
    /// an apply always names the digest it reviewed.
    #[test]
    fn a_deployment_request_is_an_apply_or_a_rollback() {
        let apply: CreateDeploymentRequest = serde_json::from_str(
            r#"{"idempotency_key":"k","base_revision":1,"manifest":{},"reason":"r",
                "reviewed_manifest_digest":"sha256:x"}"#,
        )
        .expect("apply");
        assert!(matches!(apply, CreateDeploymentRequest::Apply(ref a) if a.base_revision == 1));
        let rollback: CreateDeploymentRequest =
            serde_json::from_str(r#"{"idempotency_key":"k","base_revision":3,"rollback_to":1}"#)
                .expect("rollback");
        assert!(matches!(
            rollback,
            CreateDeploymentRequest::Rollback(ref r) if r.rollback_to == 1 && r.reason == "rollback"
        ));
        for refused in [
            r#"{"idempotency_key":"k","base_revision":1,"manifest":{},"reason":"r"}"#,
            r#"{"idempotency_key":"k","base_revision":1}"#,
            r#"{"idempotency_key":"k","base_revision":1,"manifest":{},"rollback_to":1,"reason":"r","reviewed_manifest_digest":"d"}"#,
            r#"{"idempotency_key":"k","base_revision":1,"rollback_to":1,"surprise":true}"#,
        ] {
            assert!(
                serde_json::from_str::<CreateDeploymentRequest>(refused).is_err(),
                "{refused}"
            );
        }
        // Serialized, each variant is its own fields: what the server reads.
        let wire = serde_json::to_value(&rollback).expect("serialize");
        assert_eq!(wire["rollback_to"], 1);
        assert!(wire.get("Rollback").is_none());
    }

    #[test]
    fn scaffold_request_accepts_both_kind_and_implementation() {
        let req_kind: ScaffoldRequest = serde_json::from_str(
            r#"{"publisher":"acme","provider_id":"crm","version":"0.1.0","kind":"workflow"}"#,
        )
        .expect("decode kind");
        assert_eq!(req_kind.kind, "workflow");

        let req_impl: ScaffoldRequest = serde_json::from_str(
            r#"{"publisher":"acme","provider_id":"crm","version":"0.1.0","implementation":"wasm"}"#,
        )
        .expect("decode implementation alias");
        assert_eq!(req_impl.kind, "wasm");
    }
}
