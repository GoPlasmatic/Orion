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
    pub task_trace_json: Option<String>,
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
    // `sync` awaits the insert before the caller answers: that is the mode's
    // documented promise (`docs/src/operate/run/traces.md` — "written inline
    // before responding", "a served request implies a persisted trace"), so a
    // caller holding a response can read its trace, and a crash after the
    // response cannot lose it. Moving the insert off the request path is what
    // `async` and `batch` are for.
    if matches!(cfg.mode, TraceStorageMode::Sync) {
        if let Err(e) = trace_repo
            .store_completed(crate::storage::repositories::traces::TraceCompletedRef {
                channel: trace.channel,
                channel_id: trace.channel_id,
                mode: trace.mode,
                input_json: trace.input_json.as_deref(),
                result_json: &trace.response_json,
                duration_ms: trace.duration_ms,
                task_trace_json: trace.task_trace_json.as_deref(),
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
            input_json: trace.input_json,
            result_json: trace.response_json.into_owned(),
            duration_ms: trace.duration_ms,
            task_trace_json: trace.task_trace_json,
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
