use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::channel::guards::{self, CacheLookup, CacheStoreCtx};
use crate::channel::registry::EffectiveTraceConfig;
use crate::config::TraceStorageMode;
use crate::errors::OrionError;
use crate::metrics;
use crate::queue::{TracePersistenceQueue, TracePersistenceTask};
use crate::server::state::AppState;
use crate::storage::repositories::traces::{TraceCompletedRow, TraceFilter};

// Re-export from engine utils for backward compatibility
pub(crate) use crate::engine::utils::merge_metadata;
use crate::engine::utils::{inject_rollout_bucket, remove_rollout_bucket};

/// One completed sync trace, ready for persistence and (optionally) caching.
/// Shared by [`route_store_completed`] and [`persist_trace_and_cache`] so
/// the trace fields are passed as a single borrow instead of 6 positional
/// arguments at each callsite.
struct CompletedTrace<'a> {
    channel: &'a str,
    channel_id: Option<&'a str>,
    input_json: Option<&'a str>,
    response_json: &'a str,
    duration_ms: f64,
    has_errors: bool,
    task_trace_json: Option<&'a str>,
}

/// Route a completed sync trace through the chosen persistence mode.
/// Returns early via [`EffectiveTraceConfig::should_drop`] when the
/// per-channel/global filters say this trace should not be persisted.
async fn route_store_completed(
    cfg: &EffectiveTraceConfig,
    trace_repo: &std::sync::Arc<dyn crate::storage::repositories::traces::TraceRepository>,
    persistence_queue: &TracePersistenceQueue,
    trace: &CompletedTrace<'_>,
) {
    if let Some(reason) = cfg.should_drop(trace.has_errors) {
        metrics::record_trace_dropped(reason);
        return;
    }
    // `should_drop` already returned for `Off`; remaining modes are Sync / Async / Batch.
    if matches!(cfg.mode, TraceStorageMode::Sync) {
        if let Err(e) = trace_repo
            .store_completed(
                trace.channel,
                trace.channel_id,
                "sync",
                trace.input_json,
                trace.response_json,
                trace.duration_ms,
                trace.task_trace_json,
            )
            .await
        {
            tracing::warn!(error = %e, "Failed to store sync processing result");
        }
    } else {
        let task = TracePersistenceTask::StoreCompleted(TraceCompletedRow {
            channel: trace.channel.to_string(),
            channel_id: trace.channel_id.map(str::to_string),
            mode: "sync".to_string(),
            input_json: trace.input_json.map(str::to_string),
            result_json: trace.response_json.to_string(),
            duration_ms: trace.duration_ms,
            task_trace_json: trace.task_trace_json.map(str::to_string),
        });
        persistence_queue.submit(task).await;
    }
}

/// Internal error type for `run_engine_optionally_profiled`. The outer error
/// from `engine.process_message_for_channel` is propagated through the
/// `dataflow_rs::Result` carried in the `Ok` arm; this enum just distinguishes
/// "timeout vs engine call returned".
enum EngineRunError {
    Timeout(u64),
}

/// Result tuple returned from the engine call: the engine's own `Result<()>`,
/// and an optional captured per-task `ExecutionTrace` populated when the
/// channel opted into `task_details` (A2).
type EngineCallResult = (dataflow_rs::Result<()>, Option<dataflow_rs::ExecutionTrace>);

/// Run the engine for `channel`, with optional timeout and optional
/// trace capture. Used by both profiled and non-profiled paths.
async fn run_engine_inner(
    engine: &std::sync::Arc<dataflow_rs::Engine>,
    channel: &str,
    message: &mut dataflow_rs::Message,
    timeout_ms: Option<u64>,
    capture_trace: bool,
) -> Result<EngineCallResult, EngineRunError> {
    if capture_trace {
        let fut = engine.process_message_for_channel_with_trace(channel, message);
        let inner = if let Some(ms) = timeout_ms {
            match tokio::time::timeout(Duration::from_millis(ms), fut).await {
                Ok(r) => r,
                Err(_) => return Err(EngineRunError::Timeout(ms)),
            }
        } else {
            fut.await
        };
        match inner {
            Ok(trace) => Ok((Ok(()), Some(trace))),
            Err(e) => Ok((Err(e), None)),
        }
    } else if let Some(ms) = timeout_ms {
        match tokio::time::timeout(
            Duration::from_millis(ms),
            engine.process_message_for_channel(channel, message),
        )
        .await
        {
            Ok(inner) => Ok((inner, None)),
            Err(_) => Err(EngineRunError::Timeout(ms)),
        }
    } else {
        Ok((
            engine.process_message_for_channel(channel, message).await,
            None,
        ))
    }
}

/// Run the engine for `channel`, optionally scoped inside a per-request
/// `ProfileCollector`. Honors `timeout_ms` when provided. When
/// `capture_trace` is true, uses the with-trace engine entry point and
/// returns the resulting `ExecutionTrace` for persistence.
async fn run_engine_optionally_profiled(
    engine: &std::sync::Arc<dataflow_rs::Engine>,
    channel: &str,
    message: &mut dataflow_rs::Message,
    timeout_ms: Option<u64>,
    profile: Option<&std::sync::Arc<crate::engine::profile::ProfileCollector>>,
    capture_trace: bool,
) -> Result<EngineCallResult, EngineRunError> {
    use crate::engine::profile::ORION_PROFILE;

    if let Some(p) = profile {
        ORION_PROFILE
            .scope(
                p.clone(),
                run_engine_inner(engine, channel, message, timeout_ms, capture_trace),
            )
            .await
    } else {
        run_engine_inner(engine, channel, message, timeout_ms, capture_trace).await
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
            "No channel matches {method} /{route_path}"
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

    // Profile mode: opt-in via header OR ?profile=1 query, gated by global config flag.
    let profile_requested = state.config.tracing.debug_profile_enabled
        && (header_or_query_truthy(&headers, &query_params, "x-orion-profile", "profile"));

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
        // Resolve the effective trace config (channel override > global default).
        let channel_runtime = state.channel_registry.get_by_name(&channel).await;
        let effective_trace = channel_runtime
            .as_ref()
            .map(|c| c.trace_storage)
            .unwrap_or_else(|| {
                crate::channel::registry::EffectiveTraceConfig::resolve(
                    &state.config.tracing.storage,
                    None,
                )
            });

        let trace_headers = {
            let mut h = std::collections::HashMap::new();
            crate::server::trace_context::inject_trace_context(&mut h);
            h
        };

        // In `off` mode skip the `create_pending` INSERT — emit a synthetic
        // trace_id so the worker can still process, but return null to the
        // caller along with a Warning header so polling clients know there's
        // nothing to fetch.
        let (trace_id, response): (String, Response) =
            if matches!(effective_trace.mode, crate::config::TraceStorageMode::Off) {
                metrics::record_trace_dropped("off");
                let id = uuid::Uuid::new_v4().to_string();
                let mut resp =
                    (StatusCode::ACCEPTED, Json(json!({ "trace_id": null }))).into_response();
                if let Ok(value) = axum::http::HeaderValue::from_str(&format!(
                    "299 - \"Trace persistence disabled for channel '{channel}'\""
                )) {
                    resp.headers_mut().insert("warning", value);
                }
                (id, resp)
            } else {
                let input_json = serde_json::to_string(&req.data).ok();
                let channel_id = channel_runtime
                    .as_ref()
                    .map(|c| c.channel.channel_id.as_str());
                let trace = state
                    .trace_repo
                    .create_pending(&channel, channel_id, "async", input_json.as_deref())
                    .await?;
                let id = trace.id.clone();
                let resp =
                    (StatusCode::ACCEPTED, Json(json!({ "trace_id": trace.id }))).into_response();
                (id, resp)
            };

        state
            .trace_queue
            .submit(crate::queue::QueueMessage {
                trace_id,
                channel,
                payload: req.data,
                metadata,
                trace_headers,
                profile_requested,
            })
            .await?;

        return Ok(response);
    }

    process_sync_for_channel(
        &state,
        &channel,
        req.data,
        metadata,
        &headers,
        profile_requested,
    )
    .await
}

/// Values considered truthy in header/query string flags.
const TRUTHY_VALUES: &[&str] = &["1", "true", "yes", "on"];

fn is_truthy_str(s: &str) -> bool {
    let trimmed = s.trim().to_ascii_lowercase();
    TRUTHY_VALUES.contains(&trimmed.as_str())
}

/// True when `header_name` or `query_name` is set to a truthy value
/// (`1`, `true`, `yes`, `on`). Case-insensitive.
fn header_or_query_truthy(
    headers: &axum::http::HeaderMap,
    query: &std::collections::HashMap<String, String>,
    header_name: &str,
    query_name: &str,
) -> bool {
    if let Some(v) = headers.get(header_name).and_then(|v| v.to_str().ok())
        && is_truthy_str(v)
    {
        return true;
    }
    if let Some(v) = query.get(query_name)
        && is_truthy_str(v)
    {
        return true;
    }
    false
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

/// Persist the completed trace through the configured storage mode and
/// fire-and-forget cache the serialized response if a cache context was
/// produced by [`check_response_cache`]. Records the trace-store phase in
/// the per-request profile when one is in scope.
async fn persist_trace_and_cache(
    state: &AppState,
    channel_config: &Option<std::sync::Arc<crate::channel::ChannelRuntimeConfig>>,
    trace: &CompletedTrace<'_>,
    cache_context: &Option<CacheStoreCtx>,
    profile: Option<&std::sync::Arc<crate::engine::profile::ProfileCollector>>,
) {
    let effective_trace = channel_config
        .as_ref()
        .map(|c| c.trace_storage)
        .unwrap_or_else(|| {
            crate::channel::registry::EffectiveTraceConfig::resolve(
                &state.config.tracing.storage,
                None,
            )
        });
    let trace_store_start = Instant::now();
    route_store_completed(
        &effective_trace,
        &state.trace_repo,
        &state.trace_persistence_queue,
        trace,
    )
    .await;
    if let Some(p) = profile {
        p.set_trace_store(trace_store_start.elapsed());
    }

    // Fire-and-forget cache store
    if let Some((key, cache, ttl)) = cache_context
        && let Err(e) = cache.set_ex(key, trace.response_json, *ttl).await
    {
        tracing::debug!(channel = trace.channel, error = %e, "Failed to cache response");
    }
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
    profile_requested: bool,
) -> Result<Response, OrionError> {
    let channel_config = state.channel_registry.get_by_name(channel).await;

    guards::check_cors_origin(
        channel,
        &channel_config,
        headers.get("origin").and_then(|v| v.to_str().ok()),
    )?;
    guards::validate_input(channel, &channel_config, &data, &metadata, &state.datalogic)?;
    guards::check_deduplication(channel, &channel_config, |name| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    })
    .await?;

    let profile = profile_requested.then(crate::engine::profile::ProfileCollector::new);

    // Response cache check — return early on cache hit (zero serialization)
    let cache_context =
        match guards::check_response_cache(channel, &data, &metadata, &channel_config).await {
            CacheLookup::Hit(cached) => return Ok(json_response(StatusCode::OK, cached)),
            CacheLookup::Miss(ctx) => ctx,
        };

    let _backpressure_permit = guards::acquire_backpressure(channel, &channel_config)?;

    let start = Instant::now();
    let engine = crate::engine::acquire_engine_read(&state.engine).await;
    let mut message = dataflow_rs::Message::from_value(&data);
    merge_metadata(&mut message, &metadata);
    let sticky_identity = crate::engine::utils::rollout_identity(
        &metadata,
        &state.config.engine.rollout_sticky_header,
    );
    inject_rollout_bucket(&mut message, sticky_identity);

    let timeout_ms = channel_config
        .as_ref()
        .and_then(|c| c.parsed_config.timeout_ms);

    // A2: when the channel opted in via `config.tracing.task_details = true`,
    // use the with-trace engine entry point so per-step inputs/outputs are
    // captured for persistence.
    let capture_trace = channel_config
        .as_ref()
        .map(|c| c.trace_storage.task_details)
        .unwrap_or(false);

    let workflow_start = Instant::now();
    let result = run_engine_optionally_profiled(
        &engine,
        channel,
        &mut message,
        timeout_ms,
        profile.as_ref(),
        capture_trace,
    )
    .await;
    if let Some(ref p) = profile {
        p.set_workflow_total(workflow_start.elapsed());
    }

    let (result, task_trace) = match result {
        Ok(inner) => inner,
        Err(EngineRunError::Timeout(ms)) => {
            remove_rollout_bucket(&mut message);
            metrics::record_message(channel, "timeout");
            metrics::record_error("timeout");
            return Err(OrionError::Timeout {
                channel: channel.to_string(),
                timeout_ms: ms,
            });
        }
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
                "id": message.id(),
                "status": "ok",
                "data": message.data(),
                "errors": message.errors().iter().filter_map(|e| serde_json::to_value(e).ok()).collect::<Vec<_>>(),
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
            let has_errors = message.has_errors();
            let task_trace_json = task_trace
                .as_ref()
                .and_then(|t| serde_json::to_string(t).ok());
            persist_trace_and_cache(
                state,
                &channel_config,
                &CompletedTrace {
                    channel,
                    channel_id: channel_config
                        .as_ref()
                        .map(|c| c.channel.channel_id.as_str()),
                    input_json: input_json.as_deref(),
                    response_json: &response_json,
                    duration_ms,
                    has_errors,
                    task_trace_json: task_trace_json.as_deref(),
                },
                &cache_context,
                profile.as_ref(),
            )
            .await;

            // Profile mode: rebuild the response with `_orion.profile`
            // appended and re-serialize. Only paid when profiling is on.
            //
            // B3 v0.2.0 shape lock: the debug surface lives under a single
            // top-level `_orion` namespace so future debug fields (e.g.
            // `_orion.task_trace`) can be added without colliding with
            // workflow-level output keys that callers control.
            if let Some(ref p) = profile {
                let mut response_with_profile = response;
                response_with_profile["_orion"] = json!({ "profile": p.to_json() });
                let body = serde_json::to_string(&response_with_profile).map_err(|e| {
                    OrionError::Internal(format!("Failed to serialize response: {e}"))
                })?;
                return Ok(json_response(StatusCode::OK, body));
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
        (status = 401, description = "Missing or invalid admin API key (when admin auth is enabled)"),
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
        (status = 401, description = "Missing or invalid admin API key (when admin auth is enabled)"),
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
        "channel": trace.channel,
        "channel_id": trace.channel_id,
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
    if let Some(ref tt) = trace.task_trace_json
        && let Ok(v) = serde_json::from_str::<Value>(tt)
    {
        response["task_trace_json"] = v;
    }

    Ok(Json(response))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;

    use crate::channel::registry::EffectiveTraceConfig;
    use crate::channel::{ChannelConfig, ChannelRuntimeConfig, DeduplicationConfig};
    use crate::config::TracingStorageConfig;
    use crate::connector::cache_backend::CacheBackend;
    use crate::errors::OrionError;
    use crate::storage::models::Channel;

    /// Dedup-store stub with a fixed `check_and_insert` outcome.
    enum StubOutcome {
        New,
        Duplicate,
        BackendError,
    }

    struct StubDedupBackend {
        outcome: StubOutcome,
    }

    #[async_trait]
    impl CacheBackend for StubDedupBackend {
        async fn get(&self, _key: &str) -> Result<Option<String>, OrionError> {
            Ok(None)
        }
        async fn set(&self, _key: &str, _value: &str) -> Result<(), OrionError> {
            Ok(())
        }
        async fn set_ex(&self, _key: &str, _value: &str, _ttl: u64) -> Result<(), OrionError> {
            Ok(())
        }
        async fn check_and_insert(&self, _key: &str, _window: u64) -> Result<bool, OrionError> {
            match self.outcome {
                StubOutcome::New => Ok(true),
                StubOutcome::Duplicate => Ok(false),
                StubOutcome::BackendError => {
                    Err(OrionError::Internal("dedup backend down".to_string()))
                }
            }
        }
    }

    /// Dedup-store stub that records every key passed to `check_and_insert`.
    struct CapturingDedupBackend {
        seen: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl CacheBackend for CapturingDedupBackend {
        async fn get(&self, _key: &str) -> Result<Option<String>, OrionError> {
            Ok(None)
        }
        async fn set(&self, _key: &str, _value: &str) -> Result<(), OrionError> {
            Ok(())
        }
        async fn set_ex(&self, _key: &str, _value: &str, _ttl: u64) -> Result<(), OrionError> {
            Ok(())
        }
        async fn check_and_insert(&self, key: &str, _window: u64) -> Result<bool, OrionError> {
            self.seen
                .lock()
                .expect("test lock poisoned")
                .push(key.to_string());
            Ok(true)
        }
    }

    fn dedup_runtime(outcome: StubOutcome) -> Option<Arc<ChannelRuntimeConfig>> {
        dedup_runtime_with_store(Arc::new(StubDedupBackend { outcome }))
    }

    fn dedup_runtime_with_store(store: Arc<dyn CacheBackend>) -> Option<Arc<ChannelRuntimeConfig>> {
        let now = chrono::Utc::now().naive_utc();
        Some(Arc::new(ChannelRuntimeConfig {
            channel: Channel {
                channel_id: "ch_test".to_string(),
                version: 1,
                name: "test-channel".to_string(),
                description: None,
                channel_type: "sync".to_string(),
                protocol: "rest".to_string(),
                methods: None,
                route_pattern: None,
                topic: None,
                consumer_group: None,
                transport_config_json: "{}".to_string(),
                workflow_id: None,
                config_json: "{}".to_string(),
                status: "active".to_string(),
                priority: 0,
                created_at: now,
                updated_at: now,
            },
            parsed_config: ChannelConfig {
                deduplication: Some(DeduplicationConfig {
                    header: "idempotency-key".to_string(),
                    window_secs: Some(60),
                    connector: None,
                }),
                ..Default::default()
            },
            rate_limiter: None,
            rate_limit_key_logic: None,
            validation_logic: None,
            backpressure_semaphore: None,
            dedup_store: Some(store),
            response_cache: None,
            trace_storage: EffectiveTraceConfig::resolve(&TracingStorageConfig::default(), None),
        }))
    }

    fn idempotency_headers() -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("idempotency-key", "token-1".parse().expect("test header"));
        headers
    }

    #[tokio::test]
    async fn test_dedup_new_key_passes() {
        let cfg = dedup_runtime(StubOutcome::New);
        let result = super::check_deduplication("test-channel", &cfg, &idempotency_headers()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_dedup_duplicate_rejected() {
        let cfg = dedup_runtime(StubOutcome::Duplicate);
        let result = super::check_deduplication("test-channel", &cfg, &idempotency_headers()).await;
        assert!(matches!(result, Err(OrionError::Conflict(_))));
    }

    #[tokio::test]
    async fn test_dedup_fails_open_on_backend_error() {
        let cfg = dedup_runtime(StubOutcome::BackendError);
        let result = super::check_deduplication("test-channel", &cfg, &idempotency_headers()).await;
        assert!(result.is_ok(), "backend errors must fail open, not 409");
    }

    #[tokio::test]
    async fn test_dedup_key_is_channel_scoped() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let cfg = dedup_runtime_with_store(Arc::new(CapturingDedupBackend { seen: seen.clone() }));
        super::check_deduplication("orders", &cfg, &idempotency_headers())
            .await
            .expect("dedup check should pass");
        let keys = seen.lock().expect("test lock poisoned");
        assert_eq!(keys.as_slice(), ["dedup:orders:token-1"]);
    }

    // ---- Response cache key (proposal N1) ----

    fn cache_cfg(fields: Option<Vec<String>>) -> crate::channel::ChannelCacheConfig {
        crate::channel::ChannelCacheConfig {
            enabled: true,
            ttl_secs: Some(60),
            cache_key_fields: fields,
            connector: None,
        }
    }

    fn meta(
        method: &str,
        params: serde_json::Value,
        query: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "http_method": method,
            "params": params,
            "query": query,
            "headers": {},
        })
    }

    #[test]
    fn test_cache_key_distinguishes_route_params() {
        let data = serde_json::json!({});
        let a = super::compute_cache_key(
            "orders",
            &data,
            &meta("GET", serde_json::json!({"id": "1"}), serde_json::json!({})),
            &cache_cfg(None),
        );
        let b = super::compute_cache_key(
            "orders",
            &data,
            &meta("GET", serde_json::json!({"id": "2"}), serde_json::json!({})),
            &cache_cfg(None),
        );
        assert_ne!(a, b, "different path params must not share a cache entry");
    }

    #[test]
    fn test_cache_key_distinguishes_query_and_method() {
        let data = serde_json::json!({});
        let base = meta(
            "GET",
            serde_json::json!({}),
            serde_json::json!({"page": "1"}),
        );
        let a = super::compute_cache_key("orders", &data, &base, &cache_cfg(None));
        let b = super::compute_cache_key(
            "orders",
            &data,
            &meta(
                "GET",
                serde_json::json!({}),
                serde_json::json!({"page": "2"}),
            ),
            &cache_cfg(None),
        );
        let c = super::compute_cache_key(
            "orders",
            &data,
            &meta(
                "POST",
                serde_json::json!({}),
                serde_json::json!({"page": "1"}),
            ),
            &cache_cfg(None),
        );
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn test_cache_key_stable_for_identical_requests() {
        let data = serde_json::json!({"order_id": 7});
        let m = meta(
            "GET",
            serde_json::json!({"id": "1"}),
            serde_json::json!({"expand": "items"}),
        );
        let a = super::compute_cache_key("orders", &data, &m, &cache_cfg(None));
        let b = super::compute_cache_key("orders", &data, &m, &cache_cfg(None));
        assert_eq!(a, b);
    }

    #[test]
    fn test_cache_key_folds_route_identity_with_key_fields() {
        // Even with cache_key_fields selecting body fields, route identity
        // must still distinguish keys.
        let data = serde_json::json!({"tenant": "acme"});
        let fields = Some(vec!["tenant".to_string()]);
        let a = super::compute_cache_key(
            "orders",
            &data,
            &meta("GET", serde_json::json!({"id": "1"}), serde_json::json!({})),
            &cache_cfg(fields.clone()),
        );
        let b = super::compute_cache_key(
            "orders",
            &data,
            &meta("GET", serde_json::json!({"id": "2"}), serde_json::json!({})),
            &cache_cfg(fields),
        );
        assert_ne!(a, b);
    }
}
