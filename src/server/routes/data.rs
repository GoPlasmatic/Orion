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
use crate::storage::repositories::jobs::JobFilter;

// Re-export merge_metadata from engine utils for backward compatibility
pub(crate) use crate::engine::utils::merge_metadata;

pub fn data_routes() -> Router<AppState> {
    Router::new()
        .route("/batch", post(batch_process))
        .route("/jobs", get(list_jobs))
        .route("/jobs/{id}", get(get_job))
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

    let start = Instant::now();

    // Clone the inner Arc<Engine> and release the lock immediately to avoid
    // holding the read lock across async processing (which blocks engine reloads).
    let engine = state.engine.read().await.clone();

    let mut message = dataflow_rs::Message::from_value(&req.data);
    merge_metadata(&mut message, &req.metadata);

    match engine
        .process_message_for_channel(&channel, &mut message)
        .await
    {
        Ok(()) => {
            let duration = start.elapsed().as_secs_f64();
            metrics::record_message(&channel, "ok");
            metrics::record_message_duration(&channel, duration);

            Ok(Json(json!({
                "id": message.id,
                "status": "ok",
                "data": message.data(),
                "errors": message.errors.iter().filter_map(|e| serde_json::to_value(e).ok()).collect::<Vec<_>>(),
            })))
        }
        Err(e) => {
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
        (status = 202, description = "Job accepted"),
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

    let job = state.job_repo.create_data_job(&channel).await?;
    let job_id = job.id.clone();

    // Capture current trace context so the background job inherits it
    #[cfg(feature = "otel")]
    let trace_headers = {
        let mut headers = std::collections::HashMap::new();
        crate::server::trace_context::inject_trace_context(&mut headers);
        headers
    };

    state
        .job_queue
        .submit(crate::queue::QueueMessage {
            job_id,
            channel,
            payload: req.data,
            metadata: req.metadata,
            #[cfg(feature = "otel")]
            trace_headers,
        })
        .await?;

    Ok((StatusCode::ACCEPTED, Json(json!({ "job_id": job.id }))))
}

// ============================================================
// Job Listing & Polling
// ============================================================

#[utoipa::path(
    get,
    path = "/api/v1/data/jobs",
    tag = "Data",
    params(
        ("status" = Option<String>, Query, description = "Filter by job status"),
        ("channel" = Option<String>, Query, description = "Filter by channel"),
        ("limit" = Option<i64>, Query, description = "Page size"),
        ("offset" = Option<i64>, Query, description = "Page offset"),
    ),
    responses(
        (status = 200, description = "Paginated list of jobs"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_jobs(
    State(state): State<AppState>,
    Query(filter): Query<JobFilter>,
) -> Result<Json<Value>, OrionError> {
    let result = state.job_repo.list_paginated(&filter).await?;
    Ok(Json(json!({
        "data": result.data,
        "total": result.total,
        "limit": result.limit,
        "offset": result.offset,
    })))
}

#[utoipa::path(
    get,
    path = "/api/v1/data/jobs/{id}",
    tag = "Data",
    params(("id" = String, Path, description = "Job ID")),
    responses(
        (status = 200, description = "Job status and result"),
        (status = 404, description = "Job not found"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn get_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, OrionError> {
    let job = state.job_repo.get_by_id(&id).await?;

    let mut response = json!({
        "id": job.id,
        "status": job.status,
        "created_at": job.created_at,
    });

    use crate::storage::models;
    if job.status == models::JOB_STATUS_COMPLETED {
        if let Some(ref result_str) = job.result_json
            && let Ok(result_val) = serde_json::from_str::<Value>(result_str)
        {
            response["result"] = result_val;
        }
    } else if job.status == models::JOB_STATUS_FAILED
        && let Some(ref err) = job.error_message
    {
        response["error"] = json!(err);
    }

    if let Some(ref started) = job.started_at {
        response["started_at"] = json!(started);
    }
    if let Some(ref completed) = job.completed_at {
        response["completed_at"] = json!(completed);
    }

    Ok(Json(response))
}

// ============================================================
// Batch Processing
// ============================================================

#[derive(Deserialize, utoipa::ToSchema)]
pub(crate) struct BatchRequest {
    messages: Vec<BatchMessage>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub(crate) struct BatchMessage {
    channel: String,
    data: Value,
    #[serde(default)]
    metadata: Value,
}

#[utoipa::path(
    post,
    path = "/api/v1/data/batch",
    tag = "Data",
    request_body = BatchRequest,
    responses(
        (status = 200, description = "Batch processing results"),
        (status = 400, description = "Invalid batch input"),
    )
)]
#[tracing::instrument(skip(state, req), fields(count))]
pub(crate) async fn batch_process(
    State(state): State<AppState>,
    Json(req): Json<BatchRequest>,
) -> Result<Json<Value>, OrionError> {
    if req.messages.is_empty() {
        return Err(OrionError::BadRequest(
            "Batch must contain at least one message".into(),
        ));
    }
    let max_batch = state.config.ingest.batch_size;
    if req.messages.len() > max_batch {
        return Err(OrionError::BadRequest(format!(
            "Batch size {} exceeds maximum of {}",
            req.messages.len(),
            max_batch
        )));
    }
    tracing::Span::current().record("count", req.messages.len());

    let engine = state.engine.read().await.clone();

    let mut handles = Vec::with_capacity(req.messages.len());
    for msg in req.messages {
        let engine = engine.clone();
        handles.push(tokio::spawn(async move {
            let start = Instant::now();
            let mut message = dataflow_rs::Message::from_value(&msg.data);
            merge_metadata(&mut message, &msg.metadata);

            match engine
                .process_message_for_channel(&msg.channel, &mut message)
                .await
            {
                Ok(()) => {
                    let duration = start.elapsed().as_secs_f64();
                    metrics::record_message(&msg.channel, "ok");
                    metrics::record_message_duration(&msg.channel, duration);

                    json!({
                        "id": message.id,
                        "data": message.data(),
                        "errors": message.errors.iter().filter_map(|e| serde_json::to_value(e).ok()).collect::<Vec<_>>(),
                        "status": "ok",
                    })
                }
                Err(e) => {
                    metrics::record_message(&msg.channel, "error");
                    metrics::record_error("engine");

                    json!({
                        "id": message.id,
                        "status": "error",
                        "error": e.to_string(),
                    })
                }
            }
        }));
    }

    let mut results = Vec::with_capacity(handles.len());
    for handle in handles {
        match handle.await {
            Ok(result) => results.push(result),
            Err(e) => results.push(json!({
                "status": "error",
                "error": format!("Task join error: {e}"),
            })),
        }
    }

    Ok(Json(json!({ "results": results })))
}
