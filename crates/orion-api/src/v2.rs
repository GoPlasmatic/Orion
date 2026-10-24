//! Orion v2 Admin API wire contract.
//!
//! Versioned request and response DTOs shared by `orion-server` and `orion-cli` for `/api/v2/admin/*` endpoints:
//! - Runtime capabilities & edition boundary discovery (`/runtime-capabilities`)
//! - Connector package inventory, upload, admission & revocation (`/connector-packages`)
//! - Connection revisions & health probes (`/connections`)
//! - Candidate deployment planning, CAS apply, convergence & rollback (`/deployments`)
//! - Legacy v1 -> v2 migration report & Enterprise-to-Community preflight (`/migration-report`, `/edition-transition-preflight`)
//! - Provider package authoring scaffold, lint & pack (`/authoring/*`)

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Runtime capabilities and Community/Enterprise assembly boundary report (`GET /api/v2/admin/runtime-capabilities`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct RuntimeCapabilitiesResponse {
    #[serde(default)]
    pub api_version: String,
    #[serde(default)]
    pub edition: String,
    #[serde(default)]
    pub admission_policy_id: String,
    #[serde(default)]
    pub community_boundary_compliant: bool,
    #[serde(default, alias = "channel_drivers")]
    pub registered_channel_drivers: Vec<String>,
    #[serde(default)]
    pub schema_profiles: Vec<String>,
    #[serde(default)]
    pub wasm_worlds: Vec<String>,
    #[serde(default, alias = "wasm_import_profiles")]
    pub supported_wasm_profiles: Vec<String>,
    #[serde(default)]
    pub provider_backends: Vec<Value>,
    #[serde(default)]
    pub registered_rust_providers: Vec<String>,
    #[serde(default)]
    pub registered_workflow_providers: Vec<String>,
    #[serde(default)]
    pub registered_wasm_providers: Vec<String>,
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

/// Provider package admission record (`POST /api/v2/admin/connector-packages/{digest}/admit`).
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

/// Request body for admitting an uploaded provider package (`POST /api/v2/admin/connector-packages/{digest}/admit`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct AdmitPackageRequest {
    #[serde(default)]
    pub lock: Option<Value>,
}

/// Response from admitting a provider package (`POST /api/v2/admin/connector-packages/{digest}/admit`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct AdmitPackageResponse {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub admission: ProviderAdmissionDto,
}

/// Request body for revoking an admitted provider package (`POST /api/v2/admin/connector-packages/{digest}/revoke`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct RevokePackageRequest {
    #[serde(default)]
    pub reason: Option<String>,
}

/// Response from revoking a provider package (`POST /api/v2/admin/connector-packages/{digest}/revoke`).
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

/// Summary of an immutable connection revision (`GET /api/v2/admin/connections`).
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
    #[serde(default)]
    pub secret_locator: Option<String>,
    #[serde(default)]
    pub secret_locators: Value,
    #[serde(default)]
    pub config_fingerprint: String,
    #[serde(default)]
    pub configuration_digest: String,
}

/// Response listing connection revisions (`GET /api/v2/admin/connections`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ConnectionsListResponse {
    #[serde(default)]
    pub count: usize,
    #[serde(default, alias = "data")]
    pub connections: Vec<ConnectionRevisionResponse>,
}

/// Request body to create an immutable connection revision (`POST /api/v2/admin/connections`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct CreateConnectionRevisionRequest {
    pub connection_id: String,
    pub revision: u64,
    pub provider_digest: String,
    #[serde(default, alias = "config")]
    pub public_config: Value,
    #[serde(default)]
    pub secret_locator: Option<String>,
}

/// Response from creating an immutable connection revision (`POST /api/v2/admin/connections`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct CreateConnectionRevisionResponse {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub connection: ConnectionRevisionResponse,
}

/// Request body for probing a connection revision (`POST /api/v2/admin/connections/{id}/probe`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ProbeConnectionRequest {
    #[serde(default)]
    pub revision: Option<u64>,
    #[serde(default)]
    pub driver_id: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
}

/// Result of an explicit connection health probe (`POST /api/v2/admin/connections/{id}/probe`).
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
pub struct CreateDeploymentPlanRequest {
    pub base_revision: u64,
    #[serde(alias = "candidate_manifest")]
    pub manifest: Value,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Candidate deployment plan with reviewed digest and sanitized diff (`POST /api/v2/admin/deployment-plans`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DeploymentPlanResponse {
    #[serde(default)]
    pub base_revision: u64,
    #[serde(default)]
    pub candidate_digest: String,
    #[serde(default)]
    pub reviewed_manifest_digest: String,
    #[serde(default)]
    pub request_hash: String,
    #[serde(default)]
    pub added_entities: Vec<Value>,
    #[serde(default)]
    pub updated_entities: Vec<String>,
    #[serde(default)]
    pub removed_entities: Vec<Value>,
    #[serde(default)]
    pub external_effect_warnings: Vec<String>,
    #[serde(default)]
    pub findings: Vec<Value>,
}

/// Request body to atomically apply a candidate deployment (`POST /api/v2/admin/deployments`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ApplyDeploymentRequest {
    pub idempotency_key: String,
    pub base_revision: u64,
    #[serde(alias = "candidate_manifest")]
    pub manifest: Value,
    pub reason: String,
    #[serde(default)]
    pub reviewed_manifest_digest: Option<String>,
}

/// Durable deployment apply/rollback receipt (`POST /api/v2/admin/deployments`).
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

/// Response from applying a candidate deployment (`POST /api/v2/admin/deployments`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ApplyDeploymentResponse {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub committed_revision: u64,
    #[serde(default)]
    pub receipt: DeploymentReceiptResponse,
    /// This node's convergence on the committed revision, once it tried to
    /// build it.
    #[serde(default)]
    pub evaluation: Option<ConvergenceStatusResponse>,
    /// Why this node could not build the committed revision, when it could
    /// not. The revision stays committed; the node keeps serving what it had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_error: Option<String>,
}

/// Committed deployment revision record (`GET /api/v2/admin/deployments/{revision}`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DeploymentRevisionResponse {
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub parent_revision: Option<u64>,
    #[serde(default)]
    pub manifest_digest: String,
    #[serde(default)]
    pub manifest: Value,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub members_compacted: bool,
    #[serde(default)]
    pub created_at: String,
}

/// Summary of deployment store head and committed revisions (`GET /api/v2/admin/deployments`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DeploymentsSummaryResponse {
    #[serde(default)]
    pub head_revision: u64,
    #[serde(default)]
    pub epoch: u64,
    #[serde(default)]
    pub applied_revision: u64,
    #[serde(default)]
    pub revisions: Vec<DeploymentRevisionResponse>,
}

fn default_rollback_reason() -> String {
    "rollback".to_string()
}

/// Request body to roll back to a prior deployment revision (`POST /api/v2/admin/deployments/rollback`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct RollbackDeploymentRequest {
    pub idempotency_key: String,
    pub expected_head_revision: u64,
    pub target_revision: u64,
    #[serde(default = "default_rollback_reason")]
    pub reason: String,
}

/// Response from rolling back to a prior deployment revision (`POST /api/v2/admin/deployments/rollback`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct RollbackDeploymentResponse {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub committed_revision: u64,
    #[serde(default)]
    pub receipt: DeploymentReceiptResponse,
    #[serde(default, alias = "external_effect_warnings")]
    pub warnings: Vec<String>,
    /// This node's convergence on the new revision, once it tried to build it.
    #[serde(default)]
    pub evaluation: Option<ConvergenceStatusResponse>,
    /// Why this node could not build the new revision, when it could not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_error: Option<String>,
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

/// Snapshot of a legacy v1 connector for migration reporting (`POST /api/v2/admin/migration-report`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct LegacyConnectorItem {
    #[serde(alias = "id")]
    pub name: String,
    pub connector_type: String,
    #[serde(default)]
    pub config: Value,
}

fn default_one() -> u64 {
    1
}

/// Snapshot of a legacy v1 workflow for migration reporting (`POST /api/v2/admin/migration-report`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct LegacyWorkflowItem {
    pub id: String,
    #[serde(default = "default_one")]
    pub version: u64,
    #[serde(default)]
    pub definition: Value,
}

/// Snapshot of a legacy v1 channel for migration reporting (`POST /api/v2/admin/migration-report`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct LegacyChannelItem {
    pub id: String,
    #[serde(default = "default_one")]
    pub version: u64,
    #[serde(default)]
    pub workflow_id: String,
    #[serde(default = "default_one")]
    pub workflow_version: u64,
}

/// Request body for generating a legacy v1 -> v2 migration report (`POST /api/v2/admin/migration-report`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct MigrationReportRequest {
    #[serde(default)]
    pub connectors: Vec<LegacyConnectorItem>,
    #[serde(default)]
    pub workflows: Vec<LegacyWorkflowItem>,
    #[serde(default)]
    pub channels: Vec<LegacyChannelItem>,
}

/// Legacy v1 -> v2 migration conversion report (`POST /api/v2/admin/migration-report`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct MigrationReportResponse {
    #[serde(default)]
    pub edition: String,
    #[serde(default)]
    pub compatible: bool,
    #[serde(default)]
    pub can_cutover: bool,
    #[serde(default)]
    pub converted_connections: usize,
    #[serde(default)]
    pub resolved_connectors: Vec<Value>,
    #[serde(default)]
    pub findings: Vec<Value>,
    #[serde(default)]
    pub blocking_findings: Vec<Value>,
    #[serde(default)]
    pub bootstrap_manifest: Option<Value>,
    #[serde(default)]
    pub bootstrap_manifest_digest: Option<String>,
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn v2_dtos_tolerate_empty_json_objects() {
        let cap: RuntimeCapabilitiesResponse = serde_json::from_str("{}").expect("default decode");
        assert!(cap.api_version.is_empty());
        assert!(!cap.community_boundary_compliant);

        let receipt: DeploymentReceiptResponse =
            serde_json::from_str(r#"{"committed_revision": 3}"#).expect("partial decode");
        assert_eq!(receipt.committed_revision, 3);
        assert!(!receipt.replayed);
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
