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
    params(TraceDlqFilter),
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
    OrionQuery(filter): OrionQuery<TraceDlqFilter>,
) -> Result<Json<Value>, OrionError> {
    let result = state.repos.trace_dlq.list_paginated(&filter).await?;

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
    let entry = state.repos.trace_dlq.get_by_id(&id).await?;
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
    let entry = state.repos.trace_dlq.requeue(&id).await?;
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
        .repos
        .trace_dlq
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
