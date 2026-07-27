use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde_json::Value;

use crate::errors::OrionError;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::extract::{OrionJson, OrionQuery};
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
        (status = 200, description = "Paginated list of channels"),
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
        (status = 201, description = "Channel created as draft"),
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
        (status = 200, description = "Channel details"),
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
        (status = 200, description = "Draft channel updated"),
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
        (status = 200, description = "Status updated"),
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
        StatusAction::Activate => state.channel_repo.activate(&id).await?,
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
        (status = 200, description = "Paginated version history"),
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

    let limit = filter.limit.unwrap_or(50);
    let offset = filter.offset.unwrap_or(0);
    let result = state.channel_repo.list_versions(&id, limit, offset).await?;
    paginated_into(result, |c| ChannelResponse::try_from(c))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/channels/{id}/versions",
    tag = "Channels",
    params(("id" = String, Path, description = "Channel ID")),
    responses(
        (status = 201, description = "New draft version created"),
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
    params(super::workflows::ImportQuery),
    responses(
        (status = 200, description = "Import results with counts (or would-be results when ?dry_run=true). \
            Dry-run validates each item's shape and values only — it does NOT read the database, so it \
            cannot detect name conflicts. Channels whose names already exist are reported as would_create \
            and will surface as Conflict on the real (non-dry-run) import."),
    )
)]
#[tracing::instrument(skip(state, items, principal), fields(count = items.len()))]
pub(crate) async fn import_channels(
    State(state): State<AppState>,
    OrionQuery(query): OrionQuery<super::workflows::ImportQuery>,
    principal: Option<Extension<AdminPrincipal>>,
    OrionJson(items): OrionJson<Vec<Value>>,
) -> Result<Json<Value>, OrionError> {
    // Each create runs through the same validation + persistence as the
    // singular POST endpoint, so behavior matches.
    let repo = state.channel_repo.clone();
    let (imported, failed, errors) = super::import_items::<CreateChannelRequest, _, _, _>(
        items,
        query.dry_run,
        crate::validation::validate_create_channel,
        |ch| {
            let repo = repo.clone();
            async move { repo.create(&ch).await.map(|_| ()) }
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
