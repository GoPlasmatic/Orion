use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::errors::OrionError;
use crate::server::state::AppState;

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
    let engine = state.engine.read().await;

    let mut message = dataflow_rs::Message::from_value(&req.data);
    if let Some(meta_obj) = req.metadata.as_object() {
        for (k, v) in meta_obj {
            message.metadata_mut()[k] = v.clone();
        }
    }

    engine
        .process_message_for_channel(&channel, &mut message)
        .await
        .map_err(OrionError::Engine)?;

    Ok(Json(json!({
        "id": message.id,
        "data": message.data(),
        "errors": message.errors.iter().map(|e| format!("{:?}", e)).collect::<Vec<_>>(),
    })))
}

// ============================================================
// Asynchronous Processing
// ============================================================

async fn async_submit(
    State(state): State<AppState>,
    Path(channel): Path<String>,
    Json(req): Json<ProcessRequest>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    // We don't have a full job queue yet (Phase 3), so we use a simple
    // spawn-and-track approach: create a job, spawn processing, update on completion.
    let job = state.job_repo.create_data_job(&channel).await?;
    let job_id = job.id.clone();

    // Mark as running
    state
        .job_repo
        .update_status(&job_id, "running", None, None)
        .await?;

    // Spawn background processing
    let engine = state.engine.read().await.clone();
    let job_repo = state.job_repo.clone();
    let payload = req.data.clone();
    let metadata = req.metadata.clone();
    let jid = job_id.clone();

    tokio::spawn(async move {
        let mut message = dataflow_rs::Message::from_value(&payload);
        if let Some(meta_obj) = metadata.as_object() {
            for (k, v) in meta_obj {
                message.metadata_mut()[k] = v.clone();
            }
        }

        match engine
            .process_message_for_channel(&channel, &mut message)
            .await
        {
            Ok(()) => {
                let result = serde_json::to_string(&json!({
                    "id": message.id,
                    "data": message.data(),
                }))
                .unwrap_or_default();

                let _ = job_repo.set_result(&jid, &result).await;
                let _ = job_repo
                    .update_status(&jid, "completed", None, Some(1))
                    .await;
            }
            Err(e) => {
                let _ = job_repo
                    .update_status(&jid, "failed", Some(&e.to_string()), None)
                    .await;
            }
        }
    });

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

    if job.status == "completed" {
        if let Some(ref result_str) = job.result_json
            && let Ok(result_val) = serde_json::from_str::<Value>(result_str) {
                response["result"] = result_val;
            }
    } else if job.status == "failed"
        && let Some(ref err) = job.error_message {
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
    let engine = state.engine.read().await;
    let mut results = Vec::with_capacity(req.messages.len());

    for msg in &req.messages {
        let mut message = dataflow_rs::Message::from_value(&msg.data);
        if let Some(meta_obj) = msg.metadata.as_object() {
            for (k, v) in meta_obj {
                message.metadata_mut()[k] = v.clone();
            }
        }

        match engine
            .process_message_for_channel(&msg.channel, &mut message)
            .await
        {
            Ok(()) => {
                results.push(json!({
                    "id": message.id,
                    "data": message.data(),
                    "errors": message.errors.iter().map(|e| format!("{:?}", e)).collect::<Vec<_>>(),
                    "status": "ok",
                }));
            }
            Err(e) => {
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
