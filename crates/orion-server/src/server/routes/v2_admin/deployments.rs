//! V2 candidate deployment plans, and deployments — an apply or a rollback,
//! each one CAS commit — with their convergence (`/api/v2/admin/deployment*`).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use orion_api::v2::{
    ApplyDeploymentRequest, ConvergenceStatusResponse, CreateDeploymentPlanRequest,
    CreateDeploymentRequest, CreateDeploymentResponse, DeploymentEntityUpdate,
    DeploymentPlanResponse, DeploymentReceiptResponse, DeploymentRevisionResponse,
    DeploymentRevisionSummary, DeploymentsPage, RollbackDeploymentRequest,
};
use serde::Deserialize;

use super::{V2Error, V2Result, entity_ref_dto, principal_id};
use crate::extensions::deployment_store::{
    CandidatePlan, CanonicalManifest, DeploymentError, DeploymentErrorKind, DeploymentReceipt,
    DeploymentRevisionRow, EntityRef,
};
use crate::extensions::reconciler::ConvergenceTarget;
use crate::runtime::state::AppState;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::extract::{OrionJson, OrionQuery};

#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ConvergenceQuery {
    /// Convergence evaluation scope (`this-node` or `observed`).
    pub target: Option<String>,
}

/// The default and the largest page of `GET /deployments`, as every list
/// surface pages.
const DEFAULT_PAGE: u64 = 50;
const MAX_PAGE: u64 = 1000;

#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct DeploymentsQuery {
    /// Revisions per page, newest first: 50 by default, at most 1000.
    pub limit: Option<u64>,
    /// The `next_cursor` of the previous page.
    pub cursor: Option<String>,
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
        principal: receipt.principal.clone(),
        request_hash: receipt.request_hash.clone(),
        committed_revision: receipt.committed_revision,
        manifest_digest: receipt.manifest_digest.clone(),
        epoch: receipt.epoch,
        receiving_node_id: receipt.receiving_node_id.clone(),
        created_at: wire_time(receipt.created_at),
    }
}

fn revision_summary(row: &DeploymentRevisionRow) -> DeploymentRevisionSummary {
    DeploymentRevisionSummary {
        revision: row.revision,
        parent_revision: row.parent_revision,
        manifest_digest: row.manifest_digest.clone(),
        member_count: row.manifest.members.len(),
        reason: row.reason.clone(),
        created_by: row.created_by.clone(),
        members_compacted: row.members_compacted,
        created_at: wire_time(row.created_at),
    }
}

/// The plan's change to its base, by member: one the base does not deploy
/// is added, one it deploys at another revision is updated, and one the
/// candidate drops is removed.
fn plan_diff(
    plan: &CandidatePlan,
) -> (
    Vec<orion_api::v2::EntityRef>,
    Vec<DeploymentEntityUpdate>,
    Vec<orion_api::v2::EntityRef>,
) {
    let same_member = |a: &EntityRef, b: &EntityRef| a.kind == b.kind && a.id == b.id;
    let mut added = Vec::new();
    let mut updated = Vec::new();
    for entity in &plan.added_entities {
        match plan
            .removed_entities
            .iter()
            .find(|old| same_member(old, entity))
        {
            Some(old) => updated.push(DeploymentEntityUpdate {
                kind: entity.kind,
                id: entity.id.clone(),
                from_revision: old.revision.get(),
                to_revision: entity.revision.get(),
            }),
            None => added.push(entity_ref_dto(entity)),
        }
    }
    let removed = plan
        .removed_entities
        .iter()
        .filter(|old| !plan.added_entities.iter().any(|e| same_member(old, e)))
        .map(entity_ref_dto)
        .collect();
    (added, updated, removed)
}

/// `POST /api/v2/admin/deployment-plans`.
#[utoipa::path(
    post,
    path = "/api/v2/admin/deployment-plans",
    tag = "Deployments",
    summary = "Plan and validate a candidate deployment manifest",
    description = "Validates canonical manifest invariants, provider admission status, and connection references against the requested base revision, and checks that this node could build the candidate. Answers the reviewed manifest digest an apply must name, and the members the candidate adds, updates and removes. Nothing is stored: planning the same candidate on the same base answers the same plan.",
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
    OrionJson(req): OrionJson<CreateDeploymentPlanRequest>,
) -> V2Result<Json<DeploymentPlanResponse>> {
    let manifest = CanonicalManifest::from_wire(&req.manifest)?;
    let plan = state
        .deployments
        .plan_candidate_in_db(&state.db_pool, req.base_revision, manifest)
        .await?;
    refuse_unbuildable(&state, &plan).await?;
    let (added_entities, updated_entities, removed_entities) = plan_diff(&plan);
    Ok(Json(DeploymentPlanResponse {
        base_revision: plan.base_revision,
        reviewed_manifest_digest: plan.candidate_digest,
        added_entities,
        updated_entities,
        removed_entities,
    }))
}

/// `GET /api/v2/admin/deployments`.
#[utoipa::path(
    get,
    path = "/api/v2/admin/deployments",
    tag = "Deployments",
    summary = "Page through committed deployment revisions",
    description = "The deployment head, its epoch and the revision this node serves, with one page of committed revisions, newest first, each without its manifest — `GET /deployments/{revision}` reads one in full. Pass `next_cursor` back as `?cursor=` for the next page.",
    params(DeploymentsQuery),
    responses(
        (status = 200, description = "The head and a page of revisions", body = DeploymentsPage),
        (status = 400, description = "A `limit` of 0, or a cursor this endpoint did not issue"),
    )
)]
pub(crate) async fn list_deployments(
    State(state): State<AppState>,
    OrionQuery(query): OrionQuery<DeploymentsQuery>,
) -> V2Result<Json<DeploymentsPage>> {
    let limit = query.limit.unwrap_or(DEFAULT_PAGE);
    if limit == 0 {
        return Err(crate::errors::OrionError::invalid_field(
            "limit",
            "INVALID",
            "limit must be at least 1",
        )
        .into());
    }
    let limit = limit.min(MAX_PAGE);
    // The cursor is the revision the previous page ended at: this page is
    // what came before it.
    let before = match query.cursor.as_deref() {
        None => None,
        Some(cursor) => Some(cursor.parse::<u64>().map_err(|_| {
            crate::errors::OrionError::invalid_field(
                "cursor",
                "INVALID",
                format!("'{cursor}' is not a cursor this endpoint issued"),
            )
        })?),
    };
    let store = &state.deployments;
    let (head_revision, epoch) = store.current_head();
    let mut revisions = store.list_revisions();
    revisions.retain(|r| before.is_none_or(|b| r.revision < b));
    revisions.sort_by_key(|r| std::cmp::Reverse(r.revision));
    let more = revisions.len() as u64 > limit;
    revisions.truncate(limit as usize);
    let next_cursor = more
        .then(|| revisions.last().map(|r| r.revision.to_string()))
        .flatten();
    Ok(Json(DeploymentsPage {
        head_revision,
        epoch,
        applied_revision: state.reconciler.local_status().applied_revision,
        data: revisions.iter().map(|r| revision_summary(r)).collect(),
        limit,
        next_cursor,
    }))
}

/// `POST /api/v2/admin/deployments`.
///
/// Commits the next revision under CAS on `base_revision` — the reviewed
/// candidate, or an earlier revision's manifest again — and answers
/// `202 Accepted` with the durable receipt.
#[utoipa::path(
    post,
    path = "/api/v2/admin/deployments",
    tag = "Deployments",
    summary = "Commit the next deployment revision: an apply or a rollback",
    description = "An apply (`manifest` with its `reviewed_manifest_digest`) plans the manifest again, refuses it unless it plans to the digest that was reviewed, and commits it. A rollback (`rollback_to`) commits an earlier revision's manifest as a new revision, re-checking its providers' admissions as of now. Either is a compare-and-swap against `base_revision` in one database transaction, idempotent on `idempotency_key` per principal (a replay answers the receipt it committed), and then reconciles the local node.",
    request_body = CreateDeploymentRequest,
    responses(
        (status = 202, description = "Committed and accepted for convergence", body = CreateDeploymentResponse),
        (status = 400, description = "Manifest validation failed, the body is neither an apply nor a rollback, or the rollback target is compacted"),
        (status = 403, description = "Provider not admitted or admission invalidated"),
        (status = 404, description = "The rollback target does not exist"),
        (status = 409, description = "Base revision conflict, idempotency conflict, or reviewed digest mismatch"),
    )
)]
pub(crate) async fn create_deployment(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    OrionJson(req): OrionJson<CreateDeploymentRequest>,
) -> V2Result<(StatusCode, Json<CreateDeploymentResponse>)> {
    let (receipt, warnings) = match req {
        CreateDeploymentRequest::Apply(apply) => (
            apply_deployment(&state, &principal, apply).await?,
            Vec::new(),
        ),
        CreateDeploymentRequest::Rollback(rollback) => {
            rollback_deployment(&state, &principal, rollback).await?
        }
    };
    let (evaluation, local_error) = reconcile_locally(&state, receipt.committed_revision).await;
    Ok((
        StatusCode::ACCEPTED,
        Json(CreateDeploymentResponse {
            committed_revision: receipt.committed_revision,
            receipt: receipt_to_dto(&receipt),
            warnings,
            evaluation,
            local_error,
        }),
    ))
}

/// Plan `req`'s manifest again, refuse it unless it plans to the reviewed
/// digest, and commit it.
async fn apply_deployment(
    state: &AppState,
    principal: &Option<Extension<AdminPrincipal>>,
    req: ApplyDeploymentRequest,
) -> Result<DeploymentReceipt, DeploymentError> {
    let actor = principal_id(principal);
    let manifest = CanonicalManifest::from_wire(&req.manifest)?;
    let plan = state
        .deployments
        .plan_candidate_in_db(&state.db_pool, req.base_revision, manifest)
        .await?;
    refuse_unbuildable(state, &plan).await?;
    if req.reviewed_manifest_digest != plan.candidate_digest {
        return Err(DeploymentError::new(
            DeploymentErrorKind::ReviewedDigestMismatch,
            format!(
                "reviewed_manifest_digest '{}' does not match candidate manifest digest '{}'",
                req.reviewed_manifest_digest, plan.candidate_digest
            ),
        ));
    }
    // The audit row is written in the commit's transaction: it lands with
    // the head move or not at all, and a replay that commits nothing writes
    // none.
    let (receipt, recorded) = state
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
                    principal,
                    "v2_deployment_apply",
                    "deployment",
                    &receipt.committed_revision.to_string(),
                )
            },
        )
        .await?;
    if let Some(event) = recorded {
        crate::server::routes::admin::emit_audit_trail(&event);
    }
    Ok(receipt)
}

/// Commit `req.rollback_to`'s manifest again as the next revision.
async fn rollback_deployment(
    state: &AppState,
    principal: &Option<Extension<AdminPrincipal>>,
    req: RollbackDeploymentRequest,
) -> Result<(DeploymentReceipt, Vec<String>), DeploymentError> {
    let actor = principal_id(principal);
    let ((receipt, warnings), recorded) = state
        .deployments
        .rollback_to_revision_audited_in_db(
            &state.db_pool,
            &crate::extensions::deployment_store::Rollback {
                principal: &actor,
                idempotency_key: &req.idempotency_key,
                expected_head_revision: req.base_revision,
                target_revision: req.rollback_to,
                reason: &req.reason,
            },
            state.composition.admission(),
            // In the commit's transaction, as an apply's is.
            |receipt| {
                crate::server::routes::admin::audit_record(
                    principal,
                    "v2_deployment_rollback",
                    "deployment",
                    &format!("{}->{}", req.rollback_to, receipt.committed_revision),
                )
            },
        )
        .await?;
    if let Some(event) = recorded {
        crate::server::routes::admin::emit_audit_trail(&event);
    }
    Ok((receipt, warnings))
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
        (status = 404, description = "No revision has that number (`REVISION_NOT_FOUND`)"),
    )
)]
pub(crate) async fn get_deployment_revision(
    State(state): State<AppState>,
    Path(revision): Path<u64>,
) -> V2Result<Json<DeploymentRevisionResponse>> {
    let row = state
        .deployments
        .list_revisions()
        .into_iter()
        .find(|r| r.revision == revision)
        .ok_or_else(|| {
            V2Error::new(
                DeploymentErrorKind::RevisionNotFound,
                format!("deployment revision {revision} not found"),
            )
        })?;
    Ok(Json(DeploymentRevisionResponse {
        summary: revision_summary(&row),
        manifest: row.manifest.to_wire(),
    }))
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
) -> Json<ConvergenceStatusResponse> {
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
    Json(dto)
}
