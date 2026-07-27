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
// Referenced by the `#[utoipa::path]` `body = ErrorResponse` annotations below.
use crate::server::routes::openapi::ErrorResponse;
use crate::server::state::AppState;
use crate::storage::repositories::traces::{TraceCompletedRow, TraceFilter};

// Re-export from engine utils for backward compatibility
pub(crate) use crate::engine::utils::merge_metadata;
use crate::engine::utils::{inject_rollout_bucket, remove_rollout_bucket};

/// Request headers whose values are credentials, masked before the header
/// map enters workflow metadata (S10). `http::HeaderName` is always
/// lowercase, so plain slice lookup suffices.
const CREDENTIAL_HEADERS: [&str; 4] = [
    "authorization",
    "cookie",
    "proxy-authorization",
    "x-api-key",
];

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
#[utoipa::path(
    post,
    path = "/api/v1/data/{channel}",
    tag = "Data",
    operation_id = "process_channel_request",
    summary = "Invoke a channel synchronously",
    description = "\
Invoke a channel's workflow synchronously.

This is a **templated** path, not a static one. Orion serves the whole data \
plane from a single catch-all route (`/api/v1/data/{*path}`) and resolves the \
target channel at request time, so no per-channel path exists in this document:

* **Simple HTTP channels** — a single path segment matched against the channel \
  `name`, e.g. `POST /api/v1/data/order-intake`.
* **REST channels** — each active channel registers its own method and path \
  pattern (`config.rest.routes`) at engine-reload time; those patterns may span \
  several segments and declare their own path parameters, which arrive in the \
  workflow as `metadata.params`. Any HTTP method is accepted — `GET`, `PUT`, \
  `PATCH` and `DELETE` behave identically to the `POST` documented here, with \
  the verb exposed as `metadata.http_method`. Query the admin channel API for \
  the routes a given deployment actually serves.

Append `/async` to submit to the queue instead — see \
`POST /api/v1/data/{channel}/async`.

This endpoint is unauthenticated: admin auth does not cover the data plane. \
Per-channel access control is expressed through `validation_logic` and CORS \
configuration.",
    params(
        ("channel" = String, Path, description = "Channel name, or the first segment of a REST channel's registered route pattern."),
        ("profile" = Option<bool>, Query, description = "Set to `1`/`true` to append `_orion.profile` timings to the response. Requires `tracing.debug_profile_enabled = true`; the `X-Orion-Profile` header does the same."),
    ),
    request_body(
        content = ProcessRequest,
        description = "Workflow input. `data` is the payload; `metadata` is merged into the message metadata alongside the server-supplied `channel`, `http_method`, `params`, `query`, and `headers` keys. An empty body is accepted (typical for `GET`/`DELETE` REST channels) and treated as `{\"data\": {}}`.",
        content_type = "application/json",
    ),
    responses(
        (status = 200, description = "Workflow completed. `errors` is empty on success; when tasks failed it carries sanitized `{code, message, task_id}` entries and the envelope gains a `request_id` for correlation with the persisted trace.", body = ProcessResponse),
        (status = 400, description = "Malformed JSON body, empty channel segment, or a channel `validation_logic` rejection (`VALIDATION_ERROR`, with per-field `details`)", body = ErrorResponse),
        (status = 403, description = "Origin not allowed by the channel's CORS configuration", body = ErrorResponse),
        (status = 404, description = "No REST route matches the requested method and path. Note that a single-segment path is *not* checked against the channel registry: an unknown name is accepted and the engine returns a `200` envelope with the input echoed back and no errors.", body = ErrorResponse),
        (status = 409, description = "Deduplication key already seen inside the channel's dedup window", body = ErrorResponse),
        (status = 415, description = "Non-empty body without a JSON `Content-Type`", body = ErrorResponse),
        (status = 429, description = "Rate limit exceeded (global or per-channel)", body = ErrorResponse),
        (status = 502, description = "Result exceeded `queue.max_result_size_bytes` (`RESPONSE_TOO_LARGE`)", body = ErrorResponse),
        (status = 503, description = "Channel backpressure limit reached, or a connector circuit breaker is open (`CIRCUIT_OPEN`)", body = ErrorResponse),
        (status = 504, description = "Workflow exceeded the channel's `timeout_ms`", body = ErrorResponse),
    )
)]
#[tracing::instrument(skip(state, headers, query_params, body), fields(path = %path))]
pub(crate) async fn dynamic_handler(
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
    // F4: stamp the resolved channel name (overriding any caller-supplied
    // value) so circuit-breaker keys and connector metrics are labeled
    // `channel:connector` instead of `unknown:connector`.
    metadata["channel"] = json!(channel.clone());
    metadata["http_method"] = json!(method.as_str());
    if !route_params.is_empty() {
        metadata["params"] = json!(route_params);
    }
    if !query_params.is_empty() {
        metadata["query"] = json!(query_params);
    }
    // Expose request headers so validation_logic can check content-type,
    // content-length, header presence, etc. Credential-bearing headers are
    // masked (S10): this map is persisted verbatim into `traces.result_json`
    // on the async path and `trace_dlq.metadata_json` on the failure path,
    // so a plaintext value here is a plaintext credential at rest — and,
    // before S14, one readable over HTTP. The key survives so logic can
    // still test presence; the value is never recoverable downstream.
    let header_map: serde_json::Map<String, Value> = headers
        .iter()
        .filter_map(|(name, value)| {
            let name = name.as_str();
            if CREDENTIAL_HEADERS.contains(&name) {
                return Some((name.to_string(), json!(crate::connector::MASK)));
            }
            value.to_str().ok().map(|v| (name.to_string(), json!(v)))
        })
        .collect();
    metadata["headers"] = Value::Object(header_map);

    // Per-channel ingress guards apply before the sync/async split (S1):
    // appending `/async` must not bypass CORS, validation_logic,
    // deduplication, or backpressure. The response cache stays sync-only —
    // async submissions always return 202.
    // F35: a channel that failed to load is quarantined, not silently
    // config-less — serving it here would apply none of its guards.
    let channel_runtime = state.channel_registry.require_serviceable(&channel).await?;
    guards::check_cors_origin(
        &channel,
        &channel_runtime,
        headers.get("origin").and_then(|v| v.to_str().ok()),
    )?;
    guards::validate_input(
        &channel,
        &channel_runtime,
        &req.data,
        &metadata,
        &state.datalogic,
    )?;
    guards::check_deduplication(&channel, &channel_runtime, |name| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    })
    .await?;

    if is_async {
        // Acquired before the pending trace is created so rejected requests
        // leave no trace row. The permit rides inside the queued message and
        // is held by the worker for the duration of processing, so a
        // channel's `max_concurrent` bounds sync and async work together.
        let backpressure_permit = guards::acquire_backpressure(&channel, &channel_runtime)?;

        // Resolve the effective trace config (channel override > global default).
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
                backpressure_permit,
            })
            .await?;

        return Ok(response);
    }

    process_sync_for_channel(
        &state,
        &channel,
        req.data,
        metadata,
        channel_runtime,
        profile_requested,
    )
    .await
}

/// Documentation-only anchor for the async submission path.
///
/// `POST /api/v1/data/{channel}/async` is served by [`dynamic_handler`], which
/// strips the `/async` suffix from the catch-all path — one Rust function, two
/// documented operations. `#[utoipa::path]` can only be applied once per
/// function, so the async operation hangs off this stub instead of inventing a
/// second handler. It is never called; the macro only reads its attribute.
#[allow(dead_code)]
#[utoipa::path(
    post,
    path = "/api/v1/data/{channel}/async",
    tag = "Data",
    operation_id = "submit_channel_request_async",
    summary = "Submit to a channel asynchronously",
    description = "\
Queue a channel's workflow for background execution and return immediately.

Accepts the same body and resolves the channel exactly as \
`POST /api/v1/data/{channel}` (including REST route patterns — append `/async` \
to any of them). All ingress guards still apply before the queue hand-off: \
CORS, `validation_logic`, deduplication, and backpressure. The response cache \
is sync-only, so an async submission never returns a cached body.

Poll `GET /api/v1/data/traces/{id}` with the returned `trace_id` for the \
result. That endpoint is admin-authenticated even though this one is not.",
    params(
        ("channel" = String, Path, description = "Channel name, or the first segment of a REST channel's registered route pattern."),
    ),
    request_body(
        content = ProcessRequest,
        description = "Same envelope as the synchronous endpoint.",
        content_type = "application/json",
    ),
    responses(
        (
            status = 202,
            description = "Accepted and queued. `trace_id` is `null` when trace persistence is off for this channel, in which case there is nothing to poll and a `Warning: 299` header explains why.",
            body = AsyncSubmitResponse,
            headers(
                ("warning" = String, description = "Present only when trace persistence is disabled for the channel: `299 - \"Trace persistence disabled for channel '<name>'\"`. `trace_id` is `null` in that case."),
            ),
        ),
        (status = 400, description = "Malformed JSON body, empty channel segment, or a `validation_logic` rejection", body = ErrorResponse),
        (status = 403, description = "Origin not allowed by the channel's CORS configuration", body = ErrorResponse),
        (status = 404, description = "No REST route matches the requested method and path. Note that a single-segment path is *not* checked against the channel registry: an unknown name is accepted and the engine returns a `200` envelope with the input echoed back and no errors.", body = ErrorResponse),
        (status = 409, description = "Deduplication key already seen inside the channel's dedup window", body = ErrorResponse),
        (status = 415, description = "Non-empty body without a JSON `Content-Type`", body = ErrorResponse),
        (status = 429, description = "Rate limit exceeded (global or per-channel)", body = ErrorResponse),
        (status = 503, description = "Channel backpressure limit reached or the trace queue is full/closed", body = ErrorResponse),
    )
)]
pub(crate) fn submit_channel_request_async_docs() {}

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
///
/// `cache_body` is the client-facing serialization: cache hits are returned
/// to callers verbatim, so the cached copy must be the sanitized envelope
/// (G1), while the persisted trace keeps `trace.response_json` full detail.
async fn persist_trace_and_cache(
    state: &AppState,
    channel_config: &Option<std::sync::Arc<crate::channel::ChannelRuntimeConfig>>,
    trace: &CompletedTrace<'_>,
    cache_body: &str,
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
        && let Err(e) = cache.set_ex(key, cache_body, *ttl).await
    {
        tracing::debug!(channel = trace.channel, error = %e, "Failed to cache response");
    }
}

/// Generic replacement for engine error messages on the data plane (G1).
const SANITIZED_ERROR_MESSAGE: &str =
    "Task processing failed; full detail is available in the trace";

/// Map engine `ErrorInfo` entries to a client-safe shape: code and task_id
/// only, with a generic message. Raw messages can embed upstream URLs,
/// connector names, and driver errors, which must not reach anonymous
/// data-plane callers — the persisted trace keeps the originals.
fn sanitize_errors(errors: &[dataflow_rs::ErrorInfo]) -> Vec<Value> {
    errors
        .iter()
        .map(|e| {
            let mut entry = json!({
                "code": e.code,
                "message": SANITIZED_ERROR_MESSAGE,
            });
            if let Some(ref task_id) = e.task_id {
                entry["task_id"] = json!(task_id);
            }
            entry
        })
        .collect()
}

/// Core sync processing logic shared between simple HTTP and REST routes.
/// CORS, validation, and dedup have already been applied by the caller
/// (`dynamic_handler`) before the sync/async split.
///
/// Returns a pre-serialized `Response` so the JSON is serialized exactly once
/// (or zero times on cache hit).
async fn process_sync_for_channel(
    state: &AppState,
    channel: &str,
    data: Value,
    metadata: Value,
    channel_config: Option<std::sync::Arc<crate::channel::ChannelRuntimeConfig>>,
    profile_requested: bool,
) -> Result<Response, OrionError> {
    let profile = profile_requested.then(crate::engine::profile::ProfileCollector::new);

    // O1: only registry-confirmed channels may appear as metric labels.
    // The single-segment route fallback accepts arbitrary path segments, so
    // labelling on the raw name would let callers grow Prometheus label
    // cardinality without bound.
    let metrics_channel = if channel_config.is_some() {
        channel
    } else {
        "_unknown"
    };

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
            metrics::record_message(metrics_channel, "timeout");
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
            metrics::record_message(metrics_channel, "ok");
            metrics::record_message_duration(metrics_channel, duration_secs);
            metrics::record_channel_execution(metrics_channel);

            let response = json!({
                "id": message.id(),
                "status": "ok",
                "data": message.data(),
                "errors": message.errors().iter().filter_map(|e| serde_json::to_value(e).ok()).collect::<Vec<_>>(),
            });

            // Serialize the full-detail envelope exactly once — reused for the
            // size check, trace storage, and (on the error-free hot path) the
            // cache and HTTP body with no re-serialization by Axum.
            let response_json = serde_json::to_string(&response)
                .map_err(|e| OrionError::Internal(format!("Failed to serialize response: {e}")))?;

            // G1: when workflow errors are present, the client-facing body
            // (and the cached copy) carry sanitized entries plus a
            // correlation id; the persisted trace keeps the full detail.
            let has_errors = message.has_errors();
            let public_response = if has_errors {
                let request_id = crate::server::request_context::REQUEST_ID
                    .try_with(|id| id.clone())
                    .unwrap_or_default();
                Some(json!({
                    "id": message.id(),
                    "status": "ok",
                    "data": message.data(),
                    "errors": sanitize_errors(message.errors()),
                    "request_id": request_id,
                }))
            } else {
                None
            };
            let public_json = match &public_response {
                Some(v) => Some(serde_json::to_string(v).map_err(|e| {
                    OrionError::Internal(format!("Failed to serialize response: {e}"))
                })?),
                None => None,
            };

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
                public_json.as_deref().unwrap_or(&response_json),
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
                let mut response_with_profile = public_response.unwrap_or(response);
                response_with_profile["_orion"] = json!({ "profile": p.to_json() });
                let body = serde_json::to_string(&response_with_profile).map_err(|e| {
                    OrionError::Internal(format!("Failed to serialize response: {e}"))
                })?;
                return Ok(json_response(StatusCode::OK, body));
            }

            Ok(json_response(
                StatusCode::OK,
                public_json.unwrap_or(response_json),
            ))
        }
        Err(e) => {
            remove_rollout_bucket(&mut message);
            metrics::record_message(metrics_channel, "error");
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
// Response Types (schema-only)
// ============================================================
//
// The data plane builds its envelopes with `json!` so the hot path serializes
// exactly once (see `process_sync_for_channel`). These mirrors exist purely so
// the OpenAPI document describes the real shape; they are registered in
// `openapi::ApiDoc` and never constructed at runtime.

/// Synchronous data-plane response envelope.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct ProcessResponse {
    /// Engine message id, also the correlation key inside the persisted trace.
    id: String,
    /// Always `ok` — task-level failures are reported in `errors`, not by
    /// flipping this field.
    #[schema(example = "ok")]
    status: String,
    /// Workflow output. Shape is entirely channel-defined.
    data: Value,
    /// Sanitized per-task failures. Empty on a clean run.
    errors: Vec<ProcessTaskError>,
    /// Correlation id, present only when `errors` is non-empty: the full
    /// messages are kept in the trace, not returned to the caller.
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    /// Debug namespace, present only when profiling was requested and
    /// `tracing.debug_profile_enabled` is on. Currently carries `profile`.
    #[serde(rename = "_orion", skip_serializing_if = "Option::is_none")]
    orion: Option<Value>,
}

/// One task failure, with the message replaced by a generic string — upstream
/// URLs, connector names, and driver errors stay in the trace.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct ProcessTaskError {
    code: String,
    #[schema(example = "Task processing failed; full detail is available in the trace")]
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
}

/// Acknowledgement returned by `POST /api/v1/data/{channel}/async`.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct AsyncSubmitResponse {
    /// Id to poll via `GET /api/v1/data/traces/{id}`, or `null` when trace
    /// persistence is disabled for the channel (see the `Warning` header).
    trace_id: Option<String>,
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

#[utoipa::path(
    get,
    path = "/api/v1/data/traces/{id}",
    tag = "Data",
    params(("id" = String, Path, description = "Trace ID")),
    responses(
        (status = 200, description = "Trace status and result"),
        (status = 404, description = "Trace not found", body = ErrorResponse),
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
