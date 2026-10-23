//! V2 Candidate deployment planning, CAS apply, convergence & rollback routes (`/api/v2/admin/deployment*`).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{deployment_err_response, principal_id, sync_v2_deployments_to_db};
use crate::extensions::deployment_store::CanonicalManifest;
use crate::extensions::reconciler::ConvergenceTarget;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::state::AppState;

#[derive(Debug, Deserialize)]
pub struct CreateDeploymentPlanRequest {
    pub base_revision: u64,
    pub manifest: CanonicalManifest,
}

#[derive(Debug, Deserialize)]
pub struct ApplyDeploymentRequest {
    pub idempotency_key: String,
    pub base_revision: u64,
    pub manifest: CanonicalManifest,
    pub reason: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct ConvergenceQuery {
    pub target: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RollbackDeploymentRequest {
    pub idempotency_key: String,
    pub expected_head_revision: u64,
    pub target_revision: u64,
    pub reason: String,
}

/// `POST /api/v2/admin/deployment-plans` (`V2-07.4`, `V2-04.3`).
pub(super) async fn create_deployment_plan(
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
pub(super) async fn get_deployments_summary(State(state): State<AppState>) -> impl IntoResponse {
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

/// `POST /api/v2/admin/deployments` (`V2-07.4`, `V2-04.4`).
///
/// Commits the candidate manifest under CAS on `base_revision` and returns
/// `202 Accepted` with the durable `DeploymentReceipt`.
pub(super) async fn apply_deployment(
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

    // Reconcile this node against the newly committed revision and reload RuntimeGeneration
    sync_v2_deployments_to_db(&state).await;
    state.composition.reconciler.observe_desired_head();
    let _ = state
        .composition
        .reconciler
        .build_and_publish_candidate(receipt.committed_revision, Vec::new());
    let _ = crate::runtime::reload_engine(&state).await;

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            "receipt": receipt,
        })),
    )
}

/// `GET /api/v2/admin/deployments/{revision}` (`V2-07.4`).
pub(super) async fn get_deployment_revision(
    State(state): State<AppState>,
    Path(revision): Path<u64>,
) -> impl IntoResponse {
    let revisions = state.composition.deployments.list_revisions();
    match revisions.into_iter().find(|r| r.revision == revision) {
        Some(rev) => (StatusCode::OK, Json(json!({ "data": rev }))),
        None => (
            StatusCode::NOT_FOUND,
            Json(crate::errors::error_envelope_json(
                "REVISION_NOT_FOUND",
                format!("deployment revision {revision} not found"),
            )),
        ),
    }
}

/// `GET /api/v2/admin/deployments/{revision}/convergence` (`V2-07.4`, `V2-06.4`).
pub(super) async fn get_deployment_convergence(
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
    (
        StatusCode::OK,
        Json(serde_json::to_value(report).unwrap_or(Value::Null)),
    )
}

/// `POST /api/v2/admin/deployments/rollback` (`V2-07.4`, `V2-04.5`).
pub(super) async fn rollback_deployment(
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
            sync_v2_deployments_to_db(&state).await;
            state.composition.reconciler.observe_desired_head();
            let _ = state
                .composition
                .reconciler
                .build_and_publish_candidate(receipt.committed_revision, Vec::new());
            let _ = crate::runtime::reload_engine(&state).await;
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
