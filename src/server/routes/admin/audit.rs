use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::errors::OrionError;
use crate::server::extract::OrionQuery;
use crate::server::state::AppState;
use crate::storage::repositories::audit_logs::AuditLogFilter;

// ============================================================
// Audit Logs
// ============================================================

/// `deny_unknown_fields` is deliberate: before O8 the handler accepted only
/// `offset`/`limit`, so `?action=activate` was dropped and the caller got a
/// 200 with unfiltered data. A rejected typo is better than a silently wrong
/// compliance answer.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuditLogQuery {
    #[serde(default)]
    offset: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    resource_type: Option<String>,
    #[serde(default)]
    resource_id: Option<String>,
    #[serde(default)]
    principal: Option<String>,
    /// Inclusive lower bound, RFC 3339 (e.g. `2026-07-01T00:00:00Z`).
    #[serde(default)]
    start_time: Option<String>,
    /// Exclusive upper bound, RFC 3339.
    #[serde(default)]
    end_time: Option<String>,
}

/// Accepts RFC 3339 (`2026-07-01T00:00:00Z`) or a bare naive timestamp
/// (`2026-07-01T00:00:00`); `created_at` is stored timezone-naive in UTC.
fn parse_timestamp(field: &str, raw: &str) -> Result<chrono::NaiveDateTime, OrionError> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Ok(dt.naive_utc());
    }
    chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S"))
        .map_err(|_| {
            OrionError::BadRequest(format!(
                "Invalid `{field}`: expected an RFC 3339 timestamp, got '{raw}'"
            ))
        })
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/audit-logs",
    tag = "Audit",
    params(
        ("offset" = Option<i64>, Query, description = "Pagination offset (default 0)"),
        ("limit" = Option<i64>, Query, description = "Page size, clamped to [1, 1000] (default 50)"),
        ("action" = Option<String>, Query, description = "Exact-match filter on the action (e.g. `create`, `activate`, `delete`)"),
        ("resource_type" = Option<String>, Query, description = "Exact-match filter on the resource type (`workflow`, `channel`, `connector`, `engine`)"),
        ("resource_id" = Option<String>, Query, description = "Exact-match filter on the resource ID"),
        ("principal" = Option<String>, Query, description = "Exact-match filter on the acting principal"),
        ("start_time" = Option<String>, Query, description = "Inclusive lower bound on `created_at`, RFC 3339"),
        ("end_time" = Option<String>, Query, description = "Exclusive upper bound on `created_at`, RFC 3339"),
    ),
    responses(
        (status = 200, description = "Paginated audit log entries with total count"),
        (status = 400, description = "Unknown query parameter or malformed timestamp"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_audit_logs(
    State(state): State<AppState>,
    OrionQuery(params): OrionQuery<AuditLogQuery>,
) -> Result<Json<Value>, OrionError> {
    let filter = AuditLogFilter {
        action: params.action,
        resource_type: params.resource_type,
        resource_id: params.resource_id,
        principal: params.principal,
        start_time: params
            .start_time
            .as_deref()
            .map(|raw| parse_timestamp("start_time", raw))
            .transpose()?,
        end_time: params
            .end_time
            .as_deref()
            .map(|raw| parse_timestamp("end_time", raw))
            .transpose()?,
        limit: params.limit,
        offset: params.offset,
    };

    let page = state.audit_log_repo.list_paginated(&filter).await?;

    Ok(Json(json!({
        "data": page.data,
        "pagination": {
            "offset": page.offset,
            "limit": page.limit,
            "total": page.total,
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rfc3339_and_naive_timestamps() {
        let expected = chrono::NaiveDate::from_ymd_opt(2026, 7, 1)
            .expect("date")
            .and_hms_opt(12, 30, 0)
            .expect("time");
        assert_eq!(
            parse_timestamp("start_time", "2026-07-01T12:30:00Z").expect("rfc3339"),
            expected
        );
        assert_eq!(
            parse_timestamp("start_time", "2026-07-01T12:30:00").expect("naive"),
            expected
        );
        // Offsets are normalized to UTC before comparison with `created_at`.
        assert_eq!(
            parse_timestamp("start_time", "2026-07-01T14:30:00+02:00").expect("offset"),
            expected
        );
    }

    #[test]
    fn rejects_malformed_timestamps() {
        let err = parse_timestamp("end_time", "yesterday").expect_err("must reject");
        assert!(matches!(err, OrionError::BadRequest(_)));
    }
}
