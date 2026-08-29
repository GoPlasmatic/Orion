//! Turning a finished run into a `traces` row.
//!
//! This lived in `server::routes::data::sync` and served only the HTTP sync
//! path. The Kafka consumer shares admission (`channel::guards::admit`) and
//! dispatch (`engine::execute_admitted`) with HTTP but wrote no trace at all,
//! so a Kafka-ingested message was invisible to `GET /api/v1/data/traces` and
//! to DLQ retry — a silent data loss in the one subsystem whose job is to
//! record what happened.
//!
//! It lives here, and not under `server/`, because `kafka` may not reach into
//! `crate::server::` — the layering guard forbids it, and rightly: the
//! consumer is not an HTTP concern.

use crate::channel::registry::EffectiveTraceConfig;
use crate::config::TraceStorageMode;
use crate::metrics;
use crate::storage::repositories::traces::TraceCompletedRow;

use super::{TracePersistenceQueue, TracePersistenceTask};

/// One completed trace, ready for persistence.
///
/// Passed as a single borrow rather than six positional arguments, which is
/// what it was for when it served only the sync path.
pub(crate) struct CompletedTrace<'a> {
    /// `"sync"`, `"async"` or `"kafka"` — how the message reached the runtime.
    /// Stored on the row and offered as a filter by `GET /data/traces`.
    pub(crate) mode: &'a str,
    pub(crate) channel: &'a str,
    pub(crate) channel_id: Option<&'a str>,
    pub(crate) input_json: Option<&'a str>,
    pub(crate) response_json: &'a str,
    pub(crate) duration_ms: f64,
    pub(crate) has_errors: bool,
    pub(crate) task_trace_json: Option<&'a str>,
}

/// Route a completed sync trace through the chosen persistence mode.
///
/// The drop decision is the caller's, not this function's: it gates work that
/// happens *before* a `CompletedTrace` can be built. See
/// [`TracePlan::decide`].
pub(crate) async fn route_store_completed(
    cfg: &EffectiveTraceConfig,
    trace_repo: &std::sync::Arc<dyn crate::storage::repositories::traces::TraceRepository>,
    persistence_queue: &TracePersistenceQueue,
    trace: &CompletedTrace<'_>,
) {
    // `should_drop` already returned for `Off`; remaining modes are Sync / Async / Batch.
    if matches!(cfg.mode, TraceStorageMode::Sync) {
        if let Err(e) = trace_repo
            .store_completed(crate::storage::repositories::traces::TraceCompletedRef {
                channel: trace.channel,
                channel_id: trace.channel_id,
                mode: trace.mode,
                input_json: trace.input_json,
                result_json: trace.response_json,
                duration_ms: trace.duration_ms,
                task_trace_json: trace.task_trace_json,
            })
            .await
        {
            tracing::warn!(error = %e, "Failed to store sync processing result");
        }
    } else {
        let task = TracePersistenceTask::StoreCompleted(TraceCompletedRow {
            channel: trace.channel.to_string(),
            channel_id: trace.channel_id.map(str::to_string),
            mode: trace.mode.to_string(),
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
pub(crate) enum TracePlan {
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
    pub(crate) fn decide(cfg: &EffectiveTraceConfig, has_errors: bool) -> Self {
        match cfg.should_drop(has_errors, cfg.draw_sample()) {
            Some(reason) => {
                metrics::record_trace_dropped(reason);
                Self::Drop
            }
            None => Self::Persist,
        }
    }

    pub(crate) fn persists(&self) -> bool {
        matches!(self, Self::Persist)
    }
}
