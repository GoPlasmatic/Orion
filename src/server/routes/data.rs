use std::time::Instant;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::errors::OrionError;
use crate::metrics;
use crate::server::state::AppState;
use crate::storage::repositories::traces::TraceFilter;

// Re-export from engine utils for backward compatibility
pub(crate) use crate::engine::utils::merge_metadata;
use crate::engine::utils::{inject_rollout_bucket, remove_rollout_bucket};

pub fn data_routes() -> Router<AppState> {
    Router::new()
        .route("/traces", get(list_traces))
        .route("/traces/{id}", get(get_trace))
        .route("/{channel}", post(sync_process))
        .route("/{channel}/async", post(async_submit))
}

// ============================================================
// Synchronous Processing
// ============================================================

#[derive(Deserialize, utoipa::ToSchema)]
pub(crate) struct ProcessRequest {
    data: Value,
    #[serde(default)]
    metadata: Value,
}

#[utoipa::path(
    post,
    path = "/api/v1/data/{channel}",
    tag = "Data",
    params(("channel" = String, Path, description = "Channel name")),
    request_body = ProcessRequest,
    responses(
        (status = 200, description = "Processing result"),
        (status = 400, description = "Invalid input"),
    )
)]
#[tracing::instrument(skip(state, req), fields(channel = %channel))]
pub(crate) async fn sync_process(
    State(state): State<AppState>,
    Path(channel): Path<String>,
    Json(req): Json<ProcessRequest>,
) -> Result<Json<Value>, OrionError> {
    let channel = channel.trim().to_string();
    if channel.is_empty() {
        return Err(OrionError::BadRequest(
            "Channel name must not be empty".into(),
        ));
    }

    let input_json = serde_json::to_string(&req.data).ok();
    let start = Instant::now();

    // Clone the inner Arc<Engine> and release the lock immediately to avoid
    // holding the read lock across async processing (which blocks engine reloads).
    let engine = state.engine.read().await.clone();

    let mut message = dataflow_rs::Message::from_value(&req.data);
    merge_metadata(&mut message, &req.metadata);
    inject_rollout_bucket(&mut message);

    match engine
        .process_message_for_channel(&channel, &mut message)
        .await
    {
        Ok(()) => {
            remove_rollout_bucket(&mut message);
            let duration = start.elapsed();
            let duration_secs = duration.as_secs_f64();
            let duration_ms = duration.as_secs_f64() * 1000.0;
            metrics::record_message(&channel, "ok");
            metrics::record_message_duration(&channel, duration_secs);
            metrics::record_channel_execution(&channel);

            // Store full message in DB
            if let Ok(result_json) = serde_json::to_string(&message) {
                if let Err(e) = state
                    .trace_repo
                    .store_completed(
                        &channel,
                        "sync",
                        input_json.as_deref(),
                        &result_json,
                        duration_ms,
                    )
                    .await
                {
                    tracing::warn!(error = %e, "Failed to store sync processing result");
                }
            }

            Ok(Json(json!({
                "id": message.id,
                "status": "ok",
                "data": message.data(),
                "errors": message.errors.iter().filter_map(|e| serde_json::to_value(e).ok()).collect::<Vec<_>>(),
            })))
        }
        Err(e) => {
            remove_rollout_bucket(&mut message);
            metrics::record_message(&channel, "error");
            metrics::record_error("engine");
            Err(OrionError::Engine(e))
        }
    }
}

// ============================================================
// Asynchronous Processing
// ============================================================

#[utoipa::path(
    post,
    path = "/api/v1/data/{channel}/async",
    tag = "Data",
    params(("channel" = String, Path, description = "Channel name")),
    request_body = ProcessRequest,
    responses(
        (status = 202, description = "Trace accepted"),
        (status = 400, description = "Invalid input"),
    )
)]
#[tracing::instrument(skip(state, req), fields(channel = %channel))]
pub(crate) async fn async_submit(
    State(state): State<AppState>,
    Path(channel): Path<String>,
    Json(req): Json<ProcessRequest>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    let channel = channel.trim().to_string();
    if channel.is_empty() {
        return Err(OrionError::BadRequest(
            "Channel name must not be empty".into(),
        ));
    }

    let input_json = serde_json::to_string(&req.data).ok();
    let trace = state
        .trace_repo
        .create_pending(&channel, "async", input_json.as_deref())
        .await?;
    let trace_id = trace.id.clone();

    // Capture current trace context so the background trace inherits it
    #[cfg(feature = "otel")]
    let trace_headers = {
        let mut headers = std::collections::HashMap::new();
        crate::server::trace_context::inject_trace_context(&mut headers);
        headers
    };

    state
        .trace_queue
        .submit(crate::queue::QueueMessage {
            trace_id,
            channel,
            payload: req.data,
            metadata: req.metadata,
            #[cfg(feature = "otel")]
            trace_headers,
        })
        .await?;

    Ok((StatusCode::ACCEPTED, Json(json!({ "trace_id": trace.id }))))
}

// ============================================================
// Trace Listing & Polling
// ============================================================

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
    Query(filter): Query<TraceFilter>,
) -> Result<Json<Value>, OrionError> {
    let result = state.trace_repo.list_paginated(&filter).await?;
    Ok(Json(json!({
        "data": result.data,
        "total": result.total,
        "limit": result.limit,
        "offset": result.offset,
    })))
}

#[utoipa::path(
    get,
    path = "/api/v1/data/traces/{id}",
    tag = "Data",
    params(("id" = String, Path, description = "Trace ID")),
    responses(
        (status = 200, description = "Trace status and result"),
        (status = 404, description = "Trace not found"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn get_trace(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, OrionError> {
    let trace = state.trace_repo.get_by_id(&id).await?;

    let mut response = json!({
        "id": trace.id,
        "status": trace.status,
        "mode": trace.mode,
        "created_at": trace.created_at,
    });

    use crate::storage::models;
    if trace.status == models::TRACE_STATUS_COMPLETED {
        if let Some(ref result_str) = trace.result_json
            && let Ok(result_val) = serde_json::from_str::<Value>(result_str)
        {
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

    Ok(Json(response))
}
