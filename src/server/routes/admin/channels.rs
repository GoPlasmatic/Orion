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
    audit_and_reload(&state, &principal, "delete", "channel", &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ============================================================
// Channel Status Management
// ============================================================

#[utoipa::path(
    patch,
    path = "/api/v1/admin/channels/{id}/status",
    tag = "Channels",
    params(("id" = String, Path, description = "Channel ID")),
    request_body = ChannelStatusChangeRequest,
    responses(
        (status = 200, description = "Status updated", body = DataEnvelope<ChannelResponse>),
        (status = 400, description = "Invalid status transition"),
        (status = 404, description = "Channel not found"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn change_channel_status(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
    OrionJson(req): OrionJson<ChannelStatusChangeRequest>,
) -> Result<Json<Value>, OrionError> {
    let action = StatusAction::parse(req.status)?;
    let channel = match action {
        StatusAction::Activate => {
            // R7: refuse a channel whose route another active channel already
            // claims. Same shape as R5/F52 gating workflow activation — the
            // question is fully answerable here, and answering it later means
            // answering it wrong: the loser's declared path resolves to the
            // winner's workflow, which is a wrong answer rather than an error.
            let draft = state.repos.channels.get_by_id(&id).await?;
            ensure_route_is_unclaimed(&state, &draft).await?;
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
    )
    .await?;
    Ok(data_response(ChannelResponse::try_from(&channel)?))
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
            conflicts against stored rows and duplicates within the batch, without writing.", body = DataEnvelope<ImportResult>),
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
    let (imported, failed, errors) = super::import_items::<CreateChannelRequest, _, _, _, _, _, _>(
        items,
        query.dry_run,
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
        },
    )
    .await;
    if query.dry_run {
        return Ok(super::dry_run_response(imported, failed, errors));
    }
    audit_log_draft_only(
        &state.audit_queue,
        &principal,
        "import",
        "channel",
        &format!("{imported} imported"),
    );
    Ok(super::import_response(imported, failed, errors))
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
    let repo = state.repos.channels.as_ref();
    let rows = super::collect_pages(super::EXPORT_PAGE_SIZE, |limit, offset| {
        let page_filter = ChannelFilter {
            status: filter.status.clone(),
            channel_type: filter.channel_type.clone(),
            protocol: filter.protocol.clone(),
            limit: Some(limit),
            offset: Some(offset),
            ..Default::default()
        };
        async move { Ok(repo.list_paginated(&page_filter).await?.data) }
    })
    .await?;

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
