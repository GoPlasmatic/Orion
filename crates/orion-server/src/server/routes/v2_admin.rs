//! V2 Admin API routes (`/api/v2/admin/*`) — `V2-07.1`, `V2-07.2`, `V2-07.3`, `V2-07.4`, `V2-07.5`.
//!
//! Exposes the v2 control-plane surface for:
//! - Runtime capability discovery (`/runtime-capabilities`)
//! - Connector package ingestion, admission & revocation (`/connector-packages`)
//! - Immutable connection revisions & health probes (`/connections`)
//! - Candidate deployment planning, idempotent CAS apply (`202 Accepted`),
//!   convergence polling (`?target=this-node|observed`), and rollback by new revision (`/deployments`)
//! - Legacy v1 -> v2 migration report & bootstrap manifest validation (`/migration-report`)
//! - Provider package authoring scaffold & lint (`/authoring/*`)

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::extensions::authoring::{
    lint_provider_package, pack_provider_package, scaffold_provider_package,
};
use crate::extensions::deployment_store::{
    ArchiveEntryInput, CanonicalManifest, DeploymentError, ImmutableDeploymentStore,
};
use crate::extensions::reconciler::ConvergenceTarget;
use crate::extensions::{
    AdmittedRun, AuthorityGrant, BudgetLimits, ConnectionReference, EditionTier,
    EffectiveConnectionIdentity, ImplementationKind, ProviderReference, check_connector_availability,
    map_legacy_connector,
};
use crate::server::admin_auth::AdminPrincipal;
use crate::server::state::AppState;

pub fn v2_admin_routes(max_admin_body_size: usize) -> Router<AppState> {
    Router::new()
        .route("/runtime-capabilities", get(get_runtime_capabilities))
        .route(
            "/connector-packages",
            get(list_connector_packages).post(upload_connector_package),
        )
        .route(
            "/connector-packages/{digest}/admit",
            post(admit_connector_package),
        )
        .route(
            "/connector-packages/{digest}/revoke",
            post(revoke_connector_package),
        )
        .route(
            "/connections",
            get(list_connections).post(create_connection_revision),
        )
        .route("/connections/{id}/probe", post(probe_connection))
        .route("/deployment-plans", post(create_deployment_plan))
        .route(
            "/deployments",
            get(get_deployments_summary).post(apply_deployment),
        )
        .route(
            "/deployments/{revision}/convergence",
            get(get_deployment_convergence),
        )
        .route("/deployments/rollback", post(rollback_deployment))
        .route("/migration-report", post(create_migration_report))
        .route("/authoring/scaffold", post(authoring_scaffold))
        .route("/authoring/lint", post(authoring_lint))
        .layer(DefaultBodyLimit::max(max_admin_body_size))
}

fn deployment_err_response(err: DeploymentError) -> (StatusCode, Json<Value>) {
    let status = match err.code.as_str() {
        "PACKAGE_NOT_FOUND"
        | "ADMISSION_NOT_FOUND"
        | "BASE_REVISION_NOT_FOUND"
        | "ROLLBACK_TARGET_NOT_FOUND" => StatusCode::NOT_FOUND,
        "PUBLISHER_VERSION_CONFLICT"
        | "IMMUTABLE_CONTENT_CONFLICT"
        | "IMMUTABLE_CONNECTION_CONFLICT"
        | "IDEMPOTENCY_KEY_CONFLICT"
        | "DEPLOYMENT_HEAD_CONFLICT" => StatusCode::CONFLICT,
        "PROVIDER_NOT_ADMITTED"
        | "PROVIDER_REVOKED"
        | "ADMISSION_INVALIDATED"
        | "BACKFILL_ENTERPRISE_PROVIDER_REQUIRED" => StatusCode::FORBIDDEN,
        _ => StatusCode::BAD_REQUEST,
    };
    (
        status,
        Json(json!({
            "error": {
                "code": err.code,
                "message": err.message,
            }
        })),
    )
}

fn principal_id(ext: Option<Extension<AdminPrincipal>>) -> String {
    ext.map(|Extension(p)| p.key_id)
        .unwrap_or_else(|| "anonymous-admin".to_string())
}

/// `GET /api/v2/admin/runtime-capabilities` (`V2-07.5`).
async fn get_runtime_capabilities(State(state): State<AppState>) -> impl IntoResponse {
    let comp = &state.composition;
    let (head_rev, epoch) = comp.deployments.current_head();
    let local_status = comp.reconciler.local_status();
    let providers: Vec<Value> = comp
        .providers
        .registered_drivers()
        .into_iter()
        .map(|(kind, id)| {
            json!({
                "kind": kind.as_str(),
                "driver_id": id,
            })
        })
        .collect();

    Json(json!({
        "api_version": "v2",
        "edition": "community",
        "schema_profiles": ["orion-json-schema/1"],
        "wasm_worlds": ["orion:connector@2.0.0", "orion:plugin@1.0.0"],
        "wasm_import_profiles": ["core-v2"],
        "channel_drivers": comp.channels.protocol_ids(),
        "provider_backends": providers,
        "admission_policy_id": comp.admission.policy_id(),
        "deployment_head": {
            "revision": head_rev,
            "epoch": epoch,
        },
        "reconciler": {
            "applied_revision": local_status.applied_revision,
            "backoff_attempt": local_status.backoff_attempt,
            "quarantined_entities": local_status.quarantined_entities,
        },
        "native_scheduler": {
            "in_flight": comp.native_scheduler.in_flight(),
            "max_in_flight": comp.native_scheduler.max_in_flight(),
        }
    }))
}

#[derive(Debug, Deserialize)]
pub struct UploadPackageRequest {
    /// Map of relative path -> UTF-8 or `base64:<...>` file content.
    pub files: BTreeMap<String, String>,
}

fn decode_files_map(raw: &BTreeMap<String, String>) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut out = BTreeMap::new();
    for (path, content) in raw {
        let bytes = if let Some(b64) = content.strip_prefix("base64:") {
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| format!("invalid base64 for '{path}': {e}"))?
        } else {
            content.as_bytes().to_vec()
        };
        out.insert(path.clone(), bytes);
    }
    Ok(out)
}

/// `GET /api/v2/admin/connector-packages` (`V2-07.1`).
async fn list_connector_packages(State(state): State<AppState>) -> impl IntoResponse {
    let packages = state.composition.deployments.list_packages();
    let admissions = state.composition.deployments.list_admissions();
    Json(json!({
        "packages": packages,
        "admissions": admissions,
    }))
}

/// `POST /api/v2/admin/connector-packages` (`V2-07.1`).
async fn upload_connector_package(
    State(state): State<AppState>,
    Json(req): Json<UploadPackageRequest>,
) -> impl IntoResponse {
    let mut files = match decode_files_map(&req.files) {
        Ok(f) => f,
        Err(msg) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": { "code": "INVALID_PACKAGE_ENCODING", "message": msg } })),
            );
        }
    };
    if !files.contains_key("inventory.json") {
        if let Ok(packed) = pack_provider_package(&files) {
            files.insert("inventory.json".to_string(), packed.inventory_bytes);
        }
    }
    let entries: Vec<ArchiveEntryInput> = files
        .into_iter()
        .map(|(path, bytes)| ArchiveEntryInput {
            path,
            compressed_size: bytes.len(),
            bytes,
            is_symlink: false,
        })
        .collect();

    match state
        .composition
        .deployments
        .ingest_verified_archive(&entries)
    {
        Ok(row) => (StatusCode::CREATED, Json(json!({ "package": row }))),
        Err(e) => deployment_err_response(e),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct AdmitPackageRequest {
    pub lock: Option<Value>,
}

/// `POST /api/v2/admin/connector-packages/{digest}/admit` (`V2-07.1`).
async fn admit_connector_package(
    State(state): State<AppState>,
    Path(digest): Path<String>,
    req: Option<Json<AdmitPackageRequest>>,
) -> impl IntoResponse {
    let lock = req.and_then(|Json(r)| r.lock);
    match state.composition.deployments.admit_package(
        &digest,
        state.composition.admission.as_ref(),
        lock.as_ref(),
    ) {
        Ok(adm) => (StatusCode::OK, Json(json!({ "admission": adm }))),
        Err(e) => deployment_err_response(e),
    }
}

/// `POST /api/v2/admin/connector-packages/{digest}/revoke` (`V2-07.1`).
async fn revoke_connector_package(
    State(state): State<AppState>,
    Path(digest): Path<String>,
) -> impl IntoResponse {
    match state.composition.deployments.revoke_package(&digest) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "package_digest": digest, "revoked": true })),
        ),
        Err(e) => deployment_err_response(e),
    }
}

/// `GET /api/v2/admin/connections` (`V2-07.1`).
async fn list_connections(State(state): State<AppState>) -> impl IntoResponse {
    let connections = state.composition.deployments.list_connections();
    Json(json!({ "connections": connections }))
}

#[derive(Debug, Deserialize)]
pub struct CreateConnectionRevisionRequest {
    pub connection_id: String,
    pub revision: u64,
    pub provider_digest: String,
    #[serde(default)]
    pub public_config: Value,
    pub secret_locator: Option<String>,
}

/// `POST /api/v2/admin/connections` (`V2-07.1`).
async fn create_connection_revision(
    State(state): State<AppState>,
    Json(req): Json<CreateConnectionRevisionRequest>,
) -> impl IntoResponse {
    match state.composition.deployments.store_connection_revision(
        &req.connection_id,
        req.revision,
        &req.provider_digest,
        req.public_config,
        req.secret_locator,
    ) {
        Ok(row) => (StatusCode::CREATED, Json(json!({ "connection": row }))),
        Err(e) => deployment_err_response(e),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct ProbeConnectionRequest {
    pub driver_id: Option<String>,
    pub kind: Option<String>,
}

/// `POST /api/v2/admin/connections/{id}/probe` (`V2-07.1`).
async fn probe_connection(
    State(state): State<AppState>,
    Path(connection_id): Path<String>,
    req: Option<Json<ProbeConnectionRequest>>,
) -> impl IntoResponse {
    let req = req.map(|Json(r)| r).unwrap_or_default();
    let kind = match req.kind.as_deref().unwrap_or("rust") {
        "workflow" => ImplementationKind::Workflow,
        "wasm" => ImplementationKind::Wasm,
        _ => ImplementationKind::Rust,
    };
    let driver_id = req.driver_id.as_deref().unwrap_or(&connection_id);
    let Some(backend) = state.composition.providers.get(kind, driver_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": {
                    "code": "PROVIDER_NOT_FOUND",
                    "message": format!("provider ({}, '{driver_id}') is not registered", kind.as_str())
                }
            })),
        );
    };

    let mut allowed_operations = BTreeMap::new();
    allowed_operations.insert(
        connection_id.clone(),
        ["probe".to_string()].into_iter().collect(),
    );
    let grant = AuthorityGrant {
        allowed_operations,
        declared_dependencies: BTreeMap::new(),
        allowed_channels: BTreeSet::new(),
    };
    let admitted = AdmittedRun::admit(
        format!("probe-{connection_id}"),
        1,
        Instant::now() + Duration::from_secs(5),
        BudgetLimits::default(),
        grant,
    );
    let run_ctx = admitted.context();
    let conn_ref = ConnectionReference {
        connection_id: connection_id.clone(),
        revision: 1,
        provider: ProviderReference {
            provider_id: driver_id.to_string(),
            version: "2.0.0".to_string(),
            package_digest:
                "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                    .to_string(),
        },
    };
    let eff = EffectiveConnectionIdentity {
        provider_digest: conn_ref.provider.package_digest.clone(),
        connection_id: connection_id.clone(),
        revision: 1,
        secret_hash: "sha256:probe".to_string(),
    };
    let Ok(inv) = run_ctx.start_invocation(conn_ref, eff, "probe", None, 1) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": { "code": "PROBE_INIT_FAILED" } })),
        );
    };
    let Ok(attempt) = inv.next_attempt() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": { "code": "PROBE_ATTEMPT_FAILED" } })),
        );
    };

    match backend.health_check(&attempt).await {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({
                "connection_id": connection_id,
                "driver_id": driver_id,
                "healthy": true,
            })),
        ),
        Err(err) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "connection_id": connection_id,
                "driver_id": driver_id,
                "healthy": false,
                "error": err,
            })),
        ),
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateDeploymentPlanRequest {
    pub base_revision: u64,
    pub manifest: CanonicalManifest,
}

/// `POST /api/v2/admin/deployment-plans` (`V2-07.4`, `V2-04.3`).
async fn create_deployment_plan(
    State(state): State<AppState>,
    Json(req): Json<CreateDeploymentPlanRequest>,
) -> impl IntoResponse {
    match state
        .composition
        .deployments
        .plan_candidate(req.base_revision, req.manifest)
    {
        Ok(plan) => (
            StatusCode::OK,
            Json(json!({
                "base_revision": plan.base_revision,
                "candidate_digest": plan.candidate_digest,
                "added_entities": plan.added_entities,
                "removed_entities": plan.removed_entities,
                "external_effect_warnings": plan.external_effect_warnings,
            })),
        ),
        Err(e) => deployment_err_response(e),
    }
}

/// `GET /api/v2/admin/deployments` (`V2-07.4`).
async fn get_deployments_summary(State(state): State<AppState>) -> impl IntoResponse {
    let store = &state.composition.deployments;
    let (head_rev, epoch) = store.current_head();
    let local_status = state.composition.reconciler.local_status();
    Json(json!({
        "head_revision": head_rev,
        "epoch": epoch,
        "applied_revision": local_status.applied_revision,
        "revisions": store.list_revisions(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct ApplyDeploymentRequest {
    pub idempotency_key: String,
    pub base_revision: u64,
    pub manifest: CanonicalManifest,
    pub reason: String,
}

/// `POST /api/v2/admin/deployments` (`V2-07.4`, `V2-04.4`).
///
/// Commits the candidate manifest under CAS on `base_revision` and returns
/// `202 Accepted` with the durable `DeploymentReceipt`.
async fn apply_deployment(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Json(req): Json<ApplyDeploymentRequest>,
) -> impl IntoResponse {
    let actor = principal_id(principal);
    let plan = match state
        .composition
        .deployments
        .plan_candidate(req.base_revision, req.manifest)
    {
        Ok(p) => p,
        Err(e) => return deployment_err_response(e),
    };

    let receipt = match state.composition.deployments.apply_candidate(
        &actor,
        &req.idempotency_key,
        &plan,
        &req.reason,
        state.composition.admission.as_ref(),
    ) {
        Ok(r) => r,
        Err(e) => return deployment_err_response(e),
    };

    // Reconcile this node against the newly committed revision
    state.composition.reconciler.observe_desired_head();
    let _ = state
        .composition
        .reconciler
        .build_and_publish_candidate(receipt.committed_revision, Vec::new());

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            "receipt": receipt,
        })),
    )
}

#[derive(Debug, Default, Deserialize)]
pub struct ConvergenceQuery {
    pub target: Option<String>,
}

/// `GET /api/v2/admin/deployments/{revision}/convergence` (`V2-07.4`, `V2-06.4`).
async fn get_deployment_convergence(
    State(state): State<AppState>,
    Path(revision): Path<u64>,
    Query(query): Query<ConvergenceQuery>,
) -> impl IntoResponse {
    let local_node_id = state.composition.reconciler.local_status().identity.node_id;
    let target = match query.target.as_deref().unwrap_or("this-node") {
        "observed" => ConvergenceTarget::ObservedNodes([local_node_id].into_iter().collect()),
        _ => ConvergenceTarget::ThisNode(local_node_id),
    };
    let report = state
        .composition
        .reconciler
        .evaluate_convergence(revision, &target);
    (StatusCode::OK, Json(serde_json::to_value(report).unwrap()))
}

#[derive(Debug, Deserialize)]
pub struct RollbackDeploymentRequest {
    pub idempotency_key: String,
    pub expected_head_revision: u64,
    pub target_revision: u64,
    pub reason: String,
}

/// `POST /api/v2/admin/deployments/rollback` (`V2-07.4`, `V2-04.5`).
async fn rollback_deployment(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Json(req): Json<RollbackDeploymentRequest>,
) -> impl IntoResponse {
    let actor = principal_id(principal);
    match state.composition.deployments.rollback_to_revision(
        &actor,
        &req.idempotency_key,
        req.expected_head_revision,
        req.target_revision,
        &req.reason,
        state.composition.admission.as_ref(),
    ) {
        Ok((receipt, warnings)) => {
            state.composition.reconciler.observe_desired_head();
            let _ = state
                .composition
                .reconciler
                .build_and_publish_candidate(receipt.committed_revision, Vec::new());
            (
                StatusCode::ACCEPTED,
                Json(json!({
                    "status": "accepted",
                    "receipt": receipt,
                    "warnings": warnings,
                })),
            )
        }
        Err(e) => deployment_err_response(e),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct MigrationReportRequest {
    #[serde(default)]
    pub connectors: Vec<LegacyConnectorItem>,
    #[serde(default)]
    pub workflows: Vec<LegacyWorkflowItem>,
    #[serde(default)]
    pub channels: Vec<LegacyChannelItem>,
}

#[derive(Debug, Deserialize)]
pub struct LegacyConnectorItem {
    pub name: String,
    pub connector_type: String,
    #[serde(default)]
    pub config: Value,
}

#[derive(Debug, Deserialize)]
pub struct LegacyWorkflowItem {
    pub id: String,
    pub version: u64,
    #[serde(default)]
    pub definition: Value,
}

#[derive(Debug, Deserialize)]
pub struct LegacyChannelItem {
    pub id: String,
    pub version: u64,
    pub workflow_id: String,
    pub workflow_version: u64,
}

/// `POST /api/v2/admin/migration-report` (`V2-07.3`, `V2-14.5`, `V2-03.5`).
async fn create_migration_report(
    State(state): State<AppState>,
    Json(req): Json<MigrationReportRequest>,
) -> impl IntoResponse {
    let mut findings = Vec::new();
    let mut resolved_connectors = Vec::new();

    for c in &req.connectors {
        match map_legacy_connector(&c.name, &c.connector_type, &c.config).and_then(|res| {
            check_connector_availability(&state.composition, &c.name, &res).map(|()| res)
        }) {
            Ok(res) => resolved_connectors.push(json!({
                "name": c.name,
                "provider_id": res.provider_id,
                "implementation": res.implementation.as_str(),
                "tier": res.tier.as_str(),
            })),
            Err(finding) => findings.push(finding),
        }
    }

    let enriched_conns: Vec<(String, Value)> = req
        .connectors
        .iter()
        .map(|c| {
            let mut v = c.config.clone();
            if let Some(obj) = v.as_object_mut() {
                obj.entry("connector_type".to_string())
                    .or_insert_with(|| Value::String(c.connector_type.clone()));
            }
            (c.name.clone(), v)
        })
        .collect();
    let conn_tuples: Vec<(&str, &Value)> = enriched_conns
        .iter()
        .map(|(n, cfg)| (n.as_str(), cfg))
        .collect();
    let wf_tuples: Vec<(&str, u64, &Value)> = req
        .workflows
        .iter()
        .map(|w| (w.id.as_str(), w.version, &w.definition))
        .collect();
    let ch_tuples: Vec<(&str, u64, &str, u64)> = req
        .channels
        .iter()
        .map(|ch| {
            (
                ch.id.as_str(),
                ch.version,
                ch.workflow_id.as_str(),
                ch.workflow_version,
            )
        })
        .collect();

    let bootstrap_manifest = if findings.is_empty() {
        ImmutableDeploymentStore::build_legacy_bootstrap_manifest(
            EditionTier::Community,
            &conn_tuples,
            &wf_tuples,
            &ch_tuples,
        )
        .ok()
    } else {
        None
    };

    (
        StatusCode::OK,
        Json(json!({
            "compatible": findings.is_empty() && bootstrap_manifest.is_some(),
            "resolved_connectors": resolved_connectors,
            "findings": findings,
            "bootstrap_manifest": bootstrap_manifest,
        })),
    )
}

#[derive(Debug, Deserialize)]
pub struct ScaffoldRequest {
    pub publisher: String,
    pub provider_id: String,
    pub version: String,
    pub kind: ImplementationKind,
}

/// `POST /api/v2/admin/authoring/scaffold` (`V2-07.2`).
async fn authoring_scaffold(Json(req): Json<ScaffoldRequest>) -> impl IntoResponse {
    let pkg = scaffold_provider_package(&req.publisher, &req.provider_id, &req.version, req.kind);
    let inv = pack_provider_package(&pkg.files).unwrap();
    let files_utf8: BTreeMap<String, String> = pkg
        .files
        .into_iter()
        .map(|(k, v)| {
            let s = String::from_utf8(v.clone()).unwrap_or_else(|_| {
                format!(
                    "base64:{}",
                    base64::engine::general_purpose::STANDARD.encode(&v)
                )
            });
            (k, s)
        })
        .collect();
    (
        StatusCode::OK,
        Json(json!({
            "publisher": pkg.publisher,
            "provider_id": pkg.provider_id,
            "version": pkg.version,
            "implementation": pkg.implementation.as_str(),
            "package_digest": inv.package_digest,
            "files": files_utf8,
        })),
    )
}

/// `POST /api/v2/admin/authoring/lint` (`V2-07.2`).
async fn authoring_lint(Json(req): Json<UploadPackageRequest>) -> impl IntoResponse {
    let files = match decode_files_map(&req.files) {
        Ok(f) => f,
        Err(msg) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "valid": false, "errors": [msg] })),
            );
        }
    };
    let report = lint_provider_package(&files);
    let status = if report.valid {
        StatusCode::OK
    } else {
        StatusCode::UNPROCESSABLE_ENTITY
    };
    (status, Json(serde_json::to_value(report).unwrap()))
}
