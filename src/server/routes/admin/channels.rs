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
use crate::storage::models::{ChannelResponse, StatusAction};
use crate::storage::repositories::channels::{
    ChannelFilter, ChannelStatusChangeRequest, CreateChannelRequest, UpdateChannelRequest,
};

use super::VersionFilter;
use super::audit_and_reload;
use super::audit_log_draft_only;

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
    let result = state.channel_repo.list_paginated(&filter).await?;
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
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn create_channel(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    OrionJson(req): OrionJson<CreateChannelRequest>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    crate::validation::validate_create_channel(&req)?;
    let channel = state.channel_repo.create(&req).await?;
    audit_log_draft_only(
        &state.audit_log_repo,
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
    let channel = state.channel_repo.get_by_id(&id).await?;
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
        (status = 400, description = "No draft version or invalid input"),
        (status = 404, description = "Channel not found"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn update_channel(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
    OrionJson(req): OrionJson<UpdateChannelRequest>,
) -> Result<Json<Value>, OrionError> {
    // R3: mirror create-time validation. The latest version (404 if the
    // channel is absent) supplies the protocol and current field values for
    // the merged-view checks; when a draft exists it is the latest version,
    // i.e. exactly the row `update_draft` mutates.
    let current = state.channel_repo.get_by_id(&id).await?;
    crate::validation::validate_update_channel(&current, &req)?;
    let channel = state.channel_repo.update_draft(&id, &req).await?;
    audit_log_draft_only(&state.audit_log_repo, &principal, "update", "channel", &id);
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
    state.channel_repo.delete(&id).await?;
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
            let draft = state.channel_repo.get_by_id(&id).await?;
            ensure_route_is_unclaimed(&state, &draft).await?;
            state.channel_repo.activate(&id).await?
        }
        StatusAction::Archive => state.channel_repo.archive(&id).await?,
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

/// A channel's declared route, as the route table would see it.
/// `None` for channels that register no route (Kafka, or no `route_pattern`).
fn declared_route(channel: &crate::storage::models::Channel) -> Option<(String, Vec<String>)> {
    use crate::storage::models::ChannelProtocol;
    if channel.protocol != ChannelProtocol::Rest.as_str()
        && channel.protocol != ChannelProtocol::Http.as_str()
    {
        return None;
    }
    let pattern = channel.route_pattern.as_deref()?;
    let methods: Vec<String> = channel
        .methods
        .as_deref()
        .and_then(|m| serde_json::from_str::<Vec<String>>(m).ok())
        .unwrap_or_default();
    Some((crate::channel::routing::canonical_route(pattern), methods))
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
async fn ensure_route_is_unclaimed(
    state: &AppState,
    draft: &crate::storage::models::Channel,
) -> Result<(), OrionError> {
    let Some((route, methods)) = declared_route(draft) else {
        return Ok(());
    };
    for other in state.channel_repo.list_active().await? {
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
    let _ = state.channel_repo.get_by_id(&id).await?;

    let (limit, offset) = filter.limit_offset();
    let result = state.channel_repo.list_versions(&id, limit, offset).await?;
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
    let channel = state.channel_repo.create_new_version(&id).await?;
    audit_log_draft_only(
        &state.audit_log_repo,
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
    let repo = state.channel_repo.clone();
    let probe = state.channel_repo.clone();
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
        &state.audit_log_repo,
        &principal,
        "import",
        "channel",
        &format!("{imported} imported"),
    );
    Ok(super::import_response(imported, failed, errors))
}
