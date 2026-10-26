use axum::Json;
use axum::extract::State;

use crate::errors::OrionError;
use crate::extract::OrionQuery;
use crate::routes::response_helpers::paginated_response;
use crate::runtime::state::AppState;
use crate::storage::models::AuditLogEntryResponse;
use crate::storage::repositories::audit_logs::AuditLogFilter;
use orion_api::Paginated;

// ============================================================
// Audit Logs
// ============================================================

#[utoipa::path(
    get,
    path = "/api/v2/admin/audit-logs",
    tag = "Audit",
    params(AuditLogFilter),
    responses(
        (status = 200, description = "Paginated audit log entries", body = Paginated<AuditLogEntryResponse>),
        (status = 400, description = "Unknown query parameter or malformed timestamp"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_audit_logs(
    State(state): State<AppState>,
    OrionQuery(filter): OrionQuery<AuditLogFilter>,
) -> Result<Json<Paginated<AuditLogEntryResponse>>, OrionError> {
    let page = state.repos.audit_logs.list_paginated(&filter).await?;
    let rows: Vec<AuditLogEntryResponse> =
        page.data.iter().map(AuditLogEntryResponse::from).collect();

    Ok(paginated_response(
        rows,
        page.total,
        page.limit,
        page.offset,
    ))
}
