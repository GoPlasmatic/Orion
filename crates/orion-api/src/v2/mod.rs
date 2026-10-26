//! Orion v2 Admin API wire contract.
//!
//! Versioned request and response DTOs shared by `orion-server` and `orion-cli` for `/api/v2/admin/*` endpoints:
//! - Runtime capabilities & edition discovery (`/runtime-capabilities`)
//! - Connector packages, their admission and revocations (`/connector-packages`)
//! - Connection revisions and their probes (`/connections/{id}/revisions`)
//! - Definitions, by digest (`/definitions`)
//! - Candidate deployment plans (`/deployment-plans`), and deployments — an
//!   apply or a rollback, both under CAS — with their convergence (`/deployments`)
//! - Enterprise-to-Community edition transition preflight (`/edition-transition-preflight`)
//! - Provider package authoring scaffold, lint & pack (`/authoring/*`)
//!
//! # Strict, and versioned
//!
//! Unlike the operational DTOs in [`crate::dto`], this contract is
//! **strict**: a field is required unless its absence has a meaning the
//! field's documentation states, every request type refuses an unknown
//! field, a closed vocabulary is an enum ([`vocab`]), and a part the server
//! generates is a typed struct rather than a JSON value. A server bug that
//! drops a field is then a decode error in the client, not a silent `0`,
//! `""` or `false`, and the OpenAPI document describes every part.
//!
//! What stays a [`serde_json::Value`] is a document someone else owns the
//! shape of: a definition body or a channel's `config`, a connection's
//! `public_config`, a provider package's own files (`manifest`,
//! `distribution`, `inventory`), a provider lock, the data a workflow test
//! run took and produced, and the engine's trace of that run (dataflow-rs's
//! shape, not this contract's).
//!
//! Version skew is negotiated rather than tolerated: the server reports
//! [`API_VERSION`] as [`RuntimeCapabilitiesResponse::api_version`], and a
//! client speaking another refuses to go on. A response *gaining* a field
//! is not a change (clients ignore what they do not know); anything else —
//! a field removed, renamed, retyped or made required, a request field
//! added, a variant added to an enum an answer carries — moves the number.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod manifest;
pub mod vocab;

pub use manifest::{
    COMPILED_DEPLOYMENT_SCHEMA_VERSION, CompiledConnection, CompiledDeployment, CompiledMember,
    CompiledRequirement, DEPLOYMENT_MANIFEST_SCHEMA_VERSION, DeploymentManifest, EntityRef,
    ManifestMember, ManifestProvider, RolloutInterval,
};
pub use vocab::{
    ConnectorErrorClass, EditionFindingCode, EditionTier, EffectStatus, EntityKind,
    ImplementationKind, PackageArtifactState, UnknownName,
};

/// The revision of the `/api/v2/admin` contract this crate defines, which
/// a server reports as [`RuntimeCapabilitiesResponse::api_version`]. A
/// client speaks exactly one; see the module documentation for what moves
/// it.
pub const API_VERSION: u32 = 1;

// ============================================================
// Runtime capabilities
// ============================================================

/// What this node is and serves (`GET /api/v2/admin/runtime-capabilities`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct RuntimeCapabilitiesResponse {
    /// The contract revision this server speaks: [`API_VERSION`] of the
    /// `orion-api` it was built with. A client that speaks another must
    /// not read the rest of this answer, or any other v2 answer.
    pub api_version: u32,
    pub edition: EditionTier,
    /// The package admission policy this node admits under.
    pub admission_policy_id: String,
    /// The channel protocols this node serves (`rest`, `http`, `cron`, …).
    pub channel_drivers: Vec<String>,
    pub schema_profiles: Vec<String>,
    /// The WebAssembly plugin worlds this node runs.
    pub wasm_worlds: Vec<String>,
    /// Every connector provider backend this node registers.
    pub provider_backends: Vec<ProviderBackendInfo>,
    pub deployment_head: DeploymentHead,
    pub native_scheduler: NativeSchedulerStatus,
    /// This node's reconciler: what it serves and how it got there.
    pub node_status: NodeStatus,
}

/// One registered provider backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ProviderBackendInfo {
    pub kind: ImplementationKind,
    /// The backend id: a `rust` driver's id, or a `workflow`/`wasm`
    /// package's digest.
    pub driver_id: String,
    /// A built-in driver's fixed provider digest — what a connection
    /// revision over it names as its `provider_digest`. Absent for a
    /// backend that is not a built-in driver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package_digest: Option<String>,
}

/// The committed deployment head.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DeploymentHead {
    /// `0` before the first apply.
    pub revision: u64,
    pub epoch: u64,
}

/// The node's native work scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct NativeSchedulerStatus {
    pub in_flight: u64,
    pub max_in_flight: u64,
}

/// One node's reconciler status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct NodeStatus {
    pub node_id: String,
    /// Changes on every process start.
    pub boot_id: String,
    /// The revision the node is converging on.
    pub desired_revision: u64,
    /// The revision the node serves; `0` before it served any.
    pub applied_revision: u64,
    /// The manifest digest of the applied revision; absent before one.
    pub applied_manifest_digest: Option<String>,
    /// Whether some member of the applied revision is quarantined.
    pub configuration_degraded: bool,
    pub quarantined_entities: Vec<QuarantinedEntity>,
    /// Why the last attempt to build the desired revision failed, while it
    /// does.
    pub last_error: Option<String>,
    pub backoff_attempt: u32,
    pub backoff_delay_ms: u64,
    pub security_lease_expires_at_ms: u64,
    /// The revisions the node keeps built, ascending.
    pub retained_revisions: Vec<u64>,
}

/// A member the node serves without, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct QuarantinedEntity {
    pub entity: EntityRef,
    pub reason: String,
}

// ============================================================
// Connector packages
// ============================================================

/// A stored connector provider package (`GET /api/v2/admin/connector-packages`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ConnectorPackageSummary {
    pub publisher: String,
    pub provider_id: String,
    pub version: String,
    /// The package digest: `sha256:<64 lowercase hex>`.
    pub digest: String,
    pub implementation: ImplementationKind,
    /// The tier `distribution.json` declares; `community` when it declares
    /// none.
    pub tier: EditionTier,
    /// Admitted and not revoked.
    pub admitted: bool,
    pub revoked: bool,
    pub artifact_state: PackageArtifactState,
    /// The package's `connector.json`.
    pub manifest: Value,
    /// The package's `distribution.json`.
    pub distribution: Value,
    /// The package's `inventory.json`.
    pub inventory: Value,
}

/// A package's admission record (`PUT /api/v2/admin/connector-packages/{digest}/admission`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ProviderAdmissionDto {
    pub package_digest: String,
    pub policy_id: String,
    /// Changes when the policy or the package's verified inputs do: an
    /// admission recorded under another fingerprint is invalid.
    pub admission_fingerprint: String,
    pub revoked: bool,
}

/// Every stored package, and every admission record (`GET /api/v2/admin/connector-packages`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ConnectorPackagesListResponse {
    pub count: usize,
    pub packages: Vec<ConnectorPackageSummary>,
    pub admissions: Vec<ProviderAdmissionDto>,
}

/// One connector provider package and its admission record
/// (`GET /api/v2/admin/connector-packages/{digest}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ConnectorPackageResponse {
    pub package: ConnectorPackageSummary,
    /// The admission recorded for the package; absent while it was never
    /// admitted.
    pub admission: Option<ProviderAdmissionDto>,
}

/// A provider package's files: the body of `POST /api/v2/admin/connector-packages`,
/// `/authoring/lint` and `/authoring/pack`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct UploadPackageRequest {
    /// Relative path → the file, as UTF-8 text or `base64:<…>`.
    pub files: BTreeMap<String, String>,
    /// Upload only: admit the package in the same request. Absent means
    /// `false`.
    #[serde(default)]
    pub admit: bool,
    /// Upload only, with `admit`: the provider lock the admission must
    /// verify against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<Value>,
}

/// An uploaded package (`POST /api/v2/admin/connector-packages`, `201`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct UploadPackageResponse {
    pub package: ConnectorPackageSummary,
    /// The admission, when the upload asked for one.
    pub admission: Option<ProviderAdmissionDto>,
}

/// Request body for admitting an uploaded provider package
/// (`PUT /api/v2/admin/connector-packages/{digest}/admission`). The body is
/// optional.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct AdmitPackageRequest {
    /// The provider lock the admission must verify against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<Value>,
}

/// The admission recorded (`PUT /api/v2/admin/connector-packages/{digest}/admission`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct AdmitPackageResponse {
    pub admission: ProviderAdmissionDto,
}

/// Request body for revoking an admitted provider package
/// (`POST /api/v2/admin/connector-packages/{digest}/revocations`). The body
/// is optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct RevokePackageRequest {
    /// Why, for the audit record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A recorded revocation (`POST /api/v2/admin/connector-packages/{digest}/revocations`, `201`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct RevokePackageResponse {
    pub package_digest: String,
    /// The reason the request gave, if it gave one.
    pub reason: Option<String>,
}

// ============================================================
// Connections
// ============================================================

/// An immutable connection revision (`GET /api/v2/admin/connections/{id}/revisions/{revision}`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ConnectionRevisionResponse {
    pub connection_id: String,
    pub revision: u64,
    pub provider_digest: String,
    /// The connection's settings, as created.
    pub public_config: Value,
    /// Config path → secret reference (`env://`, `vault://`, …).
    pub secret_locators: BTreeMap<String, String>,
    /// Over a `workflow` provider package: the connection each of the
    /// package's dependency aliases calls (`alias -> connection_id`). Empty
    /// for any other provider.
    pub bindings: BTreeMap<String, String>,
    /// The digest over the public settings, the locators and the bindings:
    /// what a manifest member for this revision names as its
    /// `definition_digest`.
    pub configuration_digest: String,
}

/// Connection revisions, ordered by connection then revision
/// (`GET /api/v2/admin/connections`, `GET /api/v2/admin/connections/{id}/revisions`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ConnectionsListResponse {
    pub count: usize,
    pub connections: Vec<ConnectionRevisionResponse>,
}

/// Request body to create an immutable connection revision
/// (`POST /api/v2/admin/connections/{id}/revisions`). The connection is the
/// one the path names.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    pub public_config: Value,
    /// Where each credential comes from: a config path (`connection_string`,
    /// `auth.token`, `auth.client_secret`, …) to a secret reference such as
    /// `env://ORDERS_DB_URL`. Placed at that path when the connection is
    /// used. Absent means none.
    #[serde(default)]
    pub secret_locators: BTreeMap<String, String>,
    /// Over a `workflow` provider package: which connection each of the
    /// package's declared dependency aliases calls (`alias -> connection_id`).
    /// Every alias must be bound, and each target must be a stored
    /// connection; a deployment member for this connection must require each
    /// bound connection. Absent means none, as it must be for any other
    /// provider.
    #[serde(default)]
    pub bindings: BTreeMap<String, String>,
}

/// A created (or, for identical content, the stored) connection revision
/// (`POST /api/v2/admin/connections/{id}/revisions`, `201`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct CreateConnectionRevisionResponse {
    pub connection: ConnectionRevisionResponse,
}

/// One health probe of a connection revision
/// (`POST /api/v2/admin/connections/{id}/revisions/{revision}/probes`). A probe
/// that ran answers `200` whatever it found: `healthy` is the finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ConnectionProbeResponse {
    pub connection_id: String,
    pub revision: u64,
    /// The built-in driver, or the backend id of the provider package.
    pub driver_id: String,
    pub healthy: bool,
    pub latency_ms: u64,
    pub details: ProbeDetails,
}

/// Which check a probe ran, and why it failed when it did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ProbeDetails {
    /// The check: `configuration`, `SELECT 1`, `TCP connect to the URL's
    /// host`, `cache read of a probe key`, `provider health check`.
    pub probe: String,
    /// Why it failed; absent when it passed. Never a resolved secret.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The failure's normalised class, when a provider answered one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_class: Option<ConnectorErrorClass>,
    /// Whether the failed check may have reached the backend, when a
    /// provider answered it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effect_status: Option<EffectStatus>,
}

// ============================================================
// Deployment plans and deployments
// ============================================================

/// Request body for planning a candidate deployment (`POST /api/v2/admin/deployment-plans`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct CreateDeploymentPlanRequest {
    /// The head the candidate is planned on: `0` before the first apply.
    pub base_revision: u64,
    pub manifest: DeploymentManifest,
}

/// A member the base deploys at one revision and the candidate at another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DeploymentEntityUpdate {
    pub kind: EntityKind,
    pub id: String,
    pub from_revision: u64,
    pub to_revision: u64,
}

/// A candidate deployment, checked against its base
/// (`POST /api/v2/admin/deployment-plans`). Nothing is stored: the plan is a
/// function of the request and the store, and `reviewed_manifest_digest` is
/// what an apply must name to commit exactly this candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DeploymentPlanResponse {
    pub base_revision: u64,
    /// The candidate's canonical manifest digest. An apply names it as its
    /// `reviewed_manifest_digest`.
    pub reviewed_manifest_digest: String,
    /// Members the base does not deploy at all.
    pub added_entities: Vec<EntityRef>,
    /// Members the base deploys at another revision.
    pub updated_entities: Vec<DeploymentEntityUpdate>,
    /// Members the base deploys and the candidate drops: they stop serving.
    pub removed_entities: Vec<EntityRef>,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ApplyDeploymentRequest {
    pub idempotency_key: String,
    /// The head the candidate was planned on; anything else is
    /// `DEPLOYMENT_HEAD_CONFLICT`.
    pub base_revision: u64,
    pub manifest: DeploymentManifest,
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
    /// The audit reason the revision records; `rollback` when absent.
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
    manifest: Option<DeploymentManifest>,
    reviewed_manifest_digest: Option<String>,
    rollback_to: Option<u64>,
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

/// The durable receipt of a committed apply or rollback. A replay of the
/// same idempotency key answers the receipt the first request committed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DeploymentReceiptResponse {
    pub operation_id: String,
    pub idempotency_key: String,
    /// The admin key id that committed it, or `anonymous`.
    pub principal: String,
    pub request_hash: String,
    pub committed_revision: u64,
    pub manifest_digest: String,
    pub epoch: u64,
    /// The node that took the write: what `?target=this-node` waits on.
    pub receiving_node_id: String,
    /// ISO 8601, on the database clock.
    pub created_at: String,
}

/// Node/cluster convergence on a committed deployment revision (`GET /api/v2/admin/deployments/{revision}/convergence`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ConvergenceStatusResponse {
    pub converged: bool,
    pub target_revision: u64,
    pub converged_nodes: Vec<String>,
    pub pending_nodes: Vec<String>,
    pub degraded_nodes: Vec<String>,
}

/// A committed apply or rollback (`POST /api/v2/admin/deployments`, `202`):
/// the revision is committed, and nodes converge on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct CreateDeploymentResponse {
    pub committed_revision: u64,
    pub receipt: DeploymentReceiptResponse,
    /// What committing this revision does not do — a rollback does not
    /// reverse an external effect already made. Empty for an apply.
    pub warnings: Vec<String>,
    /// This node's convergence on the committed revision, after it tried to
    /// build it.
    pub evaluation: ConvergenceStatusResponse,
    /// Why this node could not build the committed revision, when it could
    /// not. The revision stays committed; the node keeps serving what it had.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_error: Option<String>,
}

/// A committed deployment revision without its manifest: one row of
/// `GET /api/v2/admin/deployments`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DeploymentRevisionSummary {
    pub revision: u64,
    /// The head it was committed on; absent for the first revision.
    pub parent_revision: Option<u64>,
    pub manifest_digest: String,
    /// Members in the manifest; `0` once compacted.
    pub member_count: usize,
    pub reason: String,
    pub created_by: String,
    /// The collector dropped this revision's members: it can no longer be
    /// rolled back to.
    pub members_compacted: bool,
    /// ISO 8601, on the database clock.
    pub created_at: String,
}

/// A committed deployment revision with its manifest
/// (`GET /api/v2/admin/deployments/{revision}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DeploymentRevisionResponse {
    #[serde(flatten)]
    pub summary: DeploymentRevisionSummary,
    /// The canonical manifest. A compacted revision's has no members.
    pub manifest: DeploymentManifest,
}

/// One page of committed revisions, newest first, with the head they were
/// read against (`GET /api/v2/admin/deployments`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DeploymentsPage {
    pub head_revision: u64,
    pub epoch: u64,
    /// The revision this node serves.
    pub applied_revision: u64,
    pub data: Vec<DeploymentRevisionSummary>,
    pub limit: u64,
    /// Pass back as `?cursor=` for the next, older page; absent on the last.
    /// Opaque: do not parse it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

// ============================================================
// Provider package authoring
// ============================================================

fn default_scaffold_version() -> String {
    "1.0.0".to_string()
}

/// Request body to scaffold a new provider package (`POST /api/v2/admin/authoring/scaffold`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ScaffoldRequest {
    pub publisher: String,
    pub provider_id: String,
    /// The package's semantic version; `1.0.0` when absent.
    #[serde(default = "default_scaffold_version")]
    pub version: String,
    pub kind: ImplementationKind,
}

/// A generated provider package (`POST /api/v2/admin/authoring/scaffold`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ScaffoldResponse {
    pub publisher: String,
    pub provider_id: String,
    pub version: String,
    pub implementation: ImplementationKind,
    /// The digest the files pack to.
    pub package_digest: String,
    /// Relative path → the file, as UTF-8 text or `base64:<…>`.
    pub files: BTreeMap<String, String>,
}

/// A provider package's lint report (`POST /api/v2/admin/authoring/lint`):
/// `200` when it is valid, `422` when it is not, `400` when a file does not
/// decode.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct LintResponse {
    pub valid: bool,
    /// The provider id `connector.json` declares; absent when it could not
    /// be read.
    pub provider_id: Option<String>,
    /// The version `connector.json` declares; absent when it could not be
    /// read.
    pub version: Option<String>,
    /// The implementation kind `connector.json` declares; absent when it
    /// could not be read or is not one of the kinds.
    pub implementation: Option<ImplementationKind>,
    /// Each problem, `CODE: where`.
    pub errors: Vec<String>,
}

/// A deterministically packed provider package (`POST /api/v2/admin/authoring/pack`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct PackResponse {
    pub package_digest: String,
    /// The package's files, `inventory.json` included, ready for upload.
    pub files: BTreeMap<String, String>,
}

// ============================================================
// Edition transition preflight
// ============================================================

/// Request body for Enterprise-to-Community edition transition preflight
/// (`POST /api/v2/admin/edition-transition-preflight`). The body is optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct EditionTransitionPreflightRequest {
    /// The revision to inspect; the head when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
}

/// Enterprise-to-Community edition transition preflight report (`POST /api/v2/admin/edition-transition-preflight`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct EditionTransitionPreflightResponse {
    pub target_edition: EditionTier,
    pub transition_allowed: bool,
    pub inspected_revision: u64,
    /// `publisher.provider_id` of each provider only Enterprise serves.
    pub incompatible_providers: Vec<String>,
    pub incompatible_channels: Vec<String>,
    pub findings: Vec<EditionFinding>,
    pub backup_retention_checklist: Vec<String>,
}

/// One reason a revision cannot move to the target edition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct EditionFinding {
    pub code: EditionFindingCode,
    /// `publisher.provider_id`.
    pub provider: String,
    pub digest: String,
    pub message: String,
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
    pub fn kind(&self) -> EntityKind {
        match self {
            Self::Workflow(_) => EntityKind::Workflow,
            Self::Channel(_) => EntityKind::Channel,
            Self::Plugin(_) => EntityKind::Plugin,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DefinitionResponse {
    /// `sha256:…` of the canonical definition: what a manifest member's
    /// `definition_digest` names.
    pub digest: String,
    pub kind: EntityKind,
    pub size_bytes: u64,
    /// What the engine reports about a workflow and does not refuse — a
    /// `$`-prefixed key it emits with one `$` stripped, a `validation` whose
    /// failure stops nothing. Informational: the definition was stored.
    /// Empty for a channel or a plugin.
    pub advisories: Vec<DefinitionAdvisory>,
}

/// One advisory on an uploaded definition ([`DefinitionResponse::advisories`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DefinitionAdvisory {
    /// A stable id for the kind of finding (`engine.unguarded_validation`,
    /// `logic.escaped_template_key`, `engine.advisory`). A later version
    /// may add ids, so this is an identifier, not an enum.
    pub check: String,
    /// Where in the definition it was found, as the engine names it.
    pub path: String,
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
    /// The message's metadata, normalised the way an ingress builds it;
    /// empty when absent.
    #[serde(default)]
    pub metadata: Value,
}

/// One run of a workflow definition (`POST /api/v2/admin/definitions/test`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct TestDefinitionResponse {
    /// Whether the workflow's condition matched the supplied input.
    pub matched: bool,
    /// The engine's trace of the run: the context it started from
    /// (`initial`), then each step with its `task_id`, `result`
    /// (`executed` | `skipped` | `failed`) and the changes it made. Its
    /// shape is dataflow-rs's, so it is carried as the engine writes it.
    pub trace: Value,
    /// The message data after the pipeline ran.
    pub output: Value,
    /// The errors the engine recorded on the message, as it records them.
    pub errors: Vec<Value>,
    /// Why the run stopped short, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    /// An answer missing a field is a decode error, not a silent default:
    /// a server that drops one is caught by its client.
    #[test]
    fn an_answer_missing_a_field_does_not_decode() {
        assert!(serde_json::from_str::<RuntimeCapabilitiesResponse>("{}").is_err());
        assert!(
            serde_json::from_str::<DeploymentReceiptResponse>(r#"{"committed_revision": 3}"#)
                .is_err()
        );
        // A field the client does not know is not a change.
        let page: DeploymentsPage = serde_json::from_str(
            r#"{"head_revision":1,"epoch":1,"applied_revision":1,"data":[],"limit":50,
                "something_new":true}"#,
        )
        .unwrap();
        assert_eq!(page.head_revision, 1);
    }

    /// A request carrying a field the server does not read is refused, not
    /// half-applied.
    #[test]
    fn a_request_with_an_unknown_field_is_refused() {
        for (what, body) in [
            (
                "connection",
                r#"{"revision":1,"provider_digest":"d","public_config":{},"config":{}}"#,
            ),
            ("upload", r#"{"files":{},"entries":[]}"#),
            (
                "scaffold",
                r#"{"publisher":"p","provider_id":"i","kind":"rust","implementation":"rust"}"#,
            ),
            ("plan", r#"{"base_revision":0,"candidate_manifest":{}}"#),
        ] {
            let refused = match what {
                "connection" => {
                    serde_json::from_str::<CreateConnectionRevisionRequest>(body).is_err()
                }
                "upload" => serde_json::from_str::<UploadPackageRequest>(body).is_err(),
                "scaffold" => serde_json::from_str::<ScaffoldRequest>(body).is_err(),
                _ => serde_json::from_str::<CreateDeploymentPlanRequest>(body).is_err(),
            };
            assert!(refused, "{what}: {body}");
        }
        let manifest = r#"{"schema_version":2,"providers":[],"members":[],"edition":"community"}"#;
        assert!(serde_json::from_str::<DeploymentManifest>(manifest).is_err());
    }

    /// A `POST /deployments` body is an apply or a rollback, never both, and
    /// an apply always names the digest it reviewed.
    #[test]
    fn a_deployment_request_is_an_apply_or_a_rollback() {
        let manifest = r#"{"schema_version":2,"providers":[],"members":[]}"#;
        let apply: CreateDeploymentRequest = serde_json::from_str(&format!(
            r#"{{"idempotency_key":"k","base_revision":1,"manifest":{manifest},"reason":"r",
                "reviewed_manifest_digest":"sha256:x"}}"#
        ))
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
            format!(
                r#"{{"idempotency_key":"k","base_revision":1,"manifest":{manifest},"reason":"r"}}"#
            ),
            r#"{"idempotency_key":"k","base_revision":1}"#.to_string(),
            format!(
                r#"{{"idempotency_key":"k","base_revision":1,"manifest":{manifest},"rollback_to":1,"reason":"r","reviewed_manifest_digest":"d"}}"#
            ),
            r#"{"idempotency_key":"k","base_revision":1,"rollback_to":1,"surprise":true}"#
                .to_string(),
        ] {
            assert!(
                serde_json::from_str::<CreateDeploymentRequest>(&refused).is_err(),
                "{refused}"
            );
        }
        // Serialized, each variant is its own fields: what the server reads.
        let wire = serde_json::to_value(&rollback).expect("serialize");
        assert_eq!(wire["rollback_to"], 1);
        assert!(wire.get("Rollback").is_none());
    }

    /// A manifest reads back as it was written: `requires` spelled out,
    /// what is absent left out.
    #[test]
    fn a_manifest_round_trips() {
        let text = r#"{"schema_version":2,"providers":[],"members":[
            {"entity":{"kind":"channel","id":"c","revision":1},"definition_digest":"sha256:a",
             "requires":[{"kind":"workflow","id":"w","revision":2}]},
            {"entity":{"kind":"workflow","id":"w","revision":2},"definition_digest":"sha256:b"}]}"#;
        let manifest: DeploymentManifest = serde_json::from_str(text).unwrap();
        assert_eq!(manifest.members[0].requires[0].kind, EntityKind::Workflow);
        assert!(manifest.members[1].requires.is_empty());
        let wire = serde_json::to_value(&manifest).unwrap();
        assert_eq!(wire["members"][1]["requires"], serde_json::json!([]));
        assert!(wire["members"][1].get("rollouts").is_none());
        assert!(wire.get("provider_lock_digest").is_none());
        assert_eq!(
            serde_json::from_value::<DeploymentManifest>(wire).unwrap(),
            manifest
        );
    }
}
