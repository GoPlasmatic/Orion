use axum::Json;
use axum::extract::State;

use crate::errors::OrionError;
use crate::extract::OrionQuery;
use crate::routes::response_helpers::page_response;
use crate::runtime::state::AppState;
use crate::storage::models::AuditLogEntryResponse;
use crate::storage::repositories::audit_logs::AuditLogFilter;
use orion_api::Page;

// ============================================================
// Audit Logs
// ============================================================

#[utoipa::path(
    get,
    path = "/api/v2/admin/audit-logs",
    tag = "Audit",
    params(AuditLogFilter),
    responses(
        (status = 200, description = "Paginated audit log entries", body = Page<AuditLogEntryResponse>),
        (status = 400, description = "Unknown query parameter or malformed timestamp"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_audit_logs(
    State(state): State<AppState>,
    OrionQuery(filter): OrionQuery<AuditLogFilter>,
) -> Result<Json<Page<AuditLogEntryResponse>>, OrionError> {
    let page = state.repos.audit_logs.list_paginated(&filter).await?;
    let rows: Vec<AuditLogEntryResponse> =
        page.data.iter().map(AuditLogEntryResponse::from).collect();

    Ok(page_response(
        rows,
        page.limit,
        page.next_cursor,
        page.total,
    ))
}
