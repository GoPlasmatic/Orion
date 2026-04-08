use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
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

/// JSONLogic truthiness: false, null, 0, "", and [] are falsy; everything else is truthy.
fn is_truthy(val: &serde_json::Value) -> bool {
    match val {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(_) => true,
    }
}

pub fn data_routes() -> Router<AppState> {
    Router::new()
        .route("/traces", get(list_traces))
        .route("/traces/{id}", get(get_trace))
        // Catch-all: handles simple HTTP channels (/{channel}),
        // async submissions (/{channel}/async), and REST routes (/{path...}).
        .route("/{*path}", any(dynamic_handler))
}

// ============================================================
// Unified Dynamic Route Handler
// ============================================================

/// Unified handler for all data routes. Handles:
/// - Simple HTTP channels: `POST /{channel}` (single segment, direct name match)
/// - Async submissions: `POST /{channel}/async` or `POST /{path...}/async`
/// - REST channels: any method matched against route patterns from DB
#[tracing::instrument(skip(state, headers, query_params, body), fields(path = %path))]
async fn dynamic_handler(
    State(state): State<AppState>,
    Path(path): Path<String>,
    method: axum::http::Method,
    headers: axum::http::HeaderMap,
    Query(query_params): Query<std::collections::HashMap<String, String>>,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, OrionError> {
    // Strip trailing /async suffix
    let (route_path, is_async) = if let Some(stripped) = path.strip_suffix("/async") {
        (stripped, true)
    } else {
        (path.as_str(), false)
    };

    let route_path = route_path.trim_matches('/').trim();
    if route_path.is_empty() {
        return Err(OrionError::BadRequest(
            "Channel name must not be empty".into(),
        ));
    }

    // Resolve channel: try REST route table first, then direct name lookup
    let (channel, route_params) = if let Some(rm) = state
        .channel_registry
        .match_route(method.as_str(), route_path)
        .await
    {
        (rm.channel_name, rm.params)
    } else if !route_path.contains('/') {
        // Single segment — treat as simple channel name (backward compat)
        (route_path.to_string(), std::collections::HashMap::new())
    } else {
        return Err(OrionError::NotFound(format!(
            "No channel matches {} /{}",
            method, route_path
        )));
    };

    // Content-Type enforcement: non-empty bodies must declare a JSON media type
    if !body.is_empty() {
        let content_type = headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let is_json =
            content_type.starts_with("application/json") || content_type.contains("+json");
        if !is_json {
            return Err(OrionError::UnsupportedMediaType(
                "Content-Type must be application/json for requests with a body".to_string(),
            ));
        }
    }

    // Parse body: empty body is valid (GET/DELETE), otherwise must be JSON
    let req: ProcessRequest = if body.is_empty() {
        ProcessRequest {
            data: json!({}),
            metadata: json!({}),
        }
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| OrionError::BadRequest(format!("Invalid JSON body: {e}")))?
    };

    // Build metadata with all request context available for validation_logic
    let mut metadata = if req.metadata.is_object() {
        req.metadata.clone()
    } else {
        json!({})
    };
    metadata["http_method"] = json!(method.as_str());
    if !route_params.is_empty() {
        metadata["params"] = json!(route_params);
    }
    if !query_params.is_empty() {
        metadata["query"] = json!(query_params);
    }
    // Expose request headers so validation_logic can check content-type,
    // content-length, authorization, etc.
    let header_map: serde_json::Map<String, Value> = headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_string(), json!(v)))
        })
        .collect();
    metadata["headers"] = Value::Object(header_map);

    if is_async {
        let input_json = serde_json::to_string(&req.data).ok();
        let trace = state
            .trace_repo
            .create_pending(&channel, "async", input_json.as_deref())
            .await?;
        let trace_id = trace.id.clone();

        let trace_headers = {
            let mut h = std::collections::HashMap::new();
            crate::server::trace_context::inject_trace_context(&mut h);
            h
        };

        state
            .trace_queue
            .submit(crate::queue::QueueMessage {
                trace_id,
                channel,
                payload: req.data,
                metadata,
                trace_headers,
            })
            .await?;

        return Ok((StatusCode::ACCEPTED, Json(json!({ "trace_id": trace.id }))).into_response());
    }

    process_sync_for_channel(&state, &channel, req.data, metadata, &headers).await
}

/// Check per-channel CORS: reject the request if the `Origin` header is present
/// but not in the channel's allowed-origins list.
fn check_cors_origin(
    channel: &str,
    channel_config: &Option<std::sync::Arc<crate::channel::ChannelRuntimeConfig>>,
    headers: &axum::http::HeaderMap,
) -> Result<(), OrionError> {
    if let Some(cfg) = channel_config
        && let Some(cors) = &cfg.parsed_config.cors
        && let Some(allowed_origins) = &cors.allowed_origins
        && let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok())
        && !allowed_origins.iter().any(|o| o == "*" || o == origin)
    {
        return Err(OrionError::Forbidden(format!(
            "Origin '{}' is not allowed for channel '{}'",
            origin, channel
        )));
    }
    Ok(())
}

/// Evaluate per-channel input validation logic (JSONLogic). Returns `Ok(())` when
/// validation passes or no validation is configured.
fn validate_input(
    channel: &str,
    channel_config: &Option<std::sync::Arc<crate::channel::ChannelRuntimeConfig>>,
    data: &Value,
    metadata: &Value,
    datalogic: &datalogic_rs::DataLogic,
) -> Result<(), OrionError> {
    if let Some(cfg) = channel_config
        && let Some(ref compiled) = cfg.validation_logic
    {
        let context = std::sync::Arc::new(json!({ "data": data, "metadata": metadata }));
        match datalogic.evaluate(compiled, context) {
            Ok(result) => {
                if !is_truthy(&result) {
                    return Err(OrionError::BadRequest(
                        "Input validation failed".to_string(),
                    ));
                }
            }
            Err(e) => {
                tracing::warn!(channel = %channel, error = %e, "validation_logic evaluation failed, rejecting");
                return Err(OrionError::BadRequest(format!(
                    "Input validation error: {e}"
                )));
            }
        }
    }
    Ok(())
}

/// Check per-channel request deduplication. Returns `Err(Conflict)` when a
/// duplicate idempotency key is detected within the configured window.
async fn check_deduplication(
    channel_config: &Option<std::sync::Arc<crate::channel::ChannelRuntimeConfig>>,
    headers: &axum::http::HeaderMap,
) -> Result<(), OrionError> {
    if let Some(cfg) = channel_config
        && let Some(ref dedup) = cfg.parsed_config.deduplication
        && let Some(ref store) = cfg.dedup_store
        && let Some(key) = headers.get(&dedup.header).and_then(|v| v.to_str().ok())
    {
        let window = dedup.window_secs.unwrap_or(300);
        let is_new = store.check_and_insert(key, window).await.unwrap_or(false);
        if !is_new {
            return Err(OrionError::Conflict(format!(
                "Duplicate request: idempotency key '{}' already seen",
                key
            )));
        }
    }
    Ok(())
}

/// Acquire a per-channel backpressure permit. Returns `Err(ServiceUnavailable)`
/// when the channel's concurrency limit has been reached.
fn acquire_backpressure(
    channel: &str,
    channel_config: &Option<std::sync::Arc<crate::channel::ChannelRuntimeConfig>>,
) -> Result<Option<tokio::sync::OwnedSemaphorePermit>, OrionError> {
    if let Some(cfg) = channel_config
        && let Some(ref semaphore) = cfg.backpressure_semaphore
    {
        match semaphore.clone().try_acquire_owned() {
            Ok(permit) => Ok(Some(permit)),
            Err(_) => {
                metrics::record_error("backpressure");
                Err(OrionError::ServiceUnavailable(format!(
                    "Channel '{}' is at capacity",
                    channel
                )))
            }
        }
    } else {
        Ok(None)
    }
}

/// Compute a deterministic cache key from channel name and request data.
fn compute_cache_key(
    channel: &str,
    data: &Value,
    cache_cfg: &crate::channel::ChannelCacheConfig,
) -> String {
    // FNV-1a hash — deterministic across processes (unlike DefaultHasher).
    fn fnv1a_feed(h: &mut u64, bytes: &[u8]) {
        for &b in bytes {
            *h ^= b as u64;
            *h = h.wrapping_mul(0x100000001b3);
        }
    }

    let mut h: u64 = 0xcbf29ce484222325;

    if let Some(ref fields) = cache_cfg.cache_key_fields {
        // Hash selected fields directly — no intermediate Map or clones
        for f in fields {
            if let Some(v) = data.get(f) {
                fnv1a_feed(&mut h, f.as_bytes());
                let v_bytes = serde_json::to_vec(v).unwrap_or_default();
                fnv1a_feed(&mut h, &v_bytes);
            }
        }
    } else {
        let bytes = serde_json::to_vec(data).unwrap_or_default();
        fnv1a_feed(&mut h, &bytes);
    };

    format!("cache:{}:{:016x}", channel, h)
}

/// Build an HTTP response from a pre-serialized JSON string, avoiding
/// the double-serialization that `Json<Value>` would incur.
fn json_response(status: StatusCode, body: String) -> Response {
    axum::response::Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body))
        .expect("valid HTTP response builder with static header")
}

/// Core sync processing logic shared between simple HTTP and REST routes.
///
/// Returns a pre-serialized `Response` so the JSON is serialized exactly once
/// (or zero times on cache hit).
async fn process_sync_for_channel(
    state: &AppState,
    channel: &str,
    data: Value,
    metadata: Value,
    headers: &axum::http::HeaderMap,
) -> Result<Response, OrionError> {
    let channel_config = state.channel_registry.get_by_name(channel).await;

    check_cors_origin(channel, &channel_config, headers)?;
    validate_input(channel, &channel_config, &data, &metadata, &state.datalogic)?;
    check_deduplication(&channel_config, headers).await?;

    // Response cache check — return early on cache hit (zero serialization)
    let cache_context = if let Some(ref cfg) = channel_config
        && let Some(ref cache_cfg) = cfg.parsed_config.cache
        && cache_cfg.enabled
        && let Some(ref cache) = cfg.response_cache
    {
        let key = compute_cache_key(channel, &data, cache_cfg);
        match cache.get(&key).await {
            Ok(Some(cached)) => {
                metrics::record_cache_hit(channel);
                // Return the cached JSON string directly — no deserialization needed
                return Ok(json_response(StatusCode::OK, cached));
            }
            _ => {
                metrics::record_cache_miss(channel);
                Some((key, cache.clone(), cache_cfg.ttl_secs.unwrap_or(300)))
            }
        }
    } else {
        None
    };

    let _backpressure_permit = acquire_backpressure(channel, &channel_config)?;

    let start = Instant::now();
    let engine = crate::engine::acquire_engine_read(&state.engine).await;
    let mut message = dataflow_rs::Message::from_value(&data);
    merge_metadata(&mut message, &metadata);
    inject_rollout_bucket(&mut message);

    let timeout_ms = channel_config
        .as_ref()
        .and_then(|c| c.parsed_config.timeout_ms);

    let result = if let Some(ms) = timeout_ms {
        match tokio::time::timeout(
            Duration::from_millis(ms),
            engine.process_message_for_channel(channel, &mut message),
        )
        .await
        {
            Ok(inner) => inner,
            Err(_) => {
                remove_rollout_bucket(&mut message);
                metrics::record_message(channel, "timeout");
                metrics::record_error("timeout");
                return Err(OrionError::Timeout {
                    channel: channel.to_string(),
                    timeout_ms: ms,
                });
            }
        }
    } else {
        engine
            .process_message_for_channel(channel, &mut message)
            .await
    };

    match result {
        Ok(()) => {
            remove_rollout_bucket(&mut message);
            let duration = start.elapsed();
            let duration_secs = duration.as_secs_f64();
            let duration_ms = duration.as_secs_f64() * 1000.0;
            metrics::record_message(channel, "ok");
            metrics::record_message_duration(channel, duration_secs);
            metrics::record_channel_execution(channel);

            let response = json!({
                "id": message.id,
                "status": "ok",
                "data": message.data(),
                "errors": message.errors.iter().filter_map(|e| serde_json::to_value(e).ok()).collect::<Vec<_>>(),
            });

            // Serialize response exactly once — reused for size check, trace storage,
            // cache, and the HTTP response body (no re-serialization by Axum).
            let response_json = serde_json::to_string(&response)
                .map_err(|e| OrionError::Internal(format!("Failed to serialize response: {e}")))?;

            let max_result_size = state.config.queue.max_result_size_bytes;
            if max_result_size > 0 && response_json.len() > max_result_size {
                metrics::record_error("result_size_exceeded");
                return Err(OrionError::ResponseTooLarge(format!(
                    "Result size {} bytes exceeds limit of {} bytes",
                    response_json.len(),
                    max_result_size
                )));
            }

            let input_json = serde_json::to_string(&data).ok();
            if let Err(e) = state
                .trace_repo
                .store_completed(
                    channel,
                    "sync",
                    input_json.as_deref(),
                    &response_json,
                    duration_ms,
                )
                .await
            {
                tracing::warn!(error = %e, "Failed to store sync processing result");
            }

            // Fire-and-forget cache store
            if let Some((ref key, ref cache, ttl)) = cache_context
                && let Err(e) = cache.set_ex(key, &response_json, ttl).await
            {
                tracing::debug!(channel = channel, error = %e, "Failed to cache response");
            }

            Ok(json_response(StatusCode::OK, response_json))
        }
        Err(e) => {
            remove_rollout_bucket(&mut message);
            metrics::record_message(channel, "error");
            metrics::record_error("engine");
            Err(OrionError::Engine(e))
        }
    }
}

// ============================================================
// Request Types
// ============================================================

#[derive(Deserialize, utoipa::ToSchema)]
pub(crate) struct ProcessRequest {
    data: Value,
    #[serde(default)]
    metadata: Value,
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
