//! Orion v2 Admin API wire contract (`V2-07.1`–`V2-07.5`, `V2-16.4`).
//!
//! Versioned DTOs for `/api/v2/admin/*` endpoints:
//! - Runtime capabilities & edition boundary discovery (`/runtime-capabilities`)
//! - Connector package inventory, upload, admission & revocation (`/connector-packages`)
//! - Connection revisions & health probes (`/connections`)
//! - Candidate deployment planning, CAS apply, convergence & rollback (`/deployments`)
//! - Legacy v1 -> v2 migration report & Enterprise-to-Community preflight (`/migration-report`, `/edition-transition-preflight`)
//! - Provider package authoring scaffold, lint & pack (`/authoring/*`)

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Runtime capabilities and Community/Enterprise assembly boundary report (`GET /api/v2/admin/runtime-capabilities`, `V2-07.1`, `TD-31`).
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

/// Summary of an uploaded/admitted connector provider package (`GET /api/v2/admin/connector-packages`, `V2-07.3`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ConnectorPackageSummary {
    #[serde(default)]
    pub publisher: String,
    #[serde(default)]
    pub provider_id: String,
    #[serde(default)]
    pub version: String,
    #[serde(default, alias = "package_digest")]
    pub digest: String,
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
}

/// Summary of an immutable connection revision (`GET /api/v2/admin/connections`, `V2-07.3`).
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
    #[serde(default, alias = "public_config")]
    pub config: Value,
    #[serde(default)]
    pub secret_locator: Option<String>,
    #[serde(default)]
    pub secret_locators: Value,
    #[serde(default)]
    pub config_fingerprint: String,
}

/// Result of an explicit connection health probe (`POST /api/v2/admin/connections/{id}/probe`, `V2-07.3`, `TD-31`).
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

/// Candidate deployment plan with reviewed digest and sanitized diff (`POST /api/v2/admin/deployment-plans`, `V2-07.4`, `TD-31`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DeploymentPlanResponse {
    #[serde(default)]
    pub base_revision: u64,
    #[serde(default, alias = "candidate_digest")]
    pub reviewed_manifest_digest: String,
    #[serde(default)]
    pub request_hash: String,
    #[serde(default)]
    pub added_entities: Vec<Value>,
    #[serde(default)]
    pub updated_entities: Vec<String>,
    #[serde(default)]
    pub removed_entities: Vec<Value>,
    #[serde(default, alias = "external_effect_warnings")]
    pub findings: Vec<Value>,
}

/// Durable deployment apply/rollback receipt (`POST /api/v2/admin/deployments`, `V2-07.4`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DeploymentReceiptResponse {
    #[serde(default)]
    pub idempotency_key: String,
    #[serde(default)]
    pub principal_id: String,
    #[serde(default)]
    pub request_hash: String,
    #[serde(default)]
    pub committed_revision: u64,
    #[serde(default)]
    pub manifest_digest: String,
    #[serde(default)]
    pub receiving_node_id: String,
    #[serde(default)]
    pub replayed: bool,
    #[serde(default)]
    pub created_at: String,
}

/// Committed deployment revision record (`GET /api/v2/admin/deployments/{revision}`, `V2-07.4`).
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
    pub created_at: String,
}

/// Node/cluster convergence report for a committed deployment revision (`GET /api/v2/admin/deployments/{revision}/convergence`, `V2-07.4`).
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

/// Legacy v1 -> v2 migration conversion report (`POST /api/v2/admin/migration-report`, `V2-07.5`, `TD-31`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct MigrationReportResponse {
    #[serde(default)]
    pub edition: String,
    #[serde(default, alias = "compatible")]
    pub can_cutover: bool,
    #[serde(default)]
    pub converted_connections: usize,
    #[serde(default)]
    pub resolved_connectors: Vec<Value>,
    #[serde(default, alias = "findings")]
    pub blocking_findings: Vec<Value>,
    #[serde(default)]
    pub bootstrap_manifest: Option<Value>,
    #[serde(default)]
    pub bootstrap_manifest_digest: Option<String>,
}

/// Enterprise-to-Community edition transition preflight report (`POST /api/v2/admin/edition-transition-preflight`, `V2-16.4`).
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
}
