use std::time::Instant;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::errors::OrionError;
use crate::metrics;
use crate::server::state::AppState;

/// Merge metadata key-value pairs into a message's metadata.
pub(crate) fn merge_metadata(message: &mut dataflow_rs::Message, metadata: &Value) {
    if let Some(meta_obj) = metadata.as_object() {
        for (k, v) in meta_obj {
            message.metadata_mut()[k] = v.clone();
        }
    }
}

pub fn data_routes() -> Router<AppState> {
    Router::new()
        .route("/batch", post(batch_process))
        .route("/jobs/{id}", get(get_job))
        .route("/{channel}", post(sync_process))
        .route("/{channel}/async", post(async_submit))
}

// ============================================================
// Synchronous Processing
// ============================================================

#[derive(Deserialize)]
struct ProcessRequest {
    data: Value,
    #[serde(default)]
    metadata: Value,
}

async fn sync_process(
    State(state): State<AppState>,
    Path(channel): Path<String>,
    Json(req): Json<ProcessRequest>,
) -> Result<Json<Value>, OrionError> {
    let start = Instant::now();
    tracing::debug!(channel = %channel, "Processing sync message");

    let engine = state.engine.read().await;

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
                "data": message.data(),
                "errors": message.errors.iter().map(|e| format!("{:?}", e)).collect::<Vec<_>>(),
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

async fn async_submit(
    State(state): State<AppState>,
    Path(channel): Path<String>,
    Json(req): Json<ProcessRequest>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    tracing::debug!(channel = %channel, "Submitting async job");

    let job = state.job_repo.create_data_job(&channel).await?;
    let job_id = job.id.clone();

    state
        .job_queue
        .submit(crate::queue::QueueMessage {
            job_id: job_id.clone(),
            channel,
            payload: req.data,
            metadata: req.metadata,
        })
        .await?;

    Ok((StatusCode::ACCEPTED, Json(json!({ "job_id": job_id }))))
}

// ============================================================
// Job Polling
// ============================================================

async fn get_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, OrionError> {
    let job = state.job_repo.get_by_id(&id).await?;

    let mut response = json!({
        "id": job.id,
        "status": job.status,
        "created_at": job.created_at.to_string(),
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
        response["started_at"] = json!(started.to_string());
    }
    if let Some(ref completed) = job.completed_at {
        response["completed_at"] = json!(completed.to_string());
    }

    Ok(Json(response))
}

// ============================================================
// Batch Processing
// ============================================================

#[derive(Deserialize)]
struct BatchRequest {
    messages: Vec<BatchMessage>,
}

#[derive(Deserialize)]
struct BatchMessage {
    channel: String,
    data: Value,
    #[serde(default)]
    metadata: Value,
}

async fn batch_process(
    State(state): State<AppState>,
    Json(req): Json<BatchRequest>,
) -> Result<Json<Value>, OrionError> {
    let max_batch = state.config.ingest.batch_size;
    if req.messages.len() > max_batch {
        return Err(OrionError::BadRequest(format!(
            "Batch size {} exceeds maximum of {}",
            req.messages.len(),
            max_batch
        )));
    }
    tracing::debug!(count = req.messages.len(), "Processing batch");

    let engine = state.engine.read().await;
    let mut results = Vec::with_capacity(req.messages.len());

    for msg in &req.messages {
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

                results.push(json!({
                    "id": message.id,
                    "data": message.data(),
                    "errors": message.errors.iter().map(|e| format!("{:?}", e)).collect::<Vec<_>>(),
                    "status": "ok",
                }));
            }
            Err(e) => {
                metrics::record_message(&msg.channel, "error");
                metrics::record_error("engine");

                results.push(json!({
                    "id": message.id,
                    "status": "error",
                    "error": e.to_string(),
                }));
            }
        }
    }

    Ok(Json(json!({ "results": results })))
}
