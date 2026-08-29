use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde_json::Value;

use crate::errors::OrionError;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::extract::{OrionJson, OrionQuery};
use crate::server::routes::openapi::{DataEnvelope, ImportResult, PaginatedEnvelope};
use crate::server::routes::response_helpers::{created_response, data_response, paginated_into};
use crate::server::state::AppState;
use crate::storage::models::ChannelResponse;
use crate::storage::repositories::channels::{
    ChannelFilter, ChannelStatusChangeRequest, CreateChannelRequest, UpdateChannelRequest,
};

use super::StatusAction;
use super::audit_and_reload;
use super::audit_log_draft_only;
use crate::storage::repositories::helpers::VersionFilter;

// ============================================================
// Channels CRUD
// ============================================================

#[utoipa::path(
    get,
    path = "/api/v1/admin/channels",
    params(ChannelFilter),
    tag = "Channels",
    responses(
        (status = 200, description = "Paginated list of channels", body = PaginatedEnvelope<ChannelResponse>),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_channels(
    State(state): State<AppState>,
    OrionQuery(filter): OrionQuery<ChannelFilter>,
) -> Result<Json<Value>, OrionError> {
    let result = state.repos.channels.list_paginated(&filter).await?;
    paginated_into(result, |c| ChannelResponse::try_from(c))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/channels",
    tag = "Channels",
    request_body = CreateChannelRequest,
    responses(
        (status = 201, description = "Channel created as draft", body = DataEnvelope<ChannelResponse>),
        (status = 400, description = "Invalid input"),
        (status = 409, description = "Channel id already exists"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn create_channel(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    OrionJson(req): OrionJson<CreateChannelRequest>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    crate::validation::validate_create_channel(&req)?;
    let channel = state.repos.channels.create(&req).await?;
    audit_log_draft_only(
        &state.audit_queue,
        &principal,
        "create",
        "channel",
        &channel.channel_id,
    );
    Ok(created_response(ChannelResponse::try_from(&channel)?))
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/channels/{id}",
    tag = "Channels",
    params(("id" = String, Path, description = "Channel ID")),
    responses(
        (status = 200, description = "Channel details", body = DataEnvelope<ChannelResponse>),
        (status = 404, description = "Channel not found"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn get_channel(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, OrionError> {
    let channel = state.repos.channels.get_by_id(&id).await?;
    Ok(data_response(ChannelResponse::try_from(&channel)?))
}

#[utoipa::path(
    put,
    path = "/api/v1/admin/channels/{id}",
    tag = "Channels",
    params(("id" = String, Path, description = "Channel ID")),
    request_body = UpdateChannelRequest,
    responses(
        (status = 200, description = "Draft channel updated", body = DataEnvelope<ChannelResponse>),
        (status = 400, description = "Invalid input"),
        (status = 404, description = "Channel not found, or it has no draft version to update"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn update_channel(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
    OrionJson(mut req): OrionJson<UpdateChannelRequest>,
) -> Result<Json<Value>, OrionError> {
    // R3: mirror create-time validation. The latest version (404 if the
    // channel is absent) supplies the protocol and current field values for
    // the merged-view checks; when a draft exists it is the latest version,
    // i.e. exactly the row `update_draft` mutates.
    let current = state.repos.channels.get_by_id(&id).await?;
    // H3: channel reads mask `auth.keys` / `auth.secret`, so a GET → edit →
    // PUT cycle sends the sentinel back for every credential the caller
    // never saw. Restore each masked position from the stored config before
    // validating (which rejects any sentinel left unmatched) — the F34
    // treatment connectors have always had.
    if let Some(ref mut config) = req.config
        && let Ok(stored) = serde_json::from_str::<Value>(&current.config_json)
    {
        crate::connector::unmask_channel_config(config, &stored);
    }
    crate::validation::validate_update_channel(&current, &req)?;
    let channel = state.repos.channels.update_draft(&id, &req).await?;
    audit_log_draft_only(&state.audit_queue, &principal, "update", "channel", &id);
    Ok(data_response(ChannelResponse::try_from(&channel)?))
}

#[utoipa::path(
    delete,
    path = "/api/v1/admin/channels/{id}",
    tag = "Channels",
    params(("id" = String, Path, description = "Channel ID")),
    responses(
        (status = 204, description = "Channel deleted"),
        (status = 404, description = "Channel not found"),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn delete_channel(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
) -> Result<StatusCode, OrionError> {
    state.repos.channels.delete(&id).await?;
    audit_and_reload(
        &state,
        &principal,
        "delete",
        "channel",
        &id,
        super::ReloadMode::Now,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ============================================================
// Channel Status Management
// ============================================================

#[utoipa::path(
    patch,
    path = "/api/v1/admin/channels/{id}/status",
    tag = "Channels",
    params(("id" = String, Path, description = "Channel ID"), super::StatusChangeQuery),
    request_body = ChannelStatusChangeRequest,
    responses(
        (status = 200, description = "Status updated. With `?dry_run=true` nothing is \
            written and the body is instead the `/validate` envelope \
            (`{\"data\": {\"valid\", \"errors\", \"warnings\"}}`) reporting every gate \
            the real transition would run: draft existence, route collisions against \
            active channels, and the workflow-active gate (K3). With `?reload=defer` \
            the row commits but the engine (and every cluster peer) keeps serving the \
            previous active set until `POST /engine/reload` (K4).", body = DataEnvelope<ChannelResponse>),
        (status = 400, description = "Invalid status transition, route collision, or \
            the channel's workflow is missing or not active (K8)"),
        (status = 404, description = "Channel not found"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn change_channel_status(
    State(state): State<AppState>,
    OrionQuery(query): OrionQuery<super::StatusChangeQuery>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
    OrionJson(req): OrionJson<ChannelStatusChangeRequest>,
) -> Result<Json<Value>, OrionError> {
    let action = StatusAction::parse(req.status)?;
    let lifecycle = ChannelLifecycle::new(&state);
    if query.dry_run {
        let errors = super::status_change_findings(&lifecycle, &id, &action).await?;
        let envelope = super::ValidationEnvelope::new(errors, Vec::new());
        return Ok(Json(serde_json::to_value(envelope)?));
    }
    let channel = match action {
        StatusAction::Activate => {
            let draft = state.repos.channels.get_by_id(&id).await?;
            super::check_activation(&lifecycle, &draft).await?;
            state.repos.channels.activate(&id).await?
        }
        StatusAction::Archive => state.repos.channels.archive(&id).await?,
    };
    audit_and_reload(
        &state,
        &principal,
        &format!("status_{}", req.status),
        "channel",
        &id,
        query.reload,
    )
    .await?;
    Ok(data_response(ChannelResponse::try_from(&channel)?))
}

/// [`VersionedLifecycle`](super::VersionedLifecycle) for channels: the three
/// gates a channel activation runs, in the order it runs them.
///
/// Each takes the one thing it reads — a repository, the configured data mounts
/// — rather than the whole state, which is what makes them askable outside a
/// request (`services::channels`).
struct ChannelLifecycle<'a> {
    channels: &'a dyn crate::storage::repositories::channels::ChannelRepository,
    workflows: &'a dyn crate::storage::repositories::workflows::WorkflowRepository,
    data_mounts: &'a [String],
}

impl<'a> ChannelLifecycle<'a> {
    fn new(state: &'a AppState) -> Self {
        Self {
            channels: &*state.repos.channels,
            workflows: &*state.repos.workflows,
            data_mounts: &state.config.server.data_mounts,
        }
    }
}

impl super::VersionedLifecycle for ChannelLifecycle<'_> {
    type Row = crate::storage::models::Channel;
    const NOUN: &'static str = "channel";

    fn row_status(row: &Self::Row) -> &str {
        &row.status
    }

    async fn get_by_id(&self, id: &str) -> Result<Self::Row, OrionError> {
        self.channels.get_by_id(id).await
    }

    async fn has_active(&self, id: &str) -> Result<bool, OrionError> {
        Ok(self
            .channels
            .list_active()
            .await?
            .iter()
            .any(|c| c.channel_id == id))
    }

    async fn activation_gates(&self, draft: &Self::Row) -> Vec<OrionError> {
        let mut refusals = Vec::new();
        // R7: refuse a channel whose route another active channel already
        // claims. The question is fully answerable here, and answering it later
        // means answering it wrong: the loser's declared path resolves to the
        // winner's workflow, which is a wrong answer rather than an error.
        if let Err(e) = super::services::channels::ensure_route_is_unclaimed(
            self.channels,
            self.data_mounts,
            draft,
        )
        .await
        {
            refusals.push(e);
        }
        // K8: and refuse a channel whose workflow cannot serve. This gate was
        // documented (and relied on by the promotion flow's ordering) before it
        // existed in code — the failure used to surface later, as a reload-time
        // quarantine with no error to the caller.
        if let Err(e) =
            super::services::channels::ensure_workflow_is_active(self.workflows, draft).await
        {
            refusals.push(e);
        }
        // K7: and a name another *active* channel holds. Create/update refuse
        // the collision at write time; this catches rows written before that
        // gate existed, at the moment the collision would start losing requests.
        if let Err(e) =
            super::services::channels::ensure_name_is_unclaimed(self.channels, draft).await
        {
            refusals.push(e);
        }
        refusals
    }
}

// ============================================================
// Channel Version Management
// ============================================================

#[utoipa::path(
    get,
    path = "/api/v1/admin/channels/{id}/versions",
    tag = "Channels",
    params(
        ("id" = String, Path, description = "Channel ID"),
        VersionFilter,
    ),
    responses(
        (status = 200, description = "Paginated version history", body = PaginatedEnvelope<ChannelResponse>),
        (status = 404, description = "Channel not found"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_channel_versions(
    State(state): State<AppState>,
    Path(id): Path<String>,
    OrionQuery(filter): OrionQuery<VersionFilter>,
) -> Result<Json<Value>, OrionError> {
    // Verify channel exists
    let _ = state.repos.channels.get_by_id(&id).await?;

    let result = state.repos.channels.list_versions(&id, &filter).await?;
    paginated_into(result, |c| ChannelResponse::try_from(c))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/channels/{id}/versions",
    tag = "Channels",
    params(("id" = String, Path, description = "Channel ID")),
    responses(
        (status = 201, description = "New draft version created", body = DataEnvelope<ChannelResponse>),
        (status = 409, description = "Draft already exists"),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn create_new_channel_version(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    let channel = state.repos.channels.create_new_version(&id).await?;
    audit_log_draft_only(
        &state.audit_queue,
        &principal,
        "create_version",
        "channel",
        &id,
    );
    Ok(created_response(ChannelResponse::try_from(&channel)?))
}

// ============================================================
// Channel Bulk Import (B6)
// ============================================================

#[utoipa::path(
    post,
    path = "/api/v1/admin/channels/import",
    tag = "Channels",
    request_body = Vec<CreateChannelRequest>,
    params(super::ImportQuery),
    responses(
        (status = 200, description = "Import results with counts (or would-be results when ?dry_run=true). \
            Each item is handled independently: a malformed or conflicting item becomes one entry in \
            `errors` and the rest of the batch still applies. Dry-run additionally probes for id \
            conflicts against stored rows and duplicates within the batch, without writing. \
            `?on_conflict=new_version` upserts instead of refusing an existing id (K2): an existing \
            draft is replaced, an active channel whose content differs gets a new draft version, \
            and identical content is reported `unchanged` — re-importing the same artifact is a \
            no-op. Per-item outcomes are in `results`.", body = DataEnvelope<ImportResult>),
    )
)]
#[tracing::instrument(skip(state, items, principal), fields(count = items.len()))]
pub(crate) async fn import_channels(
    State(state): State<AppState>,
    OrionQuery(query): OrionQuery<super::ImportQuery>,
    principal: Option<Extension<AdminPrincipal>>,
    OrionJson(items): OrionJson<Vec<Value>>,
) -> Result<Json<Value>, OrionError> {
    // Each create runs through the same validation + persistence as the
    // singular POST endpoint, so behaviour matches.
    super::check_import_batch_size(items.len())?;
    let repo = state.repos.channels.clone();
    let probe = state.repos.channels.clone();
    let upsert_repo = state.repos.channels.clone();
    let outcome =
        super::import_items::<CreateChannelRequest, _, _, _, _, _, _, _, _>(
            items,
            query.dry_run,
            query.on_conflict,
            super::ImportOps {
                validate: crate::validation::validate_create_channel,
                conflict_key: |c: &CreateChannelRequest| c.channel_id.clone(),
                exists: |id: String| {
                    let repo = probe.clone();
                    async move { super::workflows::exists_or_err(repo.get_by_id(&id).await) }
                },
                create: |ch: CreateChannelRequest| {
                    let repo = repo.clone();
                    async move { repo.create(&ch).await.map(|_| ()) }
                },
                upsert: |ch: CreateChannelRequest, dry_run: bool| {
                    let repo = upsert_repo.clone();
                    async move {
                        super::versioned_upsert(&ChannelUpsert(repo.as_ref()), ch, dry_run).await
                    }
                },
            },
        )
        .await;
    if query.dry_run {
        return Ok(super::import_response(true, outcome));
    }
    // K5: one row per written entity, plus the batch summary row.
    for id in outcome.written() {
        audit_log_draft_only(&state.audit_queue, &principal, "import", "channel", id);
    }
    audit_log_draft_only(
        &state.audit_queue,
        &principal,
        "import",
        "channel",
        &format!("{} imported", outcome.imported),
    );
    Ok(super::import_response(false, outcome))
}

/// [`super::VersionedUpsert`] for channels — what the shared import upsert
/// needs of this entity and nothing more.
struct ChannelUpsert<'a>(&'a dyn crate::storage::repositories::channels::ChannelRepository);

impl super::VersionedUpsert for ChannelUpsert<'_> {
    type Row = crate::storage::models::Channel;
    type Request = CreateChannelRequest;

    fn request_id(req: &Self::Request) -> Option<String> {
        req.channel_id.clone()
    }

    fn row_status(row: &Self::Row) -> &str {
        &row.status
    }

    fn content_matches(row: &Self::Row, req: &Self::Request) -> Result<bool, OrionError> {
        Ok(crate::storage::content::channel_content(row)?
            == crate::storage::content::channel_request_content(req))
    }

    async fn create(&self, req: &Self::Request) -> Result<(), OrionError> {
        self.0.create(req).await.map(|_| ())
    }

    async fn get_by_id(&self, id: &str) -> Result<Self::Row, OrionError> {
        self.0.get_by_id(id).await
    }

    async fn create_new_version(&self, id: &str) -> Result<(), OrionError> {
        self.0.create_new_version(id).await.map(|_| ())
    }

    async fn replace_draft(&self, id: &str, req: &Self::Request) -> Result<(), OrionError> {
        self.0.replace_draft(id, req).await.map(|_| ())
    }
}

// ============================================================
// Channel Export / Validate
// ============================================================

#[utoipa::path(
    get,
    path = "/api/v1/admin/channels/export",
    tag = "Channels",
    params(ChannelFilter),
    responses(
        (status = 200, description = "Exported channels", body = DataEnvelope<Vec<ChannelResponse>>),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn export_channels(
    State(state): State<AppState>,
    OrionQuery(filter): OrionQuery<ChannelFilter>,
) -> Result<Json<Value>, OrionError> {
    // K12: one repeatable-read transaction — the export is a consistent
    // snapshot, not a sequence of independent page queries.
    let rows = state.repos.channels.snapshot(&filter).await?;

    let data: Vec<ChannelResponse> = rows
        .iter()
        .map(ChannelResponse::try_from)
        .collect::<Result<_, _>>()?;
    Ok(data_response(data))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/channels/validate",
    tag = "Channels",
    request_body = CreateChannelRequest,
    responses(
        (status = 200, description = "Validation result", body = super::ValidationEnvelope),
    )
)]
#[tracing::instrument(skip(req))]
pub(crate) async fn validate_channel(
    OrionJson(req): OrionJson<CreateChannelRequest>,
) -> Result<Json<super::ValidationEnvelope>, OrionError> {
    // R20: run the create-path validator verbatim rather than re-deriving its
    // rules, so `valid: true` cannot come to mean something weaker than
    // "`POST /channels` would accept this". The workflow validator learned that
    // lesson the expensive way — a second implementation drifted and reported
    // valid for payloads create rejected.
    let errors = match crate::validation::validate_create_channel(&req) {
        Ok(()) => Vec::new(),
        Err(e) => super::issues_from_error(e),
    };
    let mut warnings = Vec::new();

    // Checks create does not make, because a channel may legitimately be
    // authored before the workflow it names.
    if let Some(ref workflow_id) = req.workflow_id
        && !workflow_id.is_empty()
    {
        warnings.push(super::ValidationIssue {
            field: "workflow_id".to_string(),
            message: format!(
                "references workflow '{workflow_id}'; activation refuses a channel whose \
                 workflow is not active"
            ),
        });
    }

    Ok(Json(super::ValidationEnvelope::new(errors, warnings)))
}
