//! Trace listing & polling: the payload-free list projection (S14) and the
//! token- or admin-gated single-trace read (R12).

use axum::Json;
use axum::extract::{Path, State};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::errors::OrionError;
use crate::server::extract::OrionQuery;
// Referenced by the `#[utoipa::path]` `body = ErrorResponse` annotations below.
use crate::server::routes::openapi::ErrorResponse;
use crate::server::state::AppState;
use crate::storage::repositories::traces::TraceFilter;

#[utoipa::path(
    get,
    path = "/api/v1/data/traces",
    tag = "Data",
    params(
        ("status" = Option<String>, Query, description = "Filter by trace status"),
        ("channel" = Option<String>, Query, description = "Filter by channel"),
        ("mode" = Option<String>, Query, description = "Filter by mode: sync, async"),
        ("limit" = Option<i64>, Query, description = "Page size (default 50, max 1000)"),
        ("offset" = Option<i64>, Query, description = "Page offset"),
        ("sort_by" = Option<String>, Query, description = "Sort column: created_at (default), updated_at, status, channel, mode"),
        ("sort_order" = Option<String>, Query, description = "Sort direction: asc or desc (default)"),
    ),
    responses(
        (status = 200, description = "Paginated list of traces"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_traces(
    State(state): State<AppState>,
    OrionQuery(filter): OrionQuery<TraceFilter>,
) -> Result<Json<Value>, OrionError> {
    let result = state.trace_repo.list_paginated(&filter).await?;
    // Payload-free projection (S14): `input_json` holds the caller's request
    // body and `result_json`/`task_trace_json` the full engine message, so a
    // list row is every caller's traffic in one response — including rows
    // persisted before S10 masked credential headers. Payloads are served
    // one trace at a time by `GET /traces/{id}`, mirroring the DLQ list.
    let rows: Vec<Value> = result
        .data
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "channel": t.channel,
                "channel_id": t.channel_id,
                "mode": t.mode,
                "status": t.status,
                "error_message": t.error_message,
                "duration_ms": t.duration_ms,
                "started_at": t.started_at,
                "completed_at": t.completed_at,
                "created_at": t.created_at,
                "updated_at": t.updated_at,
            })
        })
        .collect();
    Ok(Json(json!({
        "data": rows,
        "total": result.total,
        "limit": result.limit,
        "offset": result.offset,
    })))
}

/// Query parameters for `GET /traces/{id}`.
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct TraceAccessQuery {
    /// The capability token returned with the async 202. Alternative to the
    /// `x-trace-token` header for clients that cannot set headers.
    token: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/v1/data/traces/{id}",
    tag = "Data",
    description = "\
Fetch one trace. Access follows a two-lane rule (R12): present either a \
valid admin credential, or — for async submissions — the `trace_token` \
returned with the 202, via the `x-trace-token` header or `?token=` query \
parameter. Traces without a token (sync traces, DLQ retries, rows from \
before 1.0.1) are admin-plane only when admin auth is enabled.",
    params(
        ("id" = String, Path, description = "Trace ID"),
        TraceAccessQuery,
    ),
    responses(
        (status = 200, description = "Trace status and result"),
        (status = 401, description = "Missing or wrong trace token / admin credential", body = ErrorResponse),
        (status = 404, description = "Trace not found", body = ErrorResponse),
    )
)]
#[tracing::instrument(skip(state, headers, query))]
pub(crate) async fn get_trace(
    State(state): State<AppState>,
    Path(id): Path<String>,
    OrionQuery(query): OrionQuery<TraceAccessQuery>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Value>, OrionError> {
    let trace = state.trace_repo.get_by_id(&id).await?;

    // R12 access rule. Lane 1: a valid admin credential (only meaningful when
    // admin auth is enabled — the middleware no longer guards this route, so
    // the check happens here). Lane 2: the per-submission capability token.
    // Tokenless traces stay on the admin trust model: open when auth is
    // disabled (the whole admin plane is), admin-only when enabled.
    //
    // A missing trace 404s before this check, so an unauthorized caller can
    // distinguish "exists" from "does not exist". Deliberate: trace ids are
    // v4 UUIDs, so there is nothing to enumerate (the id-listing endpoint is
    // admin-guarded, and its rows carry no payloads since S14), and the
    // distinction is what makes a wrong-id-vs-wrong-token mistake debuggable.
    let auth_cfg = &state.config.admin_auth;
    let is_admin = auth_cfg.enabled
        && crate::server::admin_auth::headers_present_valid_key(&headers, auth_cfg);
    if !is_admin {
        let presented = headers
            .get("x-trace-token")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .or_else(|| query.token.clone());
        let allowed = match trace.access_token_hash.as_deref() {
            Some(stored) => presented
                .as_deref()
                .is_some_and(|t| crate::server::admin_auth::trace_token_matches(t, stored)),
            None => !auth_cfg.enabled,
        };
        if !allowed {
            return Err(OrionError::Unauthorized(
                "This trace requires its trace_token (returned with the async 202) or an admin credential".into(),
            ));
        }
    }

    let mut response = json!({
        "id": trace.id,
        "status": trace.status,
        "mode": trace.mode,
        "channel": trace.channel,
        "channel_id": trace.channel_id,
        "created_at": trace.created_at,
    });

    use crate::storage::models;
    if trace.status == models::TRACE_STATUS_COMPLETED {
        if let Some(ref result_str) = trace.result_json
            && let Ok(mut result_val) = serde_json::from_str::<Value>(result_str)
        {
            // S14: the stored message's `context.metadata` carries the
            // request headers (masked since S10 — but rows persisted before
            // that upgrade hold them in plaintext). Strip it from the read
            // projection; pollers need `data`/`payload`, not the submitter's
            // request context.
            if let Some(ctx) = result_val.get_mut("context").and_then(Value::as_object_mut) {
                ctx.remove("metadata");
            }
            response["message"] = result_val;
        }
    } else if trace.status == models::TRACE_STATUS_FAILED
        && let Some(ref err) = trace.error_message
    {
        response["error"] = json!(err);
    }

    if let Some(ref started) = trace.started_at {
        response["started_at"] = json!(started);
    }
    if let Some(ref completed) = trace.completed_at {
        response["completed_at"] = json!(completed);
    }
    if let Some(duration) = trace.duration_ms {
        response["duration_ms"] = json!(duration);
    }
    if let Some(ref tt) = trace.task_trace_json
        && let Ok(v) = serde_json::from_str::<Value>(tt)
    {
        response["task_trace_json"] = v;
    }

    Ok(Json(response))
}
