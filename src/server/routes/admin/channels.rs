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
    if query.dry_run {
        let envelope = dry_run_status_change(&state, &id, &action).await?;
        return Ok(Json(serde_json::to_value(envelope)?));
    }
    let channel = match action {
        StatusAction::Activate => {
            // R7: refuse a channel whose route another active channel already
            // claims. Same shape as R5/F52 gating workflow activation — the
            // question is fully answerable here, and answering it later means
            // answering it wrong: the loser's declared path resolves to the
            // winner's workflow, which is a wrong answer rather than an error.
            let draft = state.repos.channels.get_by_id(&id).await?;
            ensure_route_is_unclaimed(&state, &draft).await?;
            // K8: and refuse a channel whose workflow cannot serve. This gate
            // was documented (and relied on by the promotion flow's ordering)
            // before it existed in code — the failure used to surface later,
            // as a reload-time quarantine with no error to the caller.
            ensure_channel_workflow_is_active(&state, &draft).await?;
            // K7: and a name another *active* channel holds. Create/update
            // refuse the collision at write time; this catches rows written
            // before that gate existed, at the moment the collision would
            // start losing requests.
            ensure_name_is_active_unclaimed(&state, &draft).await?;
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

/// K8: a channel activates only when the workflow it names has an active
/// version — the condition `engine/loader.rs` otherwise enforces later, as a
/// quarantine the activating caller never sees.
///
/// The channel-with-no-`workflow_id` case is refused too: the loader
/// quarantines it identically, so activating it can never serve a request.
async fn ensure_channel_workflow_is_active(
    state: &AppState,
    draft: &crate::storage::models::Channel,
) -> Result<(), OrionError> {
    let Some(workflow_id) = draft
        .workflow_id
        .as_deref()
        .filter(|w| !w.trim().is_empty())
    else {
        return Err(OrionError::validation(format!(
            "Cannot activate channel '{}': it names no workflow_id, so it would be \
             quarantined at load and never serve. Set workflow_id first.",
            draft.name
        )));
    };
    let has_active = state
        .repos
        .workflows
        .list_active()
        .await?
        .iter()
        .any(|w| w.workflow_id == workflow_id);
    if !has_active {
        let detail = match state.repos.workflows.get_by_id(workflow_id).await {
            Ok(_) => "has no active version",
            Err(OrionError::NotFound(_)) => "does not exist",
            Err(e) => return Err(e),
        };
        return Err(OrionError::validation(format!(
            "Cannot activate channel '{}': workflow '{workflow_id}' {detail} — \
             activate the workflow first",
            draft.name
        )));
    }
    Ok(())
}

/// K7: the activation half of the unique-name rule — a name held by another
/// **active** channel loses the registry slot to the incumbent, so the
/// activation is refused. The write-time gate (`ensure_name_unclaimed` in the
/// repository) keeps new collisions out; this one catches rows that predate
/// it.
async fn ensure_name_is_active_unclaimed(
    state: &AppState,
    draft: &crate::storage::models::Channel,
) -> Result<(), OrionError> {
    for other in state.repos.channels.list_active().await? {
        if other.channel_id != draft.channel_id && other.name == draft.name {
            return Err(OrionError::Conflict(format!(
                "Cannot activate channel '{}': active channel id '{}' already uses \
                 that name, and the data plane addresses channels by name. Rename one \
                 of the two first.",
                draft.name, other.channel_id
            )));
        }
    }
    Ok(())
}

/// K3: every gate the real transition runs, as findings instead of failures —
/// the same functions the un-dry-run path calls, so `valid: true` cannot
/// drift from "the real request would succeed". "Not found" arrives as an
/// `errors` entry in a 200, so a CLI can pre-flight a whole package without
/// tripping over the first missing entity.
async fn dry_run_status_change(
    state: &AppState,
    id: &str,
    action: &StatusAction,
) -> Result<super::ValidationEnvelope, OrionError> {
    let mut errors = Vec::new();
    let warnings = Vec::new();

    match action {
        StatusAction::Activate => match state.repos.channels.get_by_id(id).await {
            Ok(latest) => {
                if latest.status != crate::storage::models::EntityStatus::Draft.as_str() {
                    errors.push(super::ValidationIssue {
                        field: "status".to_string(),
                        message: format!(
                            "No draft version found for channel '{id}' — create a new \
                             version first"
                        ),
                    });
                } else {
                    if let Err(e) = ensure_route_is_unclaimed(state, &latest).await {
                        errors.extend(super::issues_from_error(e));
                    }
                    if let Err(e) = ensure_channel_workflow_is_active(state, &latest).await {
                        errors.extend(super::issues_from_error(e));
                    }
                    if let Err(e) = ensure_name_is_active_unclaimed(state, &latest).await {
                        errors.extend(super::issues_from_error(e));
                    }
                }
            }
            Err(OrionError::NotFound(_)) => errors.push(super::ValidationIssue {
                field: "(root)".to_string(),
                message: format!("Channel '{id}' not found"),
            }),
            Err(e) => return Err(e),
        },
        StatusAction::Archive => {
            let has_active = state
                .repos
                .channels
                .list_active()
                .await?
                .iter()
                .any(|c| c.channel_id == id);
            if !has_active {
                errors.push(super::ValidationIssue {
                    field: "status".to_string(),
                    message: format!("No active version found for channel '{id}'"),
                });
            }
        }
    }

    Ok(super::ValidationEnvelope::new(errors, warnings))
}

/// R7: refuse to activate a channel whose (method × path) another **active**
/// channel already claims.
///
/// `RouteTable::match_route` returns the first hit, so a second claimant is
/// simply dead: requests to its declared path run the incumbent's workflow.
/// Before this the tie broke on DB row order, so which one served the route
/// could differ per node and change on any reload. The incumbent wins by
/// construction here, which is why this is an activation gate rather than a
/// reload-time quarantine — adding a channel must never take a running one down.
///
/// What counts as a declared route is `routing::declared_route` — the same
/// projection `RouteTable::build` uses, so this gate cannot come to disagree
/// with the table that serves the route.
async fn ensure_route_is_unclaimed(
    state: &AppState,
    draft: &crate::storage::models::Channel,
) -> Result<(), OrionError> {
    use crate::channel::routing::declared_route;
    let Some((route, methods)) = declared_route(draft) else {
        return Ok(());
    };
    for other in state.repos.channels.list_active().await? {
        if other.channel_id == draft.channel_id {
            continue; // a new version of this same channel replaces itself
        }
        let Some((other_route, other_methods)) = declared_route(&other) else {
            continue;
        };
        if other_route == route
            && other.priority == draft.priority
            && crate::channel::routing::methods_overlap(&methods, &other_methods)
        {
            return Err(OrionError::validation(format!(
                "Cannot activate channel '{}': active channel '{}' (id {}) already \
                 claims {} {route} at priority {}. Requests to that path would run one \
                 of the two arbitrarily. Change the route_pattern, narrow the methods, \
                 or give one a higher priority.",
                draft.name,
                other.name,
                other.channel_id,
                if methods.is_empty() {
                    "every method on".to_string()
                } else {
                    methods.join("/")
                },
                draft.priority,
            )));
        }
    }
    Ok(())
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
    // singular POST endpoint, so behavior matches.
    super::check_import_batch_size(items.len())?;
    let repo = state.repos.channels.clone();
    let probe = state.repos.channels.clone();
    let upsert_repo = state.repos.channels.clone();
    let outcome = super::import_items::<CreateChannelRequest, _, _, _, _, _, _, _, _>(
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
                async move { upsert_channel(repo.as_ref(), ch, dry_run).await }
            },
        },
    )
    .await;
    if query.dry_run {
        return Ok(super::import_response(true, outcome));
    }
    // K5: one row per written entity, plus the batch summary row.
    for (id, _action) in outcome.written() {
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

/// K2: one channel item under `on_conflict=new_version` — the same shape as
/// `upsert_workflow`, over the channel repository's verbs. An **archived**
/// channel with identical content still gets a new draft version, because
/// re-activating it needs a draft.
async fn upsert_channel(
    repo: &dyn crate::storage::repositories::channels::ChannelRepository,
    req: CreateChannelRequest,
    dry_run: bool,
) -> Result<super::ImportAction, OrionError> {
    use super::ImportAction;
    use crate::storage::models::EntityStatus;

    let Some(id) = req.channel_id.clone() else {
        if !dry_run {
            repo.create(&req).await?;
        }
        return Ok(ImportAction::Created);
    };
    let latest = match repo.get_by_id(&id).await {
        Ok(latest) => latest,
        Err(OrionError::NotFound(_)) => {
            if !dry_run {
                repo.create(&req).await?;
            }
            return Ok(ImportAction::Created);
        }
        Err(e) => return Err(e),
    };

    let identical = crate::storage::content::channel_content(&latest)?
        == crate::storage::content::channel_request_content(&req);
    if latest.status == EntityStatus::Draft.as_str() {
        if identical {
            return Ok(ImportAction::Unchanged);
        }
        if !dry_run {
            repo.replace_draft(&id, &req).await?;
        }
        Ok(ImportAction::UpdatedDraft)
    } else if identical && latest.status == EntityStatus::Active.as_str() {
        Ok(ImportAction::Unchanged)
    } else {
        if !dry_run {
            repo.create_new_version(&id).await?;
            repo.replace_draft(&id, &req).await?;
        }
        Ok(ImportAction::NewVersion)
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
