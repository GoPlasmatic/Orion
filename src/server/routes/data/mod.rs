mod sync;
pub(crate) mod traces;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::channel::guards;
use crate::errors::OrionError;
use crate::server::extract::{OrionQuery, PeerAddr};
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
  workflow as `metadata.params`, percent-decoded exactly once (`a%2Fb` becomes \
  `a/b`). Static segments match byte-exact — the path is case-sensitive per \
  RFC 3986. Any HTTP method is accepted — `GET`, `PUT`, \
  `PATCH` and `DELETE` behave identically to the `POST` documented here, with \
  the verb exposed as `metadata.http_method`. Query the admin channel API for \
  the routes a given deployment actually serves.

Append `/async` to submit to the queue instead — see \
`POST /api/v1/data/{channel}/async`.

Authentication is per channel and off by default: `admin_auth` covers the admin \
plane only. A channel carrying an `auth` block in its config (`api_key` or \
`hmac`) authenticates every caller on this path and on `/async` alike; a channel \
without one is open to anyone who can reach the port. `validation_logic` and \
`origin_allow_list` complement it but are not authentication — `Origin` is \
client-supplied.",
    params(
        ("channel" = String, Path, description = "Channel name, or the first segment of a REST channel's registered route pattern."),
        ("profile" = Option<bool>, Query, description = "Set to `1`/`true` to append `_orion.profile` timings to the response. Requires `tracing.debug_profile_enabled = true`; the `X-Orion-Profile` header does the same."),
    ),
    request_body(
        content = ProcessRequest,
        description = "Workflow input, in either of two shapes. An object carrying `data` or `metadata` is the \
**envelope**: `data` is the payload, and `metadata` is merged into the message metadata alongside the \
server-supplied `channel`, `http_method`, `params`, `query` and `headers` keys. Any other JSON body **is** \
the payload — `{\"amount\": 5}` is equivalent to `{\"data\": {\"amount\": 5}}`. An empty body is accepted \
(typical for `GET`/`DELETE` REST channels) and treated as `{\"data\": {}}`.",
        content_type = "application/json",
    ),
    responses(
        (status = 200, description = "Workflow completed. `errors` is empty on success; when tasks failed it carries sanitized `{code, message, task_id}` entries and the envelope gains a `request_id` for correlation with the persisted trace.", body = ProcessResponse),
        (status = 400, description = "Malformed JSON body, empty channel segment, an invalid percent-sequence in the request path, or a channel `validation_logic` rejection (`VALIDATION_ERROR`, with per-field `details`)", body = ErrorResponse),
        (status = 401, description = "The channel declares `auth` and the request did not satisfy it — absent, wrong, or malformed credential. One message for every cause, so a caller cannot learn which half they had right.", body = ErrorResponse),
        (status = 403, description = "`Origin` header not in the channel's `origin_allow_list`", body = ErrorResponse),
        (status = 404, description = "No channel serves this request: either no REST route matches the requested method and path, or the single-segment name is not an active channel in the registry.", body = ErrorResponse),
        (status = 409, description = "Deduplication key already seen inside the channel's dedup window", body = ErrorResponse),
        (status = 415, description = "Non-empty body without a JSON `Content-Type`", body = ErrorResponse),
        (status = 429, description = "Rate limit exceeded (global or per-channel)", body = ErrorResponse),
        (status = 500, description = "Result exceeded `queue.max_result_size_bytes` (`RESPONSE_TOO_LARGE`), or an internal failure (`INTERNAL_ERROR`)", body = ErrorResponse),
        (status = 503, description = "Channel backpressure limit reached, a connector circuit breaker is open (`CIRCUIT_OPEN`), or a rate-limit/dedup backend outage on a channel configured with `on_backend_error = \"deny\"`", body = ErrorResponse),
        (status = 504, description = "Workflow exceeded the channel's `timeout_ms`", body = ErrorResponse),
    )
)]
#[tracing::instrument(
    skip(state, uri, headers, peer, query_params, body),
    fields(path = %uri.path())
)]
pub(crate) async fn dynamic_handler(
    State(state): State<AppState>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    PeerAddr(peer): PeerAddr,
    OrionQuery(query_params): OrionQuery<std::collections::HashMap<String, String>>,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, OrionError> {
    // N10: the raw, still-encoded path (the nested router has stripped the
    // `/api/v1/data` prefix). `Path<String>` would hand us the wildcard
    // percent-decoded once by axum — leniently, with invalid sequences
    // passed through — which made `%2F` act as a segment separator before
    // matching and would double-decode anything the matcher then decoded.
    // Splitting and decoding both belong to `RouteTable::match_route`:
    // split first on raw `/`, then decode each segment exactly once.
    //
    // Leading slashes are trimmed before the `/async` check so a bare
    // `/async` path stays a channel *named* `async` rather than a submission
    // with no channel: only a `/async` suffix on a non-empty path is a
    // submission.
    let path = uri.path().trim_start_matches('/');

    // Strip trailing /async suffix
    let (route_path, is_async) = if let Some(stripped) = path.strip_suffix("/async") {
        (stripped, true)
    } else {
        (path, false)
    };

    let route_path = route_path.trim_matches('/').trim();
    if route_path.is_empty() {
        return Err(OrionError::validation("Channel name must not be empty"));
    }

    // Resolve channel: try REST route table first, then direct name lookup.
    // N10: `?` answers 400 for invalid percent-sequences before any
    // resolution; matched params arrive percent-decoded exactly once.
    let (channel, route_params) = if let Some(rm) = state
        .channel_registry
        .match_route(method.as_str(), route_path)?
    {
        (rm.channel_name, rm.params)
    } else if !route_path.contains('/') {
        // Single segment — treat as simple channel name (backward compat),
        // decoded once like a captured param so the encoded spelling of a
        // name reaches the same channel. `match_route` above has already
        // answered 400 for invalid sequences, so the decode fallback cannot
        // fire. A name that decodes to whitespace (`%20`) is as empty as a
        // literal blank — same 400 as the raw-path emptiness check above.
        let name = crate::channel::routing::percent_decode_segment(route_path)
            .map(std::borrow::Cow::into_owned)
            .unwrap_or_else(|| route_path.to_string());
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(OrionError::validation("Channel name must not be empty"));
        }
        (name, std::collections::HashMap::new())
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

    // R13: envelope, bare payload, or empty — one rule, see `from_body`.
    let req = ProcessRequest::from_body(&body)?;

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
    // appending `/async` must not bypass the origin allow-list,
    // validation_logic, the rate limit, deduplication or backpressure. Which
    // guards run is the transport's `GuardSet`, not a decision made here.
    // F35: a channel that failed to load is quarantined, not silently
    // config-less — serving it here would apply none of its guards.
    let channel_runtime = state.channel_registry.require_serviceable(&channel)?;
    // A name that is not in the registry is not an active channel. Without
    // this check the single-segment fallback above accepted ANY name and ran
    // the engine against an empty workflow set — a 200 "ok" for channels
    // that never existed or were just archived (the ingress-side twin of the
    // channel_call missing-target bug).
    // The `Option` binding stays because `guards::GuardRequest` takes it by
    // reference and is shared with the Kafka and channel_call ingresses; the
    // clone below is the `Arc` every path past the gate is entitled to.
    let Some(runtime) = channel_runtime.clone() else {
        return Err(OrionError::NotFound(format!(
            "Channel '{channel}' not found or not active"
        )));
    };

    let header_lookup = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    // S8: the same trusted-proxy-gated identity the platform limiter uses, so
    // a channel's limit cannot be side-stepped with a spoofed
    // `X-Forwarded-For` that the platform limiter would have ignored.
    let client_ip = crate::server::rate_limit::client_ip_from_parts(
        peer.as_ref(),
        &headers,
        state.trusted_proxies(),
    );
    let transport = if is_async {
        guards::Transport::HttpAsync
    } else {
        guards::Transport::HttpSync
    };
    let admission = match guards::apply_guards(guards::GuardRequest {
        transport,
        channel: &channel,
        runtime: &channel_runtime,
        data: &req.data,
        metadata: &metadata,
        datalogic: &state.datalogic,
        origin: headers.get("origin").and_then(|v| v.to_str().ok()),
        caller_identity: &client_ip,
        header: &header_lookup,
        // The bytes as received, not `req.data`. A webhook signature is
        // computed over the wire body, and `ProcessRequest::from_body` has
        // already parsed and possibly unwrapped the `data` envelope — signing
        // over that would never match.
        raw_body: Some(&body),
        dedup_key_fallback: None,
        // One HTTP request is one delivery: nothing redelivers it, so the
        // claim gets a token nothing can present again and a replay of the
        // key is the `409` it should be.
        dedup_owner: None,
        // The synchronous path has no deadline of its own: a channel that
        // declares no `timeout_ms` runs to completion. The `/async`
        // submission leaves it to the worker, which re-resolves at dequeue
        // time against the config as it stands then.
        default_timeout_ms: None,
        // Neither HTTP path has a ceiling to protect: the deadline is the
        // caller's patience, not a shared resource.
        max_timeout_ms: None,
    })
    .await?
    {
        guards::GuardVerdict::CacheHit(body) => {
            let shaped = runtime
                .parsed_config
                .response
                .as_ref()
                .is_some_and(|cfg| cfg.is_shaped());
            return Ok(sync::cached_response(body, shaped));
        }
        guards::GuardVerdict::Admitted(admission) => admission,
    };

    if is_async {
        return submit_async(
            &state,
            channel,
            req.data,
            metadata,
            runtime,
            profile_requested,
            admission,
        )
        .await;
    }

    process_sync_for_channel(
        &state,
        &channel,
        req.data,
        metadata,
        runtime,
        profile_requested,
        admission,
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

/// The async-submission branch of [`dynamic_handler`]: create the pending
/// trace (or a synthetic id in `off` mode), enqueue the message, and answer
/// 202.
async fn submit_async(
    state: &AppState,
    channel: String,
    data: Value,
    metadata: Value,
    channel_runtime: std::sync::Arc<crate::channel::ChannelRuntimeConfig>,
    profile_requested: bool,
    admission: guards::Admission,
) -> Result<Response, OrionError> {
    // The permit was acquired by the guard chain before the pending trace is
    // created, so a shed request leaves no trace row. It rides inside the
    // queued message and is held by the worker for the duration of
    // processing, so a channel's `max_concurrent_per_node` bounds sync and
    // async work together. `admission.timeout_ms` is deliberately not carried
    // through the queue: the worker re-resolves it at dequeue time, against
    // the config as it stands then.
    //
    // `admission.dedup_claim` is dropped here rather than settled. Nothing
    // redelivers an HTTP submission — the caller holds a `trace_id` and polls
    // it — so the claim standing for the rest of the window is exactly the
    // `409` a second submission of the same key should get, whatever the
    // queued work turns out to do.
    let backpressure_permit = admission.backpressure_permit;

    let trace_headers = {
        let mut h = std::collections::HashMap::new();
        crate::server::trace_context::inject_trace_context(&mut h);
        h
    };

    // R11: the pending row is written unconditionally. `trace.mode = "off"`
    // used to skip it and answer 202 with a null `trace_id` and a `Warning`
    // header — a receipt for work whose result could never be fetched, and a
    // nullable id in the schema forever. Async submission *is* the request for
    // a later result, so persistence is not optional on this path; the sync
    // path still honours `off` exactly (see `for_async_submission`).
    let input_json = serde_json::to_string(&data).ok();
    let channel_id = Some(channel_runtime.channel.channel_id.as_str());
    // R12: mint an opaque capability token for this submission. Only its hash
    // is stored; the plaintext exists once, in this 202. Polling requires it
    // (or an admin credential), so a caller can read its own async result but
    // nobody else's.
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
    let trace_id = trace.id.clone();
    let response: Response = (
        StatusCode::ACCEPTED,
        Json(json!({ "trace_id": trace.id, "trace_token": token })),
    )
        .into_response();

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
the origin allow-list, the rate limit, `validation_logic`, deduplication and \
backpressure. The response cache is sync-only, so an async submission never \
returns a cached body.

Poll `GET /api/v1/admin/traces/{id}` with the returned `trace_id` for the \
result, presenting the returned `trace_token` via the `x-trace-token` header \
or `?token=` query parameter. The token scopes the poll to this submission \
(R12); an admin credential also works.

`trace_id` is always present. Async submission is a request for a result to be \
fetched later, so the trace row is written before the 202 is sent even when \
`trace_storage.mode` is `off` — that setting still applies in full to the \
synchronous endpoint, where the caller already has the answer.",
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
            description = "Accepted and queued. `trace_id` and `trace_token` are always present — the trace row is written before this response is sent, so the id can always be polled.",
            body = AsyncSubmitResponse,
        ),
        (status = 400, description = "Malformed JSON body, empty channel segment, an invalid percent-sequence in the request path, or a `validation_logic` rejection", body = ErrorResponse),
        (status = 401, description = "The channel declares `auth` and the request did not satisfy it — absent, wrong, or malformed credential. One message for every cause, so a caller cannot learn which half they had right.", body = ErrorResponse),
        (status = 403, description = "`Origin` header not in the channel's `origin_allow_list`", body = ErrorResponse),
        (status = 404, description = "No channel serves this request: either no REST route matches the requested method and path, or the single-segment name is not an active channel in the registry.", body = ErrorResponse),
        (status = 409, description = "Deduplication key already seen inside the channel's dedup window", body = ErrorResponse),
        (status = 415, description = "Non-empty body without a JSON `Content-Type`", body = ErrorResponse),
        (status = 429, description = "Rate limit exceeded (global or per-channel)", body = ErrorResponse),
        (status = 503, description = "Channel backpressure limit reached, the trace queue is full/closed, or a rate-limit/dedup backend outage on a channel configured with `on_backend_error = \"deny\"`", body = ErrorResponse),
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

/// An envelope that names `metadata` but not `data` carries no payload — which
/// is `{}`, the same thing an empty body means, not `null`.
fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

#[derive(Deserialize, utoipa::ToSchema)]
pub(crate) struct ProcessRequest {
    /// The workflow payload.
    #[serde(default = "empty_object")]
    data: Value,
    /// Merged into the message metadata alongside the server-supplied keys.
    #[serde(default = "empty_object")]
    metadata: Value,
}

impl ProcessRequest {
    /// Read the request body, which may be the envelope or the payload itself.
    ///
    /// R13: this endpoint had three behaviours for three body shapes, and only
    /// one was documented. `{"data": …}` was the envelope; an **empty** body
    /// became `{"data":{},"metadata":{}}`; and a **bare object** — `{"amount":
    /// 5}`, the obvious thing to send, and what every other JSON API accepts —
    /// failed with `missing field 'data'`. On the most-hit endpoint in the
    /// product.
    ///
    /// The rule is now one sentence: **an object carrying `data` or `metadata`
    /// is the envelope; anything else is the payload.** That keeps every
    /// existing envelope working (they all carry `data`), keeps the empty body
    /// meaning `{}`, and turns the previously-400 bare object into the payload
    /// the caller plainly meant — a strictly widening change.
    fn from_body(body: &[u8]) -> Result<Self, OrionError> {
        if body.is_empty() {
            return Ok(Self {
                data: json!({}),
                metadata: json!({}),
            });
        }
        let parsed: Value = serde_json::from_slice(body)
            .map_err(|e| OrionError::validation(format!("Invalid JSON body: {e}")))?;

        // The envelope's two fields are `Value`, so they are taken straight
        // out of the parsed map rather than re-deserialized — a second pass
        // would rebuild every node of the payload on the hottest endpoint in
        // the product, and could not fail.
        match parsed {
            Value::Object(mut obj) if obj.contains_key("data") || obj.contains_key("metadata") => {
                Ok(Self {
                    data: obj.remove("data").unwrap_or_else(empty_object),
                    metadata: obj.remove("metadata").unwrap_or_else(empty_object),
                })
            }
            other => Ok(Self {
                data: other,
                metadata: json!({}),
            }),
        }
    }
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
///
/// R23: deserializable under `cfg(test)` with `deny_unknown_fields`, so
/// `sync::tests` can round-trip what `response_envelope` actually emits back
/// through this struct. A field added to the envelope and not to the mirror —
/// or vice versa — fails the test instead of shipping a spec that describes a
/// shape nothing sends.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[cfg_attr(test, serde(deny_unknown_fields))]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    /// Debug namespace, present only when profiling was requested and
    /// `tracing.debug_profile_enabled` is on. Currently carries `profile`.
    #[serde(rename = "_orion", default, skip_serializing_if = "Option::is_none")]
    orion: Option<Value>,
}

/// One task failure, with the message replaced by a generic string — upstream
/// URLs, connector names, and driver errors stay in the trace.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[cfg_attr(test, serde(deny_unknown_fields))]
pub(crate) struct ProcessTaskError {
    code: String,
    #[schema(example = "Task processing failed; full detail is available in the trace")]
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
}

/// Acknowledgement returned by `POST /api/v1/data/{channel}/async`.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct AsyncSubmitResponse {
    /// Id to poll via `GET /api/v1/admin/traces/{id}`. Always present: a 202
    /// is a receipt for a result, so the row it names is written before this
    /// response is sent (R11).
    trace_id: String,
    /// Capability token scoping the poll to this submission (R12): present
    /// it via the `x-trace-token` header or `?token=` query parameter.
    /// Shown once, here — only its hash is stored.
    trace_token: String,
}
