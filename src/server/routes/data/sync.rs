//! Synchronous data-plane processing: the engine run, response envelope
//! construction, sanitization (G1), and trace persistence + response caching
//! for completed sync requests.

use std::time::Instant;

use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{Value, json};

use crate::channel::guards::{Admission, CacheStoreCtx};
use crate::channel::registry::EffectiveTraceConfig;
use crate::config::TraceStorageMode;
use crate::errors::OrionError;
use crate::metrics;
use crate::queue::{TracePersistenceQueue, TracePersistenceTask};
use crate::server::state::AppState;
use crate::storage::repositories::traces::TraceCompletedRow;

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
///
/// The drop decision is the caller's, not this function's: it gates work that
/// happens *before* a `CompletedTrace` can be built. See
/// [`TracePlan::decide`].
async fn route_store_completed(
    cfg: &EffectiveTraceConfig,
    trace_repo: &std::sync::Arc<dyn crate::storage::repositories::traces::TraceRepository>,
    persistence_queue: &TracePersistenceQueue,
    trace: &CompletedTrace<'_>,
) {
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

/// Whether this trace will be persisted, decided *before* the strings it would
/// need are built.
///
/// The filters (`off`, `errors_only`, sampling) used to be consulted inside
/// [`route_store_completed`], at the end of the request — after the caller had
/// already serialized the request payload to a `String` and capped the task
/// trace to hand it over. Both were then dropped on the floor. That is a full
/// copy of every request body on the hottest path in the product, paid in
/// exactly the configurations chosen to *avoid* trace cost: `mode = "off"`,
/// `errors_only` on a clean run, or any `sample_rate` below 1.
///
/// Deciding first makes the drop actually free.
enum TracePlan {
    Persist,
    /// The reason is not carried: `decide` has already reported it to
    /// `orion_traces_dropped_total`, which is where an operator looks.
    Drop,
}

impl TracePlan {
    /// N22: the sampling coin is drawn exactly once per trace, here — the
    /// single point a sync trace's persistence is decided — so a sampled-out
    /// trace produces no rows at all (the sync path writes no separate status
    /// row; skipping this write skips the trace entirely).
    fn decide(cfg: &EffectiveTraceConfig, has_errors: bool) -> Self {
        match cfg.should_drop(has_errors, cfg.draw_sample()) {
            Some(reason) => {
                metrics::record_trace_dropped(reason);
                Self::Drop
            }
            None => Self::Persist,
        }
    }

    fn persists(&self) -> bool {
        matches!(self, Self::Persist)
    }
}

/// Build an HTTP response from a pre-serialized JSON string, avoiding
/// the double-serialization that `Json<Value>` would incur.
///
/// G9: assembled directly rather than through `Response::builder()`, whose
/// `body()` returns a `Result` that had to be `.expect()`ed — on **every
/// successful data request**, the hottest path in the product, inside a crate
/// that sets `#![warn(clippy::panic)]`. The inputs are a status code and a
/// static header, so there was never a real failure to handle; building the
/// value directly removes the `Result` instead of asserting past it.
fn json_response(status: StatusCode, body: String) -> Response {
    let mut response = Response::new(axum::body::Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    response
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
    channel_config: &std::sync::Arc<crate::channel::ChannelRuntimeConfig>,
    plan: &TracePlan,
    trace: &CompletedTrace<'_>,
    cache_body: &str,
    cache_context: &Option<CacheStoreCtx>,
    profile: Option<&std::sync::Arc<crate::engine::profile::ProfileCollector>>,
) {
    // The cache store below is not part of the trace decision: a sampled-out or
    // `errors_only` trace still populates the response cache.
    if plan.persists() {
        let effective_trace = channel_config.trace_storage;
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
    }

    // Fire-and-forget cache store. N2: never cache a response carrying task
    // errors — one transient downstream failure would otherwise be pinned
    // for the full TTL and replayed to every caller, long after the
    // dependency recovered.
    if trace.has_errors {
        tracing::debug!(
            channel = trace.channel,
            "Response has task errors; not caching"
        );
        return;
    }
    if let Some((key, cache, ttl)) = cache_context
        && let Err(e) = cache.set_ex(key, cache_body, *ttl).await
    {
        tracing::debug!(channel = trace.channel, error = %e, "Failed to cache response");
    }
}

/// Build the synchronous response envelope.
///
/// R23: there were four `json!` literals producing one documented shape — the
/// full-detail body, the sanitized public body, the profile variant, and the
/// cached copy — against a `ProcessResponse` mirror whose own doc comment said
/// it is *"never constructed at runtime"*. Four writers and no reader is how a
/// schema drifts from the thing it describes. One writer now, and
/// `the_response_envelope_matches_its_documented_schema` deserializes what it
/// produces back into `ProcessResponse` with `deny_unknown_fields`, so the
/// mirror is pinned to reality rather than to good intentions.
///
/// `request_id` is present only on the sanitized body: the full messages live
/// in the trace, and the id is how a caller correlates the two.
pub(super) fn response_envelope(
    id: &str,
    data: Value,
    errors: Vec<Value>,
    request_id: Option<String>,
) -> Value {
    let mut envelope = json!({
        "id": id,
        "status": "ok",
        "data": data,
        "errors": errors,
    });
    if let Some(request_id) = request_id {
        envelope["request_id"] = json!(request_id);
    }
    envelope
}

/// Serialize a response envelope, mapping the (unreachable in practice)
/// serializer failure to the one `Internal` message every envelope site used
/// to spell out for itself.
fn serialize_envelope(envelope: &Value) -> Result<String, OrionError> {
    serde_json::to_string(envelope)
        .map_err(|e| OrionError::Internal(format!("Failed to serialize response: {e}")))
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

/// Serve a response-cache hit: the cached body is already the client-facing
/// serialization, so it goes out verbatim with no work at all.
pub(super) fn cached_response(body: String) -> Response {
    json_response(StatusCode::OK, body)
}

/// Core sync processing logic shared between simple HTTP and REST routes.
/// Every ingress guard — origin allow-list, rate limit, validation, dedup,
/// response-cache lookup, backpressure — has already run in the caller's
/// `apply_guards` call, before the sync/async split; what arrives here is the
/// resulting [`Admission`].
///
/// Returns a pre-serialized `Response` so the JSON is serialized exactly once.
pub(super) async fn process_sync_for_channel(
    state: &AppState,
    channel: &str,
    data: Value,
    metadata: Value,
    channel_config: std::sync::Arc<crate::channel::ChannelRuntimeConfig>,
    profile_requested: bool,
    admission: Admission,
) -> Result<Response, OrionError> {
    let profile = profile_requested.then(crate::engine::profile::ProfileCollector::new);
    let Admission {
        backpressure_permit: _backpressure_permit,
        cache_store: cache_context,
        timeout_ms,
        // Nothing redelivers an HTTP request, so the claim is left standing
        // for the rest of the window: a replay of the same idempotency key is
        // a second delivery and belongs in the `409` branch, whatever this
        // one's outcome turns out to be.
        dedup_claim: _dedup_claim,
    } = admission;

    // O1: `channel` is safe to use as a metric label below because the caller
    // has already resolved it against the registry — an unknown name is a 404
    // before it reaches here, so callers cannot grow Prometheus label
    // cardinality by inventing path segments.

    let start = Instant::now();
    let engine = crate::engine::acquire_engine_read(&state.engine).await;
    let sticky_identity = crate::engine::utils::rollout_identity(
        &metadata,
        &state.config.engine.rollout_sticky_header,
    );
    // The routing bucket rides beside the context rather than inside `data`,
    // so it never reaches the caller and never needs stripping back out.
    let mut message = dataflow_rs::Message::builder()
        .payload_json(&data)
        .metadata_json(&metadata)
        .routing_bucket(crate::engine::utils::rollout_bucket_for_identity(
            sticky_identity,
        ))
        .build();

    // A2: when the channel opted in via `config.tracing.task_details = true`,
    // use the with-trace engine entry point so per-step inputs/outputs are
    // captured for persistence.
    // Bounded by the same budget the persisted row is capped at, so the
    // post-hoc `serialize_task_trace_capped` check becomes a backstop rather
    // than the only defence.
    let capture = channel_config
        .trace_storage
        .task_details
        .then(|| crate::engine::TraceCapture {
            max_snapshot_bytes: state.config.trace_queue.max_result_size_bytes,
        });

    let workflow_start = Instant::now();
    let result = crate::engine::run_for_channel(
        &engine,
        channel,
        &mut message,
        timeout_ms,
        profile.as_ref(),
        capture,
    )
    .await;
    if let Some(ref p) = profile {
        p.set_workflow_total(workflow_start.elapsed());
    }

    let (result, task_trace) = match result {
        Ok(inner) => inner,
        Err(ms) => {
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
            let duration = start.elapsed();
            let duration_secs = duration.as_secs_f64();
            let duration_ms = duration.as_secs_f64() * 1000.0;
            metrics::record_message(channel, "ok");
            metrics::record_message_duration(channel, duration_secs);

            let mut response = response_envelope(
                message.id(),
                message.data().into(),
                message
                    .errors()
                    .iter()
                    .filter_map(|e| serde_json::to_value(e).ok())
                    .collect(),
                None,
            );

            // Serialize the full-detail envelope exactly once — reused for the
            // size check, trace storage, and (on the error-free hot path) the
            // cache and HTTP body with no re-serialization by Axum.
            let response_json = serialize_envelope(&response)?;

            // G1: when workflow errors are present, the client-facing body
            // (and the cached copy) carry sanitized entries plus a
            // correlation id; the persisted trace keeps the full detail.
            //
            // The two envelopes differ only in `errors` and `request_id`, so
            // the public one is made by overwriting those two keys in place —
            // rebuilding it would deep-convert the whole workflow output a
            // second time.
            let has_errors = message.has_errors();
            let public_json = if has_errors {
                response["errors"] = Value::Array(sanitize_errors(message.errors()));
                response["request_id"] =
                    json!(crate::server::request_context::request_id().unwrap_or_default());
                Some(serialize_envelope(&response)?)
            } else {
                None
            };

            let max_result_size = state.config.trace_queue.max_result_size_bytes;
            if max_result_size > 0 && response_json.len() > max_result_size {
                metrics::record_error("result_size_exceeded");
                return Err(OrionError::ResponseTooLarge(format!(
                    "Result size {} bytes exceeds limit of {} bytes",
                    response_json.len(),
                    max_result_size
                )));
            }

            // Decided before the two strings below are built: under `off`,
            // `errors_only` on a clean run, or any sample_rate < 1, both are
            // pure waste — and a full copy of the request payload is not a
            // cheap kind of waste on this path.
            let plan = TracePlan::decide(&channel_config.trace_storage, has_errors);
            let (input_json, task_trace_json) = if plan.persists() {
                (
                    serde_json::to_string(&data).ok(),
                    crate::engine::utils::serialize_task_trace_capped(
                        task_trace.as_ref(),
                        max_result_size,
                        channel,
                    ),
                )
            } else {
                (None, None)
            };
            persist_trace_and_cache(
                state,
                &channel_config,
                &plan,
                &CompletedTrace {
                    channel,
                    channel_id: Some(channel_config.channel.channel_id.as_str()),
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
            // B3 shape lock: the debug surface lives under a single
            // top-level `_orion` namespace so future debug fields (e.g.
            // `_orion.task_trace`) can be added without colliding with
            // workflow-level output keys that callers control.
            if let Some(ref p) = profile {
                let mut response_with_profile = response;
                response_with_profile["_orion"] = json!({ "profile": p.to_json() });
                return Ok(json_response(
                    StatusCode::OK,
                    serialize_envelope(&response_with_profile)?,
                ));
            }

            Ok(json_response(
                StatusCode::OK,
                public_json.unwrap_or(response_json),
            ))
        }
        Err(e) => {
            metrics::record_message(channel, "error");
            metrics::record_error("engine");
            Err(OrionError::Engine(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::routes::data::ProcessResponse;

    /// R23: `ProcessResponse` is a never-constructed mirror that exists only so
    /// the OpenAPI document describes the real shape — and the real shape was
    /// built by four separate `json!` literals, one of which (the plain success
    /// body) omits `request_id` while another adds it. Nothing checked the two
    /// against each other, and they had already drifted.
    ///
    /// Deserializing with `deny_unknown_fields` catches drift in both
    /// directions: a key the envelope emits and the schema lacks fails here,
    /// and a required schema field the envelope omits fails here too.
    #[test]
    fn the_response_envelope_matches_its_documented_schema() {
        let shapes = [
            // Clean run: no request_id.
            response_envelope("msg-1", json!({"ok": true}), vec![], None),
            // Sanitized run: request_id present, errors non-empty.
            response_envelope(
                "msg-2",
                json!({"partial": 1}),
                vec![json!({"code": "TASK_FAILED", "message": SANITIZED_ERROR_MESSAGE})],
                Some("req-abc".to_string()),
            ),
            // Sanitized run carrying a task_id.
            response_envelope(
                "msg-3",
                Value::Null,
                vec![json!({
                    "code": "TASK_FAILED",
                    "message": SANITIZED_ERROR_MESSAGE,
                    "task_id": "t1",
                })],
                Some("req-def".to_string()),
            ),
        ];

        for shape in shapes {
            let parsed = serde_json::from_value::<ProcessResponse>(shape.clone());
            assert!(
                parsed.is_ok(),
                "the documented schema does not describe what we send: {shape} — {:?}",
                parsed.err()
            );
        }
    }

    /// The profile variant is the same envelope plus the `_orion` namespace
    /// (B3), built by the one site that appends it.
    #[test]
    fn the_profile_variant_also_matches_the_schema() {
        let mut shape = response_envelope("msg-4", json!({}), vec![], None);
        shape["_orion"] = json!({ "profile": {"version": 2} });
        let parsed = serde_json::from_value::<ProcessResponse>(shape.clone());
        assert!(
            parsed.is_ok(),
            "profile variant does not match the schema: {shape} — {:?}",
            parsed.err()
        );
    }

    /// `sanitize_errors` output is what the envelope carries, so it must satisfy
    /// the `ProcessTaskError` half of the schema too.
    #[test]
    fn sanitized_errors_match_their_documented_schema() {
        use crate::server::routes::data::ProcessTaskError;
        let info = |code: &str, message: &str, task_id: Option<&str>| {
            // `ErrorInfo` is `#[non_exhaustive]` as of dataflow-rs 3.1, so the
            // builder is the only cross-crate construction path.
            let mut b = dataflow_rs::ErrorInfo::builder(code, message);
            if let Some(task_id) = task_id {
                b = b.task_id(task_id);
            }
            b.build()
        };
        let errors = sanitize_errors(&[
            info(
                "TASK_FAILED",
                "raw upstream detail that must not leak",
                Some("t1"),
            ),
            info("OTHER", "another", None),
        ]);
        for e in &errors {
            let parsed = serde_json::from_value::<ProcessTaskError>(e.clone());
            assert!(
                parsed.is_ok(),
                "sanitized error does not match the schema: {e} — {:?}",
                parsed.err()
            );
        }
        // And the sanitisation itself still holds.
        assert!(
            errors
                .iter()
                .all(|e| e["message"] == SANITIZED_ERROR_MESSAGE),
            "{errors:?}"
        );
    }
}
