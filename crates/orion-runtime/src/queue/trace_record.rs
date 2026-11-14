//! Turning a finished run into a `traces` row.
//!
//! This lived in `server::routes::data::sync` and served only the HTTP sync
//! path. A transport that shared admission (`channel::guards::admit`) and
//! dispatch (`engine::execute_admitted`) with HTTP but wrote no trace left its
//! messages invisible to `GET /api/v2/data/traces` and to DLQ retry — a silent
//! data loss in the one subsystem whose job is to record what happened.
//!
//! It lives here, and not under `server/`, because the non-HTTP transports
//! (cron) may not reach into `crate::server::` — the layering guard forbids
//! it, and rightly: a scheduler is not an HTTP concern.

use crate::channel::registry::EffectiveTraceConfig;
use crate::config::TraceStorageMode;
use crate::metrics;
use crate::storage::repositories::traces::TraceCompletedRow;

use super::{TracePersistenceQueue, TracePersistenceTask};

/// One completed trace, ready for persistence.
///
/// One value rather than six positional arguments, which is what it was for
/// when it served only the sync path.
///
/// The three payloads are owned, or owned when the caller can give them up,
/// because the queued modes hand the row to a background worker: a borrowed
/// string there is a full copy of the request body or the response, per
/// request. `input_json` and `task_trace_json` are built for the trace alone,
/// so they always move; `response_json` moves when nothing else needs it
/// after the trace is handed over (it is often also the HTTP body).
pub struct CompletedTrace<'a> {
    /// `"sync"`, `"async"` or `"cron"` — how the message reached the runtime.
    /// Stored on the row and offered as a filter by `GET /data/traces`.
    pub mode: &'a str,
    pub channel: &'a str,
    pub channel_id: Option<&'a str>,
    pub input_json: Option<String>,
    pub response_json: std::borrow::Cow<'a, str>,
    pub duration_ms: f64,
    pub has_errors: bool,
    /// Whether execution stopped with a timeout or engine error.
    pub failed: bool,
    pub task_trace_json: Option<String>,
    /// Why a failed run failed (`OrionError::trace_text`); `None` when it
    /// completed.
    pub error_message: Option<String>,
}

/// Route a completed sync trace through the chosen persistence mode.
///
/// The drop decision is the caller's, not this function's: it gates work that
/// happens *before* a `CompletedTrace` can be built. See
/// [`TracePlan::decide`].
pub async fn route_store_completed(
    cfg: &EffectiveTraceConfig,
    trace_repo: &dyn crate::storage::repositories::traces::TraceSink,
    persistence_queue: &TracePersistenceQueue,
    trace: CompletedTrace<'_>,
) {
    // `should_drop` already returned for `Off`; remaining modes are Sync / Async / Batch.
    //
    // Sync awaits the write attempt. A failed insert is logged and leaves the
    // workflow response unchanged, since external effects may have happened.
    if matches!(cfg.mode, TraceStorageMode::Sync) {
        if let Err(e) = trace_repo
            .store_completed(crate::storage::repositories::traces::TraceCompletedRef {
                failed: trace.failed,
                channel: trace.channel,
                channel_id: trace.channel_id,
                mode: trace.mode,
                input_json: trace.input_json.as_deref(),
                result_json: &trace.response_json,
                duration_ms: trace.duration_ms,
                task_trace_json: trace.task_trace_json.as_deref(),
                error_message: trace.error_message.as_deref(),
            })
            .await
        {
            tracing::warn!(error = %e, "Failed to store sync processing result");
        }
    } else {
        let task = TracePersistenceTask::StoreCompleted(TraceCompletedRow {
            failed: trace.failed,
            channel: trace.channel.to_string(),
            channel_id: trace.channel_id.map(str::to_string),
            mode: trace.mode.to_string(),
            input_json: trace.input_json,
            result_json: trace.response_json.into_owned(),
            duration_ms: trace.duration_ms,
            task_trace_json: trace.task_trace_json,
            error_message: trace.error_message,
        });
        persistence_queue.submit(task).await;
    }
}

/// Whether this trace will be persisted, decided *before* the strings it would
/// need are built.
///
/// Consulting the filters (`off`, `errors_only`, sampling) inside
/// [`route_store_completed`], at the end of the request, would come after the
/// caller had already serialized the request payload to a `String` and capped
/// the task trace to hand it over — only to drop both. That is a full copy of
/// every request body on the hottest path in the product, paid in exactly the
/// configurations chosen to *avoid* trace cost: `mode = "off"`, `errors_only`
/// on a clean run, or any `sample_rate` below 1.
///
/// Deciding first makes the drop actually free.
pub enum TracePlan {
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
    pub fn decide(cfg: &EffectiveTraceConfig, has_errors: bool) -> Self {
        match cfg.should_drop(has_errors, cfg.draw_sample()) {
            Some(reason) => {
                metrics::record_trace_dropped(reason);
                Self::Drop
            }
            None => Self::Persist,
        }
    }

    pub fn persists(&self) -> bool {
        matches!(self, Self::Persist)
    }
}

/// Where [`record_run`] writes: the trace repository, the persistence queue,
/// and the cap on a stored result.
pub struct RunTraceSink<'a> {
    pub trace_repo: &'a dyn crate::storage::repositories::traces::TraceSink,
    pub persistence_queue: &'a TracePersistenceQueue,
    /// `trace_queue.max_result_size_bytes`; `0` is no cap.
    pub max_result_size_bytes: usize,
}

/// Persist the trace of one run an edition's driver made (a delivery from its
/// own listener, or a run of a hooked channel), under the channel's own
/// `tracing` block, with `mode` = the driver's protocol. A trace that cannot
/// be written is logged and dropped: it must not change what the driver does
/// with the run.
pub async fn record_run(
    sink: RunTraceSink<'_>,
    mode: &str,
    runtime: &crate::channel::ChannelRuntimeConfig,
    channel: &str,
    input_json: Option<String>,
    execution: &crate::engine::Execution,
) {
    let has_errors = !execution.outcome.is_ok();
    let cfg = runtime.trace_storage;
    if !TracePlan::decide(&cfg, has_errors).persists() {
        return;
    }
    // The data, and the task errors beside it when the run recorded any, as
    // the sync path stores them: a failed run's row says why it failed.
    let errors = execution.message.errors();
    let result = if errors.is_empty() {
        execution.data()
    } else {
        serde_json::json!({ "data": execution.data(), "errors": errors })
    };
    let Ok(result_json) = serde_json::to_string(&result) else {
        tracing::warn!(channel, "Failed to serialize a run's result for its trace");
        return;
    };
    if sink.max_result_size_bytes > 0 && result_json.len() > sink.max_result_size_bytes {
        metrics::record_trace_dropped("result_too_large");
        tracing::warn!(
            channel,
            result_bytes = result_json.len(),
            cap = sink.max_result_size_bytes,
            "Run result exceeds trace_queue.max_result_size_bytes; trace not stored"
        );
        return;
    }
    let task_trace_json = crate::engine::utils::serialize_task_trace_capped(
        execution.task_trace.as_ref(),
        sink.max_result_size_bytes,
        channel,
    );
    route_store_completed(
        &cfg,
        sink.trace_repo,
        sink.persistence_queue,
        CompletedTrace {
            mode,
            channel,
            channel_id: Some(&runtime.channel.channel_id),
            input_json,
            response_json: std::borrow::Cow::Owned(result_json),
            duration_ms: execution.duration.as_secs_f64() * 1000.0,
            has_errors,
            failed: matches!(
                execution.outcome,
                crate::engine::RunOutcome::Timeout(_) | crate::engine::RunOutcome::EngineError(_)
            ),
            task_trace_json,
            error_message: execution.outcome.error_text(channel),
        },
    )
    .await;
}
