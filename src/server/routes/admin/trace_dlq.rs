use axum::extract::{Path, State};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::errors::OrionError;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::extract::{OrionJson, OrionQuery};
use crate::server::routes::openapi::{DataEnvelope, DlqPurgeResult, PaginatedEnvelope};
use crate::server::routes::response_helpers::{data_response, paginated_response};
use crate::server::state::AppState;
use crate::storage::models::{TraceDlqEntryResponse, TraceDlqSummaryResponse};
use crate::storage::repositories::trace_dlq::TraceDlqFilter;

use super::audit_log;

// ============================================================
// Trace DLQ (O4)
// ============================================================

#[derive(Debug, Deserialize)]
pub(crate) struct TraceDlqQuery {
    #[serde(default)]
    channel: Option<String>,
    /// `true` = only entries that have given up, `false` = only entries still
    /// scheduled for retry, absent = both.
    #[serde(default)]
    exhausted: Option<bool>,
    #[serde(default)]
    offset: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(crate) struct PurgeTraceDlqRequest {
    /// Age cut-off in hours: exhausted entries whose `failed_at` is older than
    /// this are deleted. Required rather than defaulted — purging is
    /// destructive and an omitted age must not silently mean "everything".
    #[schema(example = 168)]
    older_than_hours: u64,
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/trace-dlq",
    tag = "Trace DLQ",
    params(
        ("channel" = Option<String>, Query, description = "Filter by channel name"),
        ("exhausted" = Option<bool>, Query, description = "true = only exhausted entries, false = only entries awaiting retry"),
        ("offset" = Option<i64>, Query, description = "Pagination offset (default 0)"),
        ("limit" = Option<i64>, Query, description = "Page size, clamped to [1, 1000] (default 50)"),
    ),
    responses(
        // D28: the summary, not the full entry. The listing has never carried
        // `payload_json` / `metadata_json`, but the spec advertised them here
        // because the row struct that *did* have them was also the wire type.
        (status = 200, description = "Paginated DLQ entries without payloads — fetch one by id for the payload", body = PaginatedEnvelope<TraceDlqSummaryResponse>),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_trace_dlq(
    State(state): State<AppState>,
    OrionQuery(params): OrionQuery<TraceDlqQuery>,
) -> Result<Json<Value>, OrionError> {
    let result = state
        .trace_dlq_repo
        .list_paginated(&TraceDlqFilter {
            channel: params.channel,
            exhausted: params.exhausted,
            limit: params.limit,
            offset: params.offset,
        })
        .await?;

    let rows: Vec<TraceDlqSummaryResponse> = result
        .data
        .iter()
        .map(TraceDlqSummaryResponse::from)
        .collect();
    Ok(paginated_response(
        rows,
        result.total,
        result.limit,
        result.offset,
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/trace-dlq/{id}",
    tag = "Trace DLQ",
    params(("id" = String, Path, description = "DLQ entry id")),
    responses(
        (status = 200, description = "DLQ entry including the failed payload and metadata", body = DataEnvelope<TraceDlqEntryResponse>),
        (status = 404, description = "No such DLQ entry", body = crate::server::routes::openapi::ErrorResponse),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn get_trace_dlq_entry(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, OrionError> {
    let entry = state.trace_dlq_repo.get_by_id(&id).await?;
    Ok(data_response(TraceDlqEntryResponse::from(&entry)))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/trace-dlq/{id}/requeue",
    tag = "Trace DLQ",
    params(("id" = String, Path, description = "DLQ entry id")),
    responses(
        (status = 200, description = "Entry reset to retry_count = 0 and scheduled for immediate retry", body = DataEnvelope<TraceDlqEntryResponse>),
        (status = 404, description = "No such DLQ entry", body = crate::server::routes::openapi::ErrorResponse),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn requeue_trace_dlq_entry(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, OrionError> {
    let entry = state.trace_dlq_repo.requeue(&id).await?;
    audit_log(&state.audit_queue, &principal, "requeue", "trace_dlq", &id);
    Ok(data_response(TraceDlqEntryResponse::from(&entry)))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/trace-dlq/purge",
    tag = "Trace DLQ",
    request_body = PurgeTraceDlqRequest,
    responses(
        (status = 200, description = "Exhausted entries older than `older_than_hours` deleted", body = DataEnvelope<DlqPurgeResult>),
        (status = 400, description = "Missing or malformed `older_than_hours`", body = crate::server::routes::openapi::ErrorResponse),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn purge_trace_dlq(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    OrionJson(req): OrionJson<PurgeTraceDlqRequest>,
) -> Result<Json<Value>, OrionError> {
    let purged = state
        .trace_dlq_repo
        .purge_exhausted(req.older_than_hours)
        .await?;
    audit_log(
        &state.audit_queue,
        &principal,
        "purge",
        "trace_dlq",
        &format!("older_than_hours={}", req.older_than_hours),
    );
    Ok(data_response(json!({
        "purged": purged,
        "older_than_hours": req.older_than_hours,
    })))
}
