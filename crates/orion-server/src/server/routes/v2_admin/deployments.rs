//! V2 Candidate deployment planning, CAS apply, convergence & rollback routes (`/api/v2/admin/deployment*`).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Extension, Json};
use orion_api::v2::{
    ApplyDeploymentRequest, ApplyDeploymentResponse, ConvergenceStatusResponse,
    CreateDeploymentPlanRequest, DeploymentPlanResponse, DeploymentReceiptResponse,
    DeploymentRevisionResponse, DeploymentsSummaryResponse, RollbackDeploymentRequest,
    RollbackDeploymentResponse,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{deployment_err_response, principal_id};
use crate::extensions::deployment_store::{
    CanonicalManifest, DeploymentError, DeploymentErrorKind, DeploymentReceipt,
    DeploymentRevisionRow,
};
use crate::extensions::reconciler::ConvergenceTarget;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::state::AppState;

#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ConvergenceQuery {
    /// Convergence evaluation scope (`this-node` or `observed`).
    pub target: Option<String>,
}

/// A stored time as the wire carries it: ISO 8601, as every other admin
/// timestamp serialises; empty for a value never read back.
fn wire_time(at: Option<chrono::NaiveDateTime>) -> String {
    at.map(|t| t.format("%Y-%m-%dT%H:%M:%S%.f").to_string())
        .unwrap_or_default()
}

fn receipt_to_dto(receipt: &DeploymentReceipt) -> DeploymentReceiptResponse {
    DeploymentReceiptResponse {
        operation_id: receipt.operation_id.clone(),
        idempotency_key: receipt.idempotency_key.clone(),
        principal_id: receipt.principal.clone(),
        principal: receipt.principal.clone(),
        request_hash: receipt.request_hash.clone(),
        committed_revision: receipt.committed_revision,
        manifest_digest: receipt.manifest_digest.clone(),
        epoch: receipt.epoch,
        receiving_node_id: receipt.receiving_node_id.clone(),
        replayed: false,
        created_at: wire_time(receipt.created_at),
    }
}

fn revision_row_to_dto(row: &DeploymentRevisionRow) -> DeploymentRevisionResponse {
    DeploymentRevisionResponse {
        revision: row.revision,
        parent_revision: row.parent_revision,
        manifest_digest: row.manifest_digest.clone(),
        manifest: serde_json::to_value(&row.manifest).unwrap_or(Value::Null),
        reason: row.reason.clone(),
        created_by: row.created_by.clone(),
        members_compacted: row.members_compacted,
        created_at: wire_time(row.created_at),
    }
}

/// `POST /api/v2/admin/deployment-plans`.
#[utoipa::path(
    post,
    path = "/api/v2/admin/deployment-plans",
    tag = "Deployments",
    summary = "Plan and validate a candidate deployment manifest",
    description = "Validates canonical manifest invariants, provider admission status, and connection references against the requested base revision, returning the reviewed manifest digest and entity diff.",
    request_body = CreateDeploymentPlanRequest,
    responses(
        (status = 200, description = "Candidate deployment plan", body = DeploymentPlanResponse),
        (status = 400, description = "Manifest validation failed"),
        (status = 403, description = "Referenced provider package not admitted or revoked"),
        (status = 404, description = "Base revision not found"),
    )
)]
pub(crate) async fn create_deployment_plan(
    State(state): State<AppState>,
    Json(req): Json<CreateDeploymentPlanRequest>,
) -> impl IntoResponse {
    let manifest = match CanonicalManifest::from_json(&req.manifest) {
        Ok(m) => m,
        Err(e) => return deployment_err_response(e),
    };
    let plan = state
        .deployments
        .plan_candidate_in_db(&state.db_pool, req.base_revision, manifest)
        .await;
    if let Ok(ref plan) = plan
        && let Err(e) = refuse_unbuildable(&state, plan).await
    {
        return deployment_err_response(e);
    }
    match plan {
        Ok(plan) => {
            let added_values: Vec<Value> = plan
                .added_entities
                .iter()
                .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
                .collect();
            let removed_values: Vec<Value> = plan
                .removed_entities
                .iter()
                .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
                .collect();
            let findings: Vec<Value> = plan
                .external_effect_warnings
                .iter()
                .map(|w| json!({ "warning": w }))
                .collect();
            let request_hash = plan
                .compute_request_hash("deployment_plan", "")
                .unwrap_or_else(|_| plan.candidate_digest.clone());
            let resp = DeploymentPlanResponse {
                base_revision: plan.base_revision,
                candidate_digest: plan.candidate_digest.clone(),
                reviewed_manifest_digest: plan.candidate_digest,
                request_hash,
                added_entities: added_values,
                updated_entities: Vec::new(),
                removed_entities: removed_values,
                external_effect_warnings: plan.external_effect_warnings,
                findings,
            };
            (
                StatusCode::OK,
                Json(serde_json::to_value(resp).unwrap_or(Value::Null)),
            )
        }
        Err(e) => deployment_err_response(e),
    }
}

/// `GET /api/v2/admin/deployments`.
#[utoipa::path(
    get,
    path = "/api/v2/admin/deployments",
    tag = "Deployments",
    summary = "Get deployment head and committed revision history",
    description = "Returns the current deployment store head revision, epoch, node-applied revision, and committed revision rows.",
    responses(
        (status = 200, description = "Deployment summary", body = DeploymentsSummaryResponse),
    )
)]
pub(crate) async fn get_deployments_summary(State(state): State<AppState>) -> impl IntoResponse {
    let store = &state.deployments;
    let (head_rev, epoch) = store.current_head();
    let local_status = state.reconciler.local_status();
    let revisions = store
        .list_revisions()
        .iter()
        .map(|r| revision_row_to_dto(r))
        .collect();
    Json(DeploymentsSummaryResponse {
        head_revision: head_rev,
        epoch,
        applied_revision: local_status.applied_revision,
        revisions,
    })
}

/// `POST /api/v2/admin/deployments`.
///
/// Commits the candidate manifest under CAS on `base_revision` and returns
/// `202 Accepted` with the durable `DeploymentReceipt`.
#[utoipa::path(
    post,
    path = "/api/v2/admin/deployments",
    tag = "Deployments",
    summary = "Atomically apply a candidate deployment manifest",
    description = "Validates the candidate manifest (and optional reviewed_manifest_digest), performs compare-and-swap against base_revision in a single database transaction, and reconciles the local node.",
    request_body = ApplyDeploymentRequest,
    responses(
        (status = 202, description = "Deployment committed and accepted for convergence", body = ApplyDeploymentResponse),
        (status = 400, description = "Manifest validation failed"),
        (status = 403, description = "Provider not admitted or admission invalidated"),
        (status = 409, description = "Base revision conflict, idempotency conflict, or reviewed digest mismatch"),
    )
)]
pub(crate) async fn apply_deployment(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Json(req): Json<ApplyDeploymentRequest>,
) -> impl IntoResponse {
    let actor = principal_id(&principal);
    let manifest = match CanonicalManifest::from_json(&req.manifest) {
        Ok(m) => m,
        Err(e) => return deployment_err_response(e),
    };
    let plan = match state
        .deployments
        .plan_candidate_in_db(&state.db_pool, req.base_revision, manifest)
        .await
    {
        Ok(p) => p,
        Err(e) => return deployment_err_response(e),
    };

    if let Err(e) = refuse_unbuildable(&state, &plan).await {
        return deployment_err_response(e);
    }

    if let Some(ref expected_digest) = req.reviewed_manifest_digest
        && expected_digest != &plan.candidate_digest
    {
        return deployment_err_response(DeploymentError::new(
            DeploymentErrorKind::ReviewedDigestMismatch,
            format!(
                "reviewed_manifest_digest '{expected_digest}' does not match candidate manifest digest '{}'",
                plan.candidate_digest
            ),
        ));
    }

    // The audit row is written in the commit's transaction: it lands with
    // the head move or not at all, and a replay that commits nothing writes
    // none.
    let receipt = match state
        .deployments
        .apply_candidate_audited_in_db(
            &state.db_pool,
            &actor,
            &req.idempotency_key,
            &plan,
            &req.reason,
            state.composition.admission(),
            |receipt| {
                crate::server::routes::admin::audit_record(
                    &principal,
                    "v2_deployment_apply",
                    "deployment",
                    &receipt.committed_revision.to_string(),
                )
            },
        )
        .await
    {
        Ok((r, recorded)) => {
            if let Some(event) = recorded {
                crate::server::routes::admin::emit_audit_trail(&event);
            }
            r
        }
        Err(e) => return deployment_err_response(e),
    };

    let (evaluation, local_error) = reconcile_locally(&state, receipt.committed_revision).await;
    let resp = ApplyDeploymentResponse {
        status: "accepted".to_string(),
        committed_revision: receipt.committed_revision,
        receipt: receipt_to_dto(&receipt),
        evaluation: Some(evaluation),
        local_error,
    };
    (
        StatusCode::ACCEPTED,
        Json(serde_json::to_value(resp).unwrap_or(Value::Null)),
    )
}

/// Refuse a candidate this node could not build, before anything is
/// committed: a head no node can serve would otherwise be committed, leave
/// every node on the previous revision and read as up to date to the next
/// apply.
async fn refuse_unbuildable(
    state: &AppState,
    plan: &crate::extensions::deployment_store::CandidatePlan,
) -> Result<(), DeploymentError> {
    crate::runtime::check_candidate(state, &plan.candidate_manifest)
        .await
        .map_err(|e| {
            DeploymentError::new(DeploymentErrorKind::DeploymentWouldNotBuild, e.to_string())
        })
}

/// Build `revision` on this node, and say how that went: the node's
/// convergence on it, and the error when it could not be built. The
/// revision is committed either way; a node that cannot build it keeps
/// serving what it had, and its reconciler retries.
async fn reconcile_locally(
    state: &AppState,
    revision: u64,
) -> (ConvergenceStatusResponse, Option<String>) {
    let local_error = match crate::runtime::reconcile_deployment_revision(state, revision).await {
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(error = %e, revision, "this node could not build the committed deployment revision");
            Some(e.to_string())
        }
    };
    let node = state.reconciler.local_status().identity.node_id;
    let report = state
        .reconciler
        .evaluate_convergence(revision, &ConvergenceTarget::ThisNode(node));
    let evaluation = ConvergenceStatusResponse {
        converged: report.converged,
        target_revision: report.target_revision,
        converged_nodes: report.converged_nodes,
        pending_nodes: report.pending_nodes,
        degraded_nodes: report.degraded_nodes,
    };
    (evaluation, local_error)
}

/// `GET /api/v2/admin/deployments/{revision}`.
#[utoipa::path(
    get,
    path = "/api/v2/admin/deployments/{revision}",
    tag = "Deployments",
    summary = "Get a committed deployment revision",
    description = "Returns the canonical manifest and metadata for a committed deployment revision.",
    params(
        ("revision" = u64, Path, description = "Committed deployment revision number")
    ),
    responses(
        (status = 200, description = "Committed deployment revision", body = DeploymentRevisionResponse),
        (status = 404, description = "Revision not found"),
    )
)]
pub(crate) async fn get_deployment_revision(
    State(state): State<AppState>,
    Path(revision): Path<u64>,
) -> impl IntoResponse {
    let revisions = state.deployments.list_revisions();
    match revisions.into_iter().find(|r| r.revision == revision) {
        Some(rev) => {
            let dto = revision_row_to_dto(&rev);
            let mut val = serde_json::to_value(&dto).unwrap_or(Value::Null);
            if let Some(obj) = val.as_object_mut() {
                obj.insert(
                    "data".to_string(),
                    serde_json::to_value(&dto).unwrap_or(Value::Null),
                );
            }
            (StatusCode::OK, Json(val))
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(crate::errors::error_envelope_json(
                "REVISION_NOT_FOUND",
                format!("deployment revision {revision} not found"),
            )),
        ),
    }
}

/// `GET /api/v2/admin/deployments/{revision}/convergence`.
#[utoipa::path(
    get,
    path = "/api/v2/admin/deployments/{revision}/convergence",
    tag = "Deployments",
    summary = "Evaluate node or cluster convergence for a deployment revision",
    description = "Reports whether the target node (`?target=this-node`) or all observed cluster replicas (`?target=observed`) have converged to the specified deployment revision.",
    params(
        ("revision" = u64, Path, description = "Target deployment revision"),
        ConvergenceQuery,
    ),
    responses(
        (status = 200, description = "Convergence evaluation report", body = ConvergenceStatusResponse),
    )
)]
pub(crate) async fn get_deployment_convergence(
    State(state): State<AppState>,
    Path(revision): Path<u64>,
    Query(query): Query<ConvergenceQuery>,
) -> impl IntoResponse {
    let local_node_id = state.reconciler.local_status().identity.node_id;
    let target = match query.target.as_deref().unwrap_or("this-node") {
        "observed" => {
            let observed = state
                .reconciler
                .refresh_observed_nodes_from_db_pool(&state.db_pool)
                .await
                .unwrap_or_else(|_| state.reconciler.observed_node_ids());
            ConvergenceTarget::ObservedNodes(observed)
        }
        _ => ConvergenceTarget::ThisNode(local_node_id),
    };
    let report = state.reconciler.evaluate_convergence(revision, &target);
    let dto = ConvergenceStatusResponse {
        converged: report.converged,
        target_revision: report.target_revision,
        converged_nodes: report.converged_nodes,
        pending_nodes: report.pending_nodes,
        degraded_nodes: report.degraded_nodes,
    };
    (StatusCode::OK, Json(dto))
}

/// `POST /api/v2/admin/deployments/rollback`.
#[utoipa::path(
    post,
    path = "/api/v2/admin/deployments/rollback",
    tag = "Deployments",
    summary = "Roll back to a prior deployment revision",
    description = "Creates a new forward deployment revision whose manifest matches target_revision under compare-and-swap against expected_head_revision, then reconciles the local node.",
    request_body = RollbackDeploymentRequest,
    responses(
        (status = 202, description = "Rollback committed as a new deployment revision", body = RollbackDeploymentResponse),
        (status = 403, description = "Provider in target revision is no longer admitted"),
        (status = 404, description = "Target revision not found"),
        (status = 409, description = "Expected head revision conflict or idempotency conflict"),
    )
)]
pub(crate) async fn rollback_deployment(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Json(req): Json<RollbackDeploymentRequest>,
) -> impl IntoResponse {
    let actor = principal_id(&principal);
    match state
        .deployments
        .rollback_to_revision_audited_in_db(
            &state.db_pool,
            &crate::extensions::deployment_store::Rollback {
                principal: &actor,
                idempotency_key: &req.idempotency_key,
                expected_head_revision: req.expected_head_revision,
                target_revision: req.target_revision,
                reason: &req.reason,
            },
            state.composition.admission(),
            // In the commit's transaction, as an apply's is.
            |receipt| {
                crate::server::routes::admin::audit_record(
                    &principal,
                    "v2_deployment_rollback",
                    "deployment",
                    &format!("{}->{}", req.target_revision, receipt.committed_revision),
                )
            },
        )
        .await
    {
        Ok(((receipt, warnings), recorded)) => {
            if let Some(event) = recorded {
                crate::server::routes::admin::emit_audit_trail(&event);
            }
            let (evaluation, local_error) =
                reconcile_locally(&state, receipt.committed_revision).await;
            let resp = RollbackDeploymentResponse {
                status: "accepted".to_string(),
                committed_revision: receipt.committed_revision,
                receipt: receipt_to_dto(&receipt),
                warnings,
                evaluation: Some(evaluation),
                local_error,
            };
            (
                StatusCode::ACCEPTED,
                Json(serde_json::to_value(resp).unwrap_or(Value::Null)),
            )
        }
        Err(e) => deployment_err_response(e),
    }
}
