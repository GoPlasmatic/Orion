//! Synchronous data-plane processing: the engine run, response envelope
//! construction, sanitization (G1), and trace persistence + response caching
//! for completed sync requests.

use std::time::Instant;

use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{Value, json};

use crate::channel::guards::{self, CacheLookup, CacheStoreCtx};
use crate::channel::registry::EffectiveTraceConfig;
use crate::config::TraceStorageMode;
use crate::errors::OrionError;
use crate::metrics;
use crate::queue::{TracePersistenceQueue, TracePersistenceTask};
use crate::server::state::AppState;
use crate::storage::repositories::traces::TraceCompletedRow;

use crate::engine::utils::{inject_rollout_bucket, merge_metadata, remove_rollout_bucket};

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
                &state.config.trace_storage,
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
pub(super) async fn process_sync_for_channel(
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
    let result = crate::engine::run_for_channel(
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
        Err(ms) => {
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

            let response = response_envelope(
                message.id(),
                crate::engine::utils::data_without_rollout_bucket(&message),
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
                Some(response_envelope(
                    message.id(),
                    crate::engine::utils::data_without_rollout_bucket(&message),
                    sanitize_errors(message.errors()),
                    Some(request_id),
                ))
            } else {
                None
            };
            let public_json = match &public_response {
                Some(v) => Some(serde_json::to_string(v).map_err(|e| {
                    OrionError::Internal(format!("Failed to serialize response: {e}"))
                })?),
                None => None,
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

            let input_json = serde_json::to_string(&data).ok();
            let task_trace_json = crate::engine::utils::serialize_task_trace_capped(
                task_trace.as_ref(),
                max_result_size,
                channel,
            );
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
            // B3 shape lock: the debug surface lives under a single
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
        let info = |code: &str, message: &str, task_id: Option<&str>| dataflow_rs::ErrorInfo {
            code: code.to_string(),
            message: message.to_string(),
            path: None,
            workflow_id: None,
            task_id: task_id.map(str::to_string),
            timestamp: None,
            retry_attempted: None,
            retry_count: None,
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
