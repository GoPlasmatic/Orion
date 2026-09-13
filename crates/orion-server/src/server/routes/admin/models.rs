//! `/api/v1/admin/models` — the model entity, mirroring the plugin surface:
//! register as a draft, list, get, update, delete, status, versions,
//! dependencies, import, export, validate — plus `admit`, which is the one
//! verb a model has that a plugin does not.
//!
//! What differs from a plugin is that nothing is uploaded. A registration
//! carries the manifest and a *reference* — a storage connector, an object
//! key and the digest the bytes must hash to — and answers `202`: the
//! synchronous half (`services::models::prepare` and `resolve`) has checked
//! the manifest, the reference and that the object is there, and the node's
//! admission worker then fetches and verifies the bytes and records the
//! verdict on the row. Activation is gated on that verdict.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde_json::Value;

use crate::errors::OrionError;
use crate::runtime::model_admission::{admit_now, job_for};
use crate::server::admin_auth::AdminPrincipal;
use crate::server::extract::{OrionJson, OrionQuery};
use crate::server::routes::openapi::{DataEnvelope, PaginatedEnvelope};
use crate::server::routes::response_helpers::{created_response, data_response, paginated_into};
use crate::server::state::AppState;
use crate::storage::models::{EntityStatus, Model, ModelAdmission, ModelHealth, ModelResponse};
use crate::storage::repositories::helpers::VersionFilter;
use crate::storage::repositories::models::{
    CreateModelRequest, ModelArtifactRef, ModelFilter, ModelRepository, UpdateModelRequest,
};
use crate::storage::repositories::workflows::StatusChangeRequest;

use super::StatusAction;
use super::services::models as svc;
use super::{ValidationEnvelope, ValidationIssue, audit_log_draft_only, issues_from_error};

/// The admission state a row must reach before it may activate.
const ADMISSION_PASSED: &str = "passed";

/// A request's model id without resolving it: `model_id` when given, else
/// the manifest's `name`.
fn manifest_name(req: &CreateModelRequest) -> Option<String> {
    if let Some(id) = &req.model_id {
        return Some(id.clone());
    }
    req.manifest
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// `prepare` then `resolve`: the whole path from a request to a stored draft.
async fn resolve_request(
    state: &AppState,
    req: &CreateModelRequest,
) -> Result<
    (
        crate::storage::repositories::models::ModelDraft,
        crate::model::HeadInfo,
    ),
    OrionError,
> {
    let prepared = svc::prepare(
        &state.config.models,
        req.model_id.as_deref(),
        &req.manifest,
        &req.artifact,
        req.signature.as_deref(),
        &req.tags,
    )?;
    svc::resolve(state, prepared).await
}

/// Hand a row to this node's admission worker.
///
/// A full queue is logged, not answered: the row is written and honestly
/// `pending`, and `POST /models/{id}/admit` queues it again — where a `5xx`
/// after the write would tell the caller its registration failed when it
/// did not. Nothing to queue on a node without the runtime; the routes that
/// write a row have already refused there.
fn queue_admission(state: &AppState, row: &Model) -> Result<(), OrionError> {
    let Some(models) = &state.models else {
        return Ok(());
    };
    if let Err(full) = models.admissions.enqueue(job_for(row)?) {
        tracing::warn!(
            model = %row.model_id,
            version = row.version,
            error = %full,
            "Model registered but not queued for admission; POST /models/{{id}}/admit retries"
        );
    }
    Ok(())
}

/// The stored verdict, decoded.
fn admission_of(row: &Model) -> Result<ModelAdmission, OrionError> {
    serde_json::from_str(&row.admission_json).map_err(|e| OrionError::Internal {
        context: format!(
            "model '{}' version {}: admission_json does not parse: {e}",
            row.model_id, row.version
        ),
        source: None,
    })
}

/// This node's view of a version, for the single-entity read: `disabled`
/// without the runtime; `pending` or `rejected` from the verdict until it
/// passes; then `admitted` for the active version and `inactive` for any
/// other. The loading states arrive with the runtime that loads.
fn health_of(state: &AppState, row: &Model) -> Result<ModelHealth, OrionError> {
    let mut health = ModelHealth::default();
    if state.models.is_none() {
        health.state = "disabled".to_string();
        return Ok(health);
    }
    let admission = admission_of(row)?;
    match admission.state.as_str() {
        ADMISSION_PASSED => {
            health.state = if row.status == EntityStatus::Active.as_str() {
                "admitted".to_string()
            } else {
                "inactive".to_string()
            };
        }
        "failed" => {
            health.state = "rejected".to_string();
            health.reason = Some(format!(
                "{}: {}",
                admission.stage.as_deref().unwrap_or("admission"),
                admission.reason.as_deref().unwrap_or("no reason recorded")
            ));
        }
        other => {
            health.state = other.to_string();
            health.reason = Some(
                "no node has verified the artifact yet; poll GET /models/{id} or POST \
                 /models/{id}/admit"
                    .to_string(),
            );
        }
    }
    Ok(health)
}

// ============================================================
// Models CRUD
// ============================================================

#[utoipa::path(
    get,
    path = "/api/v1/admin/models",
    params(ModelFilter),
    tag = "Models",
    responses(
        (status = 200, description = "Paginated list of models", body = PaginatedEnvelope<ModelResponse>),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_models(
    State(state): State<AppState>,
    OrionQuery(filter): OrionQuery<ModelFilter>,
) -> Result<Json<Value>, OrionError> {
    let result = state.repos.models.list_paginated(&filter).await?;
    paginated_into(result, |m| ModelResponse::try_from(m))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/models",
    tag = "Models",
    request_body = CreateModelRequest,
    responses(
        (status = 202, description = "Model registered as draft and queued for admission on this \
            node. The manifest was validated, the artifact reference checked against a storage \
            connector that allows reads, and the object confirmed to exist within \
            `models.max_artifact_bytes`; `admission.state` is `pending` until the worker has \
            fetched and verified the bytes — poll `GET /models/{id}`.", body = DataEnvelope<ModelResponse>),
        (status = 400, description = "Invalid manifest, artifact reference, signature or connector; \
            or models are disabled on this node"),
        (status = 409, description = "Model id already exists"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn create_model(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    OrionJson(req): OrionJson<CreateModelRequest>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    let (draft, _head) = resolve_request(&state, &req).await?;
    let model = state.repos.models.create(&draft).await?;
    queue_admission(&state, &model)?;
    audit_log_draft_only(
        &state.audit_queue,
        &principal,
        "create",
        "model",
        &model.model_id,
    );
    Ok((
        StatusCode::ACCEPTED,
        data_response(ModelResponse::try_from(&model)?),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/models/{id}",
    tag = "Models",
    params(("id" = String, Path, description = "Model ID")),
    responses(
        (status = 200, description = "The latest version, with this node's view of it under `health`", body = DataEnvelope<ModelResponse>),
        (status = 404, description = "Model not found"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn get_model(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, OrionError> {
    let model = state.repos.models.get_by_id(&id).await?;
    let mut response = ModelResponse::try_from(&model)?;
    response.health = Some(health_of(&state, &model)?);
    Ok(data_response(response))
}

#[utoipa::path(
    put,
    path = "/api/v1/admin/models/{id}",
    tag = "Models",
    params(("id" = String, Path, description = "Model ID")),
    request_body = UpdateModelRequest,
    responses(
        (status = 200, description = "Draft model updated; an absent field keeps its stored value. \
            A changed artifact reference resets admission to `pending` and queues the draft \
            again; a change to the manifest or tags alone keeps the verdict, which was about \
            the bytes.", body = DataEnvelope<ModelResponse>),
        (status = 400, description = "Invalid input, or models are disabled on this node"),
        (status = 404, description = "Model not found, or it has no draft version to update"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn update_model(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
    OrionJson(req): OrionJson<UpdateModelRequest>,
) -> Result<Json<Value>, OrionError> {
    let existing = state.repos.models.get_by_id(&id).await?;
    let manifest = match req.manifest {
        Some(m) => m,
        None => serde_json::from_str(&existing.manifest_json)?,
    };
    let tags: Vec<String> = match req.tags {
        Some(t) => t,
        None => serde_json::from_str(&existing.tags_json)?,
    };
    let stored: ModelArtifactRef = serde_json::from_str(&existing.artifact_json)?;
    let artifact = req.artifact.unwrap_or_else(|| stored.clone());
    // The stored signature carries over an edit that keeps the digest; one
    // that changes the artifact needs a new signature, and a node with keys
    // says so when the old one no longer verifies.
    let signature = req.signature.or_else(|| existing.signature.clone());
    let prepared = svc::prepare(
        &state.config.models,
        Some(&id),
        &manifest,
        &artifact,
        signature.as_deref(),
        &tags,
    )?;
    let same_artifact = prepared.artifact.connector == stored.connector
        && prepared.artifact.key == stored.key
        && prepared.artifact.digest == stored.digest;
    let (draft, _head) = svc::resolve(&state, prepared).await?;
    let mut model = state.repos.models.replace_draft(&id, &draft).await?;
    if same_artifact {
        // The repository reset the verdict with the content; the reference
        // it was about is unchanged, so it still holds and comes back.
        state
            .repos
            .models
            .set_admission(&id, model.version, &existing.admission_json)
            .await?;
        state
            .repos
            .models
            .set_stats(&id, model.version, existing.stats_json.as_deref())
            .await?;
        model = state.repos.models.get_version(&id, model.version).await?;
    } else {
        queue_admission(&state, &model)?;
    }
    audit_log_draft_only(&state.audit_queue, &principal, "update", "model", &id);
    Ok(data_response(ModelResponse::try_from(&model)?))
}

#[utoipa::path(
    delete,
    path = "/api/v1/admin/models/{id}",
    tag = "Models",
    params(("id" = String, Path, description = "Model ID")),
    responses(
        (status = 204, description = "Model deleted (all versions). The cached artifact, if any, \
            stays in this node's cache until swept"),
        (status = 404, description = "Model not found"),
        (status = 409, description = "An active workflow still names it"),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn delete_model(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
) -> Result<StatusCode, OrionError> {
    let _ = state.repos.models.get_by_id(&id).await?;
    svc::ensure_no_active_dependants(state.repos.workflows.as_ref(), &id, "delete").await?;
    // The delete and its audit row commit together.
    let mut write = super::audited_write(&state, &principal, "delete", "model", &id).await?;
    state.repos.models.delete_tx(write.tx(), &id).await?;
    write.commit().await?;

    super::reload_after_commit_scoped(
        &state,
        super::ReloadMode::Now,
        crate::cluster::EpochScope::Models,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ============================================================
// Model Status Management and admission
// ============================================================

#[utoipa::path(
    patch,
    path = "/api/v1/admin/models/{id}/status",
    tag = "Models",
    params(("id" = String, Path, description = "Model ID"), super::StatusChangeQuery),
    request_body = StatusChangeRequest,
    responses(
        (status = 200, description = "Status updated. Activating supersedes the previously active \
            version in the same transaction, so a model id resolves to one digest per \
            generation; it is refused until the draft's admission has passed. Archiving is \
            refused while an active workflow names the model. `?dry_run=true` reports every \
            gate without writing; `?reload=defer` commits without rebuilding the engine.",
            body = DataEnvelope<ModelResponse>),
        (status = 400, description = "Invalid status transition, or models are disabled on this node"),
        (status = 404, description = "Model not found"),
        (status = 409, description = "Admission is pending or failed, or an active workflow still \
            names the model"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn change_model_status(
    State(state): State<AppState>,
    OrionQuery(query): OrionQuery<super::StatusChangeQuery>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
    OrionJson(req): OrionJson<StatusChangeRequest>,
) -> Result<Json<Value>, OrionError> {
    let action = StatusAction::parse(req.status)?;
    let lifecycle = ModelLifecycle {
        models: state.repos.models.as_ref(),
        enabled: state.models.is_some(),
    };
    // Archiving has a gate of its own: the dependants. Checked before the
    // transaction opens, like the activation gates.
    let archive_gate = async {
        if !matches!(action, StatusAction::Archive) {
            return Ok(());
        }
        svc::ensure_no_active_dependants(state.repos.workflows.as_ref(), &id, "archive").await
    };
    if query.dry_run {
        let mut errors = super::status_change_findings(&lifecycle, &id, &action).await?;
        if let Err(e) = archive_gate.await {
            errors.extend(issues_from_error(e));
        }
        let envelope = ValidationEnvelope::new(errors, Vec::new());
        return Ok(Json(serde_json::to_value(envelope)?));
    }
    if matches!(action, StatusAction::Activate) {
        let draft = state.repos.models.get_by_id(&id).await?;
        super::check_activation(&lifecycle, &draft).await?;
    }
    archive_gate.await?;

    let mut write = super::audited_write(
        &state,
        &principal,
        &format!("status_{}", req.status),
        "model",
        &id,
    )
    .await?;
    let model = match action {
        StatusAction::Activate => state.repos.models.activate_tx(write.tx(), &id).await?,
        StatusAction::Archive => state.repos.models.archive_tx(write.tx(), &id).await?,
    };
    write.commit().await?;

    super::reload_after_commit_scoped(&state, query.reload, crate::cluster::EpochScope::Models)
        .await?;
    Ok(data_response(ModelResponse::try_from(&model)?))
}

/// [`VersionedLifecycle`](super::VersionedLifecycle) for models.
///
/// Two activation gates: the runtime must be on for this node, and the
/// draft's admission must have passed — the artifact was fetched, hashed to
/// its claim and kept. A third, whether the dependants still fit the
/// version, is `Ok` by construction for now; `services::models` says why.
struct ModelLifecycle<'a> {
    models: &'a dyn ModelRepository,
    enabled: bool,
}

impl super::VersionedLifecycle for ModelLifecycle<'_> {
    type Row = Model;
    const NOUN: &'static str = "model";

    fn row_status(row: &Self::Row) -> &str {
        &row.status
    }

    async fn get_by_id(&self, id: &str) -> Result<Self::Row, OrionError> {
        self.models.get_by_id(id).await
    }

    async fn has_active(&self, id: &str) -> Result<bool, OrionError> {
        Ok(self
            .models
            .list_active()
            .await?
            .iter()
            .any(|m| m.model_id == id))
    }

    async fn activation_gates(&self, draft: &Self::Row) -> Vec<OrionError> {
        let mut gates = Vec::new();
        if !self.enabled {
            gates.push(OrionError::validation(format!(
                "Cannot activate model '{}': models are disabled on this node \
                 (models.enabled = false), so it could not be loaded",
                draft.model_id
            )));
        }
        match admission_of(draft) {
            Ok(admission) if admission.state == ADMISSION_PASSED => {}
            Ok(admission) if admission.state == "failed" => {
                gates.push(OrionError::Conflict(format!(
                    "Cannot activate model '{}' version {}: admission failed at stage '{}' on \
                     node '{}': {} — fix the artifact or the reference, then POST \
                     /models/{}/admit to retry",
                    draft.model_id,
                    draft.version,
                    admission.stage.as_deref().unwrap_or("unknown"),
                    admission.node.as_deref().unwrap_or("unknown"),
                    admission.reason.as_deref().unwrap_or("no reason recorded"),
                    draft.model_id
                )));
            }
            Ok(admission) => {
                gates.push(OrionError::Conflict(format!(
                    "Cannot activate model '{}' version {}: admission is '{}' — no node has \
                     verified the artifact yet; poll GET /models/{} until admission.state is \
                     'passed', or POST /models/{}/admit",
                    draft.model_id, draft.version, admission.state, draft.model_id, draft.model_id
                )));
            }
            Err(e) => gates.push(e),
        }
        if let Err(e) = svc::ensure_dependants_accept(draft) {
            gates.push(e);
        }
        gates
    }
}

/// Query parameter accepted by `POST /models/{id}/admit`.
#[derive(Debug, Default, serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct AdmitQuery {
    /// When true, run the admission inline and answer with the verdict
    /// recorded; otherwise queue it for the worker and answer `202`.
    #[serde(default)]
    pub wait: bool,
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/models/{id}/admit",
    tag = "Models",
    params(("id" = String, Path, description = "Model ID"), AdmitQuery),
    responses(
        (status = 202, description = "The latest version queued for admission on this node again \
            — after a fixed bucket, a re-uploaded object, or a node that never ran it. \
            Idempotent: a version already admitted is admitted again and the verdict replaced.",
            body = DataEnvelope<ModelResponse>),
        (status = 200, description = "With `?wait=true`: the admission ran inline and the row \
            carries its verdict", body = DataEnvelope<ModelResponse>),
        (status = 400, description = "Models are disabled on this node"),
        (status = 404, description = "Model not found"),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn admit_model(
    State(state): State<AppState>,
    OrionQuery(query): OrionQuery<AdmitQuery>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    if state.models.is_none() {
        return Err(svc::disabled());
    }
    let mut model = state.repos.models.get_by_id(&id).await?;
    // Nothing in the active set moves: the verdict is a derived column
    // outside the immutability trigger, and no reload follows.
    super::audit_log(&state.audit_queue, &principal, "admit", "model", &id);
    let status = if query.wait {
        admit_now(&state, job_for(&model)?).await?;
        model = state.repos.models.get_version(&id, model.version).await?;
        StatusCode::OK
    } else {
        queue_admission(&state, &model)?;
        StatusCode::ACCEPTED
    };
    Ok((status, data_response(ModelResponse::try_from(&model)?)))
}

// ============================================================
// Model Version Management and dependencies
// ============================================================

#[utoipa::path(
    get,
    path = "/api/v1/admin/models/{id}/versions",
    tag = "Models",
    params(("id" = String, Path, description = "Model ID"), VersionFilter),
    responses(
        (status = 200, description = "Paginated version history", body = PaginatedEnvelope<ModelResponse>),
        (status = 404, description = "Model not found"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_model_versions(
    State(state): State<AppState>,
    Path(id): Path<String>,
    OrionQuery(filter): OrionQuery<VersionFilter>,
) -> Result<Json<Value>, OrionError> {
    let _ = state.repos.models.get_by_id(&id).await?;
    let result = state.repos.models.list_versions(&id, &filter).await?;
    paginated_into(result, |m| ModelResponse::try_from(m))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/models/{id}/versions",
    tag = "Models",
    params(("id" = String, Path, description = "Model ID")),
    responses(
        (status = 201, description = "New draft version copied from the latest, admission verdict \
            and stats included — the reference they describe is unchanged", body = DataEnvelope<ModelResponse>),
        (status = 409, description = "Draft already exists"),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn create_new_model_version(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    let model = state.repos.models.create_new_version(&id).await?;
    audit_log_draft_only(
        &state.audit_queue,
        &principal,
        "create_version",
        "model",
        &id,
    );
    Ok(created_response(ModelResponse::try_from(&model)?))
}

/// What depends on a model: the active workflows naming it.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct ModelDependencies {
    model_id: String,
    version: i64,
    /// Active workflows with a `model_infer` task whose `input.model` is
    /// this id as a literal — the ones an archive or delete is refused for.
    workflows: Vec<svc::ModelDependant>,
    /// Always `true`: a task whose `input.model` is an expression resolves
    /// its model per message, so a reference of that kind is not listed and
    /// not gated on.
    dynamic_references_unlisted: bool,
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/models/{id}/dependencies",
    tag = "Models",
    params(("id" = String, Path, description = "Model ID")),
    responses(
        (status = 200, description = "The active workflows calling `model_infer` on this model by \
            its literal id, with the task ids that do", body = DataEnvelope<ModelDependencies>),
        (status = 404, description = "Model not found"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn model_dependencies(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, OrionError> {
    let model = state.repos.models.get_by_id(&id).await?;
    let workflows = svc::active_workflows_naming(state.repos.workflows.as_ref(), &id).await?;
    Ok(data_response(ModelDependencies {
        model_id: model.model_id,
        version: model.version,
        workflows,
        dynamic_references_unlisted: true,
    }))
}

// ============================================================
// Model Import / Export / Validation
// ============================================================

#[utoipa::path(
    post,
    path = "/api/v1/admin/models/import",
    tag = "Models",
    request_body = Vec<CreateModelRequest>,
    params(super::ImportQuery),
    responses(
        (status = 200, description = "Import results with counts (or would-be results when ?dry_run=true). \
            Each item is handled independently and carries a manifest and an artifact reference, \
            never bytes — what an export produces. Every item written is queued for admission \
            on this node. `?on_conflict=new_version` upserts: an existing draft is replaced, an \
            active model whose content differs gets a new draft version, identical content is \
            reported `unchanged`.", body = DataEnvelope<orion_api::ImportResult>),
    )
)]
#[tracing::instrument(skip(state, items, principal), fields(count = items.len()))]
pub(crate) async fn import_models(
    State(state): State<AppState>,
    OrionQuery(query): OrionQuery<super::ImportQuery>,
    principal: Option<Extension<AdminPrincipal>>,
    OrionJson(items): OrionJson<Vec<Value>>,
) -> Result<Json<Value>, OrionError> {
    super::check_import_batch_size(items.len())?;
    let config = state.config.clone();
    let probe = state.repos.models.clone();
    let create_state = state.clone();
    let upsert_state = state.clone();
    let outcome = super::import_items::<CreateModelRequest, _, _, _, _, _, _, _, _>(
        items,
        query.dry_run,
        query.on_conflict,
        super::ImportOps {
            // The synchronous half: manifest, id, reference, signature. The
            // connector and the object are checked when the item is written.
            validate: |m: &CreateModelRequest| {
                svc::prepare(
                    &config.models,
                    m.model_id.as_deref(),
                    &m.manifest,
                    &m.artifact,
                    m.signature.as_deref(),
                    &m.tags,
                )
                .map(|_| ())
            },
            conflict_key: manifest_name,
            exists: |id: String| {
                let repo = probe.clone();
                async move { super::workflows::exists_or_err(repo.get_by_id(&id).await) }
            },
            create: |m: CreateModelRequest| {
                let state = create_state.clone();
                async move { super::VersionedUpsert::create(&ModelUpsert(&state), &m).await }
            },
            upsert: |m: CreateModelRequest, dry_run: bool| {
                let state = upsert_state.clone();
                async move { super::versioned_upsert(&ModelUpsert(&state), m, dry_run).await }
            },
        },
    )
    .await;
    if query.dry_run {
        return Ok(super::import_response(true, outcome));
    }
    for id in outcome.written() {
        audit_log_draft_only(&state.audit_queue, &principal, "import", "model", id);
    }
    audit_log_draft_only(
        &state.audit_queue,
        &principal,
        "import",
        "model",
        &format!("{} imported", outcome.imported),
    );
    Ok(super::import_response(false, outcome))
}

/// [`super::VersionedUpsert`] for models. Every write queues the row for
/// admission: a created row has no verdict, and a replaced draft's verdict
/// was reset with its content.
struct ModelUpsert<'a>(&'a AppState);

impl super::VersionedUpsert for ModelUpsert<'_> {
    type Row = Model;
    type Request = CreateModelRequest;

    fn request_id(req: &Self::Request) -> Option<String> {
        manifest_name(req)
    }

    fn row_status(row: &Self::Row) -> &str {
        &row.status
    }

    fn content_matches(row: &Self::Row, req: &Self::Request) -> Result<bool, OrionError> {
        // `content_matches` is synchronous, and the content is decidable
        // without the bucket: the manifest as it will be stored, the
        // reference without its size, the tags.
        let prepared = svc::prepare(
            // The default config names no trust keys: the signature is not
            // content — the digest is the identity — and the real config
            // applies at write.
            &crate::config::ModelsConfig::default(),
            req.model_id.as_deref(),
            &req.manifest,
            &req.artifact,
            req.signature.as_deref(),
            &req.tags,
        )?;
        Ok(crate::storage::content::model_content(row)?
            == crate::storage::content::model_request_content(
                &prepared.manifest_json,
                &serde_json::to_value(&prepared.artifact)?,
                &prepared.tags,
            ))
    }

    async fn create(&self, req: &Self::Request) -> Result<(), OrionError> {
        let (draft, _head) = resolve_request(self.0, req).await?;
        let model = self.0.repos.models.create(&draft).await?;
        queue_admission(self.0, &model)
    }

    async fn get_by_id(&self, id: &str) -> Result<Self::Row, OrionError> {
        self.0.repos.models.get_by_id(id).await
    }

    async fn create_new_version(&self, id: &str) -> Result<(), OrionError> {
        self.0.repos.models.create_new_version(id).await.map(|_| ())
    }

    async fn replace_draft(&self, id: &str, req: &Self::Request) -> Result<(), OrionError> {
        let (draft, _head) = resolve_request(self.0, req).await?;
        let model = self.0.repos.models.replace_draft(id, &draft).await?;
        queue_admission(self.0, &model)
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/models/export",
    tag = "Models",
    params(ModelFilter),
    responses(
        (status = 200, description = "Exported models, importable as they are: each item carries \
            the manifest and the artifact reference — connector, key, digest — and never the \
            bytes. The target fetches the object from its own connector of that name at \
            admission.", body = DataEnvelope<Vec<ModelResponse>>),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn export_models(
    State(state): State<AppState>,
    OrionQuery(filter): OrionQuery<ModelFilter>,
) -> Result<Json<Value>, OrionError> {
    let rows = state.repos.models.snapshot(&filter).await?;
    let data = rows
        .iter()
        .map(ModelResponse::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(data_response(data))
}

/// What the bucket said about the object, when `validate` got that far.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct ArtifactHead {
    /// `Content-Length`, when the bucket sent one.
    size: Option<u64>,
    /// The object's ETag, unquoted, when the bucket sent one.
    etag: Option<String>,
}

/// The `/validate` envelope for models: the shared shape plus what the
/// synchronous half learned about the object.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct ModelValidationResponse {
    #[serde(flatten)]
    validation: super::ValidationResponse,
    /// Present when the reference resolved and the object answered a HEAD.
    #[serde(skip_serializing_if = "Option::is_none")]
    head: Option<ArtifactHead>,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct ModelValidationEnvelope {
    data: ModelValidationResponse,
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/models/validate",
    tag = "Models",
    request_body = CreateModelRequest,
    responses(
        (status = 200, description = "Validation result: `valid: true` means `POST /models` would \
            accept this payload on this node — the manifest parses, the reference names a \
            storage connector that allows reads, and the object exists within \
            `models.max_artifact_bytes`. `head` carries the object's size and ETag when the \
            check got that far. Nothing is written and nothing is fetched.",
            body = ModelValidationEnvelope),
    )
)]
#[tracing::instrument(skip(state, req))]
pub(crate) async fn validate_model(
    State(state): State<AppState>,
    OrionJson(req): OrionJson<CreateModelRequest>,
) -> Result<Json<ModelValidationEnvelope>, OrionError> {
    let (errors, head): (Vec<ValidationIssue>, Option<ArtifactHead>) =
        match resolve_request(&state, &req).await {
            Ok((_draft, head)) => (
                Vec::new(),
                Some(ArtifactHead {
                    size: head.size,
                    etag: head.etag,
                }),
            ),
            Err(e) => (issues_from_error(e), None),
        };
    // `valid` is derived in one place, the shared envelope, and lifted from
    // there so this endpoint cannot come to mean something else by it.
    let validation = ValidationEnvelope::new(errors, Vec::new()).data;
    Ok(Json(ModelValidationEnvelope {
        data: ModelValidationResponse { validation, head },
    }))
}
