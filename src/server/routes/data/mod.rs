mod sync;
pub(crate) mod traces;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::channel::guards;
use crate::errors::OrionError;
use crate::metrics;
use crate::server::extract::OrionQuery;
// Referenced by the `#[utoipa::path]` `body = ErrorResponse` annotations below.
use crate::server::routes::openapi::ErrorResponse;
use crate::server::state::AppState;

use sync::process_sync_for_channel;

/// Request headers whose values are credentials, masked before the header
/// map enters workflow metadata (S10). `http::HeaderName` is always
/// lowercase, so plain slice lookup suffices.
const CREDENTIAL_HEADERS: [&str; 4] = [
    "authorization",
    "cookie",
    "proxy-authorization",
    "x-api-key",
];

pub fn data_routes() -> Router<AppState> {
    // A single catch-all, with no static segments to shadow it. The trace
    // reads used to sit here as `/traces` and `/traces/{id}`; static routes
    // win over `/{*path}` in axum, so a channel named `traces` was
    // permanently unreachable (`POST /api/v1/data/traces` returned 405) and
    // the rate limiter carried a special case to skip the name. They moved to
    // `/api/v1/admin/traces` in 1.0 (R8), where the admin-guarded list
    // endpoint always belonged.
    Router::new().route("/{*path}", any(dynamic_handler))
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
    OrionQuery(query_params): OrionQuery<std::collections::HashMap<String, String>>,
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

    let metadata = build_request_metadata(
        &req.metadata,
        &channel,
        &method,
        &route_params,
        &query_params,
        &headers,
    );

    // Per-channel ingress guards apply before the sync/async split (S1):
    // appending `/async` must not bypass CORS, validation_logic,
    // deduplication, or backpressure. The response cache stays sync-only —
    // async submissions always return 202.
    // F35: a channel that failed to load is quarantined, not silently
    // config-less — serving it here would apply none of its guards.
    let channel_runtime = state.channel_registry.require_serviceable(&channel).await?;
    // A name that is not in the registry is not an active channel. Without
    // this check the single-segment fallback above accepted ANY name and ran
    // the engine against an empty workflow set — a 200 "ok" for channels
    // that never existed or were just archived (the ingress-side twin of the
    // channel_call missing-target bug).
    if channel_runtime.is_none() {
        return Err(OrionError::NotFound(format!(
            "Channel '{channel}' not found or not active"
        )));
    }
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
        return submit_async(
            &state,
            channel,
            req.data,
            metadata,
            channel_runtime,
            profile_requested,
        )
        .await;
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

/// Build the workflow metadata object for a request: the caller-supplied
/// `metadata` merged with the server-supplied `channel`, `http_method`,
/// `params`, `query`, and (credential-masked) `headers` keys.
fn build_request_metadata(
    req_metadata: &Value,
    channel: &str,
    method: &axum::http::Method,
    route_params: &std::collections::HashMap<String, String>,
    query_params: &std::collections::HashMap<String, String>,
    headers: &axum::http::HeaderMap,
) -> Value {
    // Build metadata with all request context available for validation_logic
    let mut metadata = if req_metadata.is_object() {
        req_metadata.clone()
    } else {
        json!({})
    };
    // F4: stamp the resolved channel name (overriding any caller-supplied
    // value) so circuit-breaker keys and connector metrics are labeled
    // `channel:connector` instead of `unknown:connector`.
    metadata["channel"] = json!(channel);
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
    metadata
}

/// The async-submission branch of [`dynamic_handler`]: acquire backpressure,
/// create the pending trace (or a synthetic id in `off` mode), enqueue the
/// message, and answer 202.
async fn submit_async(
    state: &AppState,
    channel: String,
    data: Value,
    metadata: Value,
    channel_runtime: Option<std::sync::Arc<crate::channel::ChannelRuntimeConfig>>,
    profile_requested: bool,
) -> Result<Response, OrionError> {
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
            let mut resp = (
                StatusCode::ACCEPTED,
                Json(json!({ "trace_id": null, "trace_token": null })),
            )
                .into_response();
            if let Ok(value) = axum::http::HeaderValue::from_str(&format!(
                "299 - \"Trace persistence disabled for channel '{channel}'\""
            )) {
                resp.headers_mut().insert("warning", value);
            }
            (id, resp)
        } else {
            let input_json = serde_json::to_string(&data).ok();
            let channel_id = channel_runtime
                .as_ref()
                .map(|c| c.channel.channel_id.as_str());
            // R12: mint an opaque capability token for this submission.
            // Only its hash is stored; the plaintext exists once, in this
            // 202. Polling requires it (or an admin credential), so a
            // caller can read its own async result but nobody else's.
            let token = uuid::Uuid::new_v4().simple().to_string();
            let token_hash = crate::server::admin_auth::hash_trace_token(&token);
            let trace = state
                .trace_repo
                .create_pending(
                    &channel,
                    channel_id,
                    "async",
                    input_json.as_deref(),
                    Some(&token_hash),
                )
                .await?;
            let id = trace.id.clone();
            let resp = (
                StatusCode::ACCEPTED,
                Json(json!({ "trace_id": trace.id, "trace_token": token })),
            )
                .into_response();
            (id, resp)
        };

    state
        .trace_queue
        .submit(crate::queue::QueueMessage {
            trace_id,
            channel,
            payload: data,
            metadata,
            trace_headers,
            profile_requested,
            backpressure_permit,
        })
        .await?;

    Ok(response)
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

Poll `GET /api/v1/admin/traces/{id}` with the returned `trace_id` for the \
result, presenting the returned `trace_token` via the `x-trace-token` header \
or `?token=` query parameter. The token scopes the poll to this submission \
(R12); an admin credential also works.",
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
// exactly once (see `sync::process_sync_for_channel`). These mirrors exist
// purely so the OpenAPI document describes the real shape; they are registered
// in `openapi::ApiDoc` and never constructed at runtime.

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
    /// Id to poll via `GET /api/v1/admin/traces/{id}`, or `null` when trace
    /// persistence is disabled for the channel (see the `Warning` header).
    trace_id: Option<String>,
    /// Capability token scoping the poll to this submission (R12): present
    /// it via the `x-trace-token` header or `?token=` query parameter.
    /// Shown once, here — only its hash is stored. `null` whenever
    /// `trace_id` is.
    trace_token: Option<String>,
}
