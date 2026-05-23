use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::errors::OrionError;
use crate::server::state::AppState;

// ============================================================
// Audit Logs
// ============================================================

#[derive(Debug, Deserialize)]
pub(crate) struct AuditLogQuery {
    #[serde(default)]
    offset: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/audit-logs",
    tag = "Audit",
    params(
        ("offset" = Option<i64>, Query, description = "Pagination offset (default 0)"),
        ("limit" = Option<i64>, Query, description = "Page size, clamped to [1, 1000] (default 50)"),
    ),
    responses(
        (status = 200, description = "Paginated audit log entries with total count"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_audit_logs(
    State(state): State<AppState>,
    Query(params): Query<AuditLogQuery>,
) -> Result<Json<Value>, OrionError> {
    let offset = params.offset.unwrap_or(0).max(0);
    let limit = params.limit.unwrap_or(50).clamp(1, 1000);

    let entries = state.audit_log_repo.list_paginated(offset, limit).await?;
    let total = state.audit_log_repo.count().await?;

    Ok(Json(json!({
        "data": entries,
        "pagination": {
            "offset": offset,
            "limit": limit,
            "total": total,
        }
    })))
}
