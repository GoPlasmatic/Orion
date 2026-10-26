use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;

use crate::config::TraceStorageMode;
use crate::metrics;
use crate::storage::models;
use crate::storage::repositories::trace_dlq::TraceDlqRepository;
use crate::storage::repositories::traces::TraceSink;

use super::QueuedItem;

/// Serialize a finished message to JSON, embedding the per-request profile
/// JSON under `_orion.profile` when one is provided (B3 shape lock —
/// matches the sync response envelope).
fn serialize_result_with_profile(
    message: &dataflow_rs::Message,
    profile: Option<&Arc<crate::engine::profile::ProfileCollector>>,
) -> Result<String, serde_json::Error> {
    // No profile is the overwhelmingly common case — it needs both
    // `tracing.debug_profile_enabled` and a per-request opt-in — so serialize
    // straight to the string. Going through an intermediate `Value` builds a
    // full tree copy of every workflow result to satisfy a branch that is
    // almost never taken.
    let Some(p) = profile else {
        return serde_json::to_string(message);
    };
    let mut v = serde_json::to_value(message)?;
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "_orion".to_string(),
            serde_json::json!({ "profile": p.to_json() }),
        );
    }
    serde_json::to_string(&v)
}

/// Shared counters for queue observability metrics.
///
/// Queue *depth* is not here: it belongs to the `BoundedWorker` the dispatcher
/// receives from, which releases each item's reservation as it is dequeued.
/// These two cover what happens after that — how many traces are executing, and
/// how many payload bytes they hold.
pub(super) struct QueueCounters {
    pub(super) active: Arc<AtomicUsize>,
    pub(super) memory_bytes: Arc<AtomicUsize>,
}

/// Bundled context for the dispatcher loop, grouping parameters that share
/// the same lifecycle and reducing positional argument count.
pub(super) struct DispatcherContext {
    pub(super) max_workers: usize,
    pub(super) shutdown_timeout_secs: u64,
    pub(super) counters: QueueCounters,
    pub(super) processing: ProcessingContext,
}

/// Per-task subset of [`DispatcherContext`] — everything `process_trace`
/// needs to execute one queued message. Cloned once per spawn so each task
/// owns its own handles.
#[derive(Clone)]
pub(super) struct ProcessingContext {
    /// Loaded once per dequeued message (see `process_trace`), not once per
    /// worker: a worker outlives many generations.
    pub(super) runtime: Arc<crate::runtime::RuntimeHandle>,
    pub(super) trace_repo: Arc<dyn TraceSink>,
    pub(super) dlq_repo: Option<Arc<dyn TraceDlqRepository>>,
    pub(super) processing_timeout_ms: u64,
    pub(super) max_result_size_bytes: usize,
    pub(super) dlq_max_retries: i64,
    /// `Arc<str>` so the per-message context clone is a refcount bump.
    pub(super) rollout_sticky_header: std::sync::Arc<str>,
    pub(super) persistence_queue: crate::queue::TracePersistenceQueue,
    pub(super) global_trace_storage: crate::config::TraceStorageConfig,
}

/// Everything a DLQ row is built from, passed as a single borrow instead of
/// five positional arguments through `mark_running` → `handle_failure` →
/// `enqueue_dlq_row`. `payload`/`metadata` stay unserialized: only
/// [`enqueue_dlq_row`] ever needs them as JSON, and only on the failure path.
struct DlqCandidate<'a> {
    trace_id: &'a str,
    channel: &'a str,
    payload: &'a serde_json::Value,
    metadata: &'a serde_json::Value,
    retry_count: i64,
    dlq_entry_id: Option<&'a str>,
    dlq_max_retries: Option<i64>,
}

/// One trace's claim on the queue's capacity, released on drop.
///
/// Q6: the dispatcher used to increment the two counters, spawn the task, and
/// decrement them after `process_trace(...).await` returned. A panic in that
/// future skipped both decrements — the permit came back (it was already RAII)
/// but the accounting did not. `memory_bytes` is the admission authority in
/// [`super::TraceQueue::enqueue`], so every panic permanently shrank the
/// queue's capacity: enough of them and healthy submissions answer 503 with a
/// counter describing work that finished long ago, until the process is
/// restarted. `active` only fed a gauge, but the gauge is how an operator
/// decides whether the pool is stuck.
///
/// A guard rather than tidier decrements: this is the same fix
/// [`super::bounded::Leased`] already applies to the depth counter, for the
/// same reason ("a worker that panics mid-write cannot strand the
/// reservation"), and it is the only shape that holds for a failure mode
/// nobody remembers to write code for.
struct ActiveTrace {
    active: Arc<AtomicUsize>,
    memory_bytes: Arc<AtomicUsize>,
    /// Exactly what `enqueue` reserved for this item.
    reserved: usize,
    /// Named in the panic log below: a `JoinError` from a detached task says
    /// only *that* something panicked, and the dispatcher would have to keep a
    /// side table to say *which* trace. The guard already knows.
    trace_id: String,
    /// Declared last so it is dropped last — after this type's `Drop` body has
    /// released the counters. Shutdown waits on the permits, so returning one
    /// while the accounting it was holding is still outstanding would let a
    /// drain finish against counters that have not settled.
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl Drop for ActiveTrace {
    fn drop(&mut self) {
        let active = super::bounded::release_counter(&self.active, 1);
        metrics::set_trace_workers_active(active as f64);
        let memory = super::bounded::release_counter(&self.memory_bytes, self.reserved);
        metrics::set_trace_queue_memory_bytes(memory as f64);

        // The panic itself still unwinds — this only makes it attributable.
        // Without it a panicking trace is a bare stderr backtrace with no
        // trace id, no channel, and nothing an alert can key on.
        if std::thread::panicking() {
            metrics::record_error("trace_worker_panic");
            tracing::error!(
                trace_id = %self.trace_id,
                "Trace worker panicked; its queue capacity has been released. \
                 The trace itself is lost — no result and no DLQ row — because \
                 the panic unwound past both."
            );
        }
    }
}

/// Main dispatcher loop: receives traces from the channel and spawns processing
/// tasks, limited by a semaphore to `max_workers` concurrent traces.
pub(super) async fn dispatcher_loop(
    mut rx: super::bounded::WorkerReceiver<QueuedItem>,
    ctx: DispatcherContext,
) {
    let semaphore = Arc::new(Semaphore::new(ctx.max_workers));

    // `recv` releases the item's depth reservation as it hands it over — the
    // item is no longer queued, it is running. `orion_trace_workers_active`
    // below is what covers it from here.
    while let Some(item) = rx.recv().await {
        // Acquire a permit — blocks if all workers are busy
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break, // Semaphore closed
        };

        // Dequeued — now active.
        let active = ctx.counters.active.fetch_add(1, Ordering::Relaxed) + 1;
        metrics::set_trace_workers_active(active as f64);

        // Everything this item holds — the permit, its slot in the active
        // count, and the bytes `enqueue` reserved for it — in one value that
        // releases them however the task ends.
        let claim = ActiveTrace {
            active: ctx.counters.active.clone(),
            memory_bytes: ctx.counters.memory_bytes.clone(),
            reserved: item.payload_size,
            trace_id: item.msg.trace_id.clone(),
            _permit: permit,
        };
        let processing = ctx.processing.clone();

        tokio::spawn(async move {
            let _claim = claim;
            run_supervised_trace(item, processing).await;
        });
    }

    // Wait for all in-flight traces to complete, with a timeout
    if tokio::time::timeout(
        Duration::from_secs(ctx.shutdown_timeout_secs),
        semaphore.acquire_many(ctx.max_workers as u32),
    )
    .await
    .is_err()
    {
        tracing::warn!("Timed out waiting for in-flight traces to complete");
    }
    tracing::info!("Trace queue workers shut down");
}

impl ProcessingContext {
    /// Mode-aware trace status write. Sync mode writes inline (logging an
    /// error if the DB call fails); async and batch modes enqueue to the
    /// persistence queue; off mode is a no-op.
    async fn set_trace_status(
        &self,
        mode: crate::config::TraceStorageMode,
        trace_id: &str,
        status: &str,
        message: Option<&str>,
    ) {
        match mode {
            TraceStorageMode::Sync => {
                if let Err(e) = self
                    .trace_repo
                    .update_status(trace_id, status, message)
                    .await
                {
                    tracing::error!(trace_id = %trace_id, error = %e, "Failed to update trace status to {}", status);
                }
            }
            TraceStorageMode::Async | TraceStorageMode::Batch => {
                self.persistence_queue
                    .submit(crate::queue::TracePersistenceTask::UpdateStatus {
                        id: trace_id.to_string(),
                        status: status.to_string(),
                        error_message: message.map(str::to_string),
                    })
                    .await;
            }
            TraceStorageMode::Off => {}
        }
    }
}

/// Mode-aware completion (result and `completed` status, one write) for the
/// non-sync modes. `Async`/`Batch` enqueue; `Off` skips. `Sync` never reaches
/// here — the caller writes it inline so it can keep the result and task
/// trace by value.
async fn route_complete(
    mode: crate::config::TraceStorageMode,
    persistence_queue: &crate::queue::TracePersistenceQueue,
    trace_id: &str,
    result_json: String,
    duration_ms: f64,
    task_trace_json: Option<String>,
) {
    match mode {
        TraceStorageMode::Async | TraceStorageMode::Batch => {
            persistence_queue
                .submit(crate::queue::TracePersistenceTask::Complete(
                    crate::storage::repositories::traces::TraceResultRow {
                        id: trace_id.to_string(),
                        result_json,
                        duration_ms,
                        task_trace_json,
                    },
                ))
                .await;
        }
        TraceStorageMode::Sync | TraceStorageMode::Off => {}
    }
}

/// Supervise one queued trace so a panicking handler or task still records a
/// terminal trace status (`FAILED`) and settles any leased DLQ retry entry
/// as exhausted rather than stranding the trace at `pending`/`running` or
/// automatically replaying after uncertain side effects (`TD-06`).
async fn run_supervised_trace(item: QueuedItem, ctx: ProcessingContext) {
    let trace_id = item.msg.trace_id.clone();
    let channel = item.msg.channel.clone();
    let dlq_entry_id = item.dlq_entry_id.clone();
    let ctx_for_panic = ctx.clone();

    if let Err(join_err) = tokio::spawn(process_trace(item, ctx)).await
        && join_err.is_panic()
    {
        settle_panicked_trace(&ctx_for_panic, &trace_id, &channel, dlq_entry_id.as_deref()).await;
    }
}

/// Settle a trace whose worker task panicked during execution (`TD-06`).
async fn settle_panicked_trace(
    ctx: &ProcessingContext,
    trace_id: &str,
    channel: &str,
    dlq_entry_id: Option<&str>,
) {
    const PANIC_ERROR_MSG: &str = "Internal error: trace worker panicked during execution";

    metrics::record_error("trace_worker_panic");
    tracing::error!(
        trace_id = %trace_id,
        channel = %channel,
        "Trace worker panicked during execution; settling trace as failed without automatic replay"
    );

    let generation = ctx.runtime.load();
    let channel_runtime = generation.channels.get_by_name(channel);
    let effective_trace = channel_runtime
        .as_ref()
        .map(|c| c.trace_storage)
        .unwrap_or_else(|| {
            crate::channel::registry::EffectiveTraceConfig::resolve(&ctx.global_trace_storage, None)
        })
        .for_async_submission();

    if matches!(effective_trace.mode, TraceStorageMode::Sync) {
        if let Err(e) = ctx
            .trace_repo
            .update_status(trace_id, models::TRACE_STATUS_FAILED, Some(PANIC_ERROR_MSG))
            .await
        {
            tracing::error!(
                trace_id = %trace_id,
                error = %e,
                "Failed to settle panicked trace status to failed"
            );
        }
    } else {
        ctx.set_trace_status(
            effective_trace.mode,
            trace_id,
            models::TRACE_STATUS_FAILED,
            Some(PANIC_ERROR_MSG),
        )
        .await;
    }

    // Avoid automatic replay when external side effects are uncertain: if this
    // trace was a leased DLQ retry, mark the DLQ entry exhausted (`TD-06`).
    if let Some(dlq_id) = dlq_entry_id
        && let Some(ref dlq) = ctx.dlq_repo
        && let Err(e) = dlq.mark_exhausted(dlq_id).await
    {
        tracing::error!(
            dlq_id = %dlq_id,
            trace_id = %trace_id,
            error = %e,
            "Failed to mark DLQ entry exhausted after worker panic"
        );
    }
}

/// Process a single queued trace.
#[tracing::instrument(skip_all, fields(trace_id = %item.msg.trace_id, channel = %item.msg.channel))]
async fn process_trace(item: QueuedItem, ctx: ProcessingContext) {
    let QueuedItem {
        mut msg,
        dlq_retry_count,
        dlq_entry_id,
        dlq_max_retries,
        // Already released by the dispatcher once this task returns.
        payload_size: _,
    } = item;
    // Hold the channel's backpressure permit (acquired at submission) for
    // the duration of processing; released on return.
    let _backpressure_permit = msg.backpressure_permit.take();

    // Resolve effective trace-storage config for this channel (channel
    // override > global default).
    //
    // R11: everything reaching this worker came in through `/async`, which
    // hands the caller a `trace_id` to poll — so `for_async_submission`
    // upgrades `Off` to `Sync` here, matching the pending row the submission
    // path already wrote. Dropping the result while the row exists would leave
    // the trace stuck at `pending` forever.
    // F35 on the dequeue path: `require_serviceable`, like every other
    // ingress — `get_by_name` answers `None` for a quarantined channel,
    // indistinguishable from an unregistered name, and the message would
    // run against the engine with the channel's own timeout and trace
    // policy silently replaced by global defaults. The refusal is handled
    // below, once the DLQ candidate exists to fail into.
    //
    // One generation for this message: the channel's timeout and trace policy
    // below, and the engine that runs it, come from the same build. It is
    // deliberately loaded *here* rather than carried from the submission that
    // enqueued it — a config change between submission and dequeue applies, as
    // the timeout note below says.
    let generation = ctx.runtime.load();
    let (channel_runtime, quarantine_reason) =
        match generation.channels.require_serviceable(&msg.channel) {
            Ok(runtime) => (runtime, None),
            Err(e) => (None, Some(e.to_string())),
        };
    // O1: unregistered channel names (arbitrary path segments on the async
    // route) must not become Prometheus label values.
    let channel_registered = channel_runtime.is_some();
    // N16: the channel's own `timeout_ms` governs here too. This worker used
    // to apply `trace_queue.processing_timeout_ms` unconditionally, so a
    // channel declaring `timeout_ms = 2000` timed out at 2 s over HTTP and at
    // the global 60 s over `/async` — the same channel, two contracts.
    // Re-resolved here rather than carried through the queue, so a config
    // change between submission and dequeue applies.
    //
    // Clamped to `trace_queue.processing_timeout_ms`, which is an operator's
    // cap on how long one of a fixed number of queue workers may be occupied,
    // not a default a channel may raise: a channel declaring `timeout_ms`
    // above it would otherwise hold a worker past the ceiling and starve
    // every other channel's queued work.
    let timeout_ms = crate::channel::guards::effective_timeout_ms(
        &channel_runtime,
        Some(ctx.processing_timeout_ms),
        Some(ctx.processing_timeout_ms),
    )
    .unwrap_or(ctx.processing_timeout_ms);
    let effective_trace = channel_runtime
        .as_ref()
        .map(|c| c.trace_storage)
        .unwrap_or_else(|| {
            crate::channel::registry::EffectiveTraceConfig::resolve(&ctx.global_trace_storage, None)
        })
        .for_async_submission();
    let trace_mode = effective_trace.mode;
    // Restore W3C trace context from the originating request so this span
    // appears as a child in the caller's distributed trace.
    let _cx = crate::trace_context::set_parent_from_map(&msg.trace_headers);

    let trace_id = msg.trace_id;
    let channel = msg.channel;
    let metrics_channel = if channel_registered {
        channel.as_str()
    } else {
        "_unknown"
    };
    let profile = msg
        .profile_requested
        .then(crate::engine::profile::ProfileCollector::new);
    let start = Instant::now();

    // Everything a DLQ row needs, borrowed once instead of threaded
    // positionally through the failure paths. Payload and metadata stay as
    // `Value` here and are serialized only if a row is actually written.
    let dlq = DlqCandidate {
        trace_id: &trace_id,
        channel: &channel,
        payload: &msg.payload,
        metadata: &msg.metadata,
        retry_count: dlq_retry_count,
        dlq_entry_id: dlq_entry_id.as_deref(),
        dlq_max_retries,
    };

    // A quarantined channel is refused rather than executed. The trace
    // fails into the DLQ, so already-queued messages are replayable once
    // the operator fixes the channel's stored config — and the retry count
    // converges (Q3) instead of the DLQ retry loop spinning forever.
    if let Some(reason) = quarantine_reason {
        metrics::record_message(metrics_channel, "error");
        metrics::record_error("channel_quarantined");
        handle_failure(&ctx, trace_mode, &dlq, &reason).await;
        return;
    }

    // Mark as running. In sync mode this blocks; in async/batch it enqueues;
    // in off mode it's a no-op since no DB row exists.
    if !mark_running(&ctx, trace_mode, &dlq).await {
        return;
    }

    // A2: capture the per-task execution trace when the channel opted in via
    // `config.tracing.task_details = true`.
    let capture = effective_trace
        .task_details
        .then_some(crate::engine::TraceCapture {
            max_snapshot_bytes: ctx.max_result_size_bytes,
        });

    // The shared post-admission step: message build, engine snapshot, the
    // deadline arm and the `has_errors` rule. What stays here is persistence
    // and the DLQ routing.
    let execution = crate::runtime::execute_admitted_on_generation(
        &generation,
        &channel,
        &msg.payload,
        &msg.metadata,
        crate::engine::ExecOpts {
            timeout_ms: Some(timeout_ms),
            capture,
            routing_bucket: Some(crate::engine::utils::rollout_bucket_for_identity(
                crate::engine::utils::rollout_identity(&msg.metadata, &ctx.rollout_sticky_header),
            )),
            profile: profile.as_ref(),
        },
    )
    .await;
    if let Some(ref p) = profile {
        p.set_workflow_total(execution.duration);
    }
    let crate::engine::Execution {
        message,
        task_trace,
        outcome,
        duration: engine_duration,
    } = execution;

    let task_trace_json = crate::engine::utils::serialize_task_trace_capped(
        task_trace.as_ref(),
        ctx.max_result_size_bytes,
        &trace_id,
    );

    // The whole hop, including the status write and the persistence below —
    // what the trace row reports. `engine_duration` is the engine call alone,
    // which is what the latency histogram measures.
    let duration_ms = start.elapsed().as_secs_f64() * 1000.0;
    // A registered channel's own handles; an unregistered name has none, and
    // is counted under `_unknown` through the uncached path.
    match &channel_runtime {
        Some(runtime) => {
            runtime
                .metrics
                .record_message(metrics_channel, outcome.status_label());
            runtime
                .metrics
                .record_message_duration(metrics_channel, engine_duration.as_secs_f64());
        }
        None => {
            metrics::record_message(metrics_channel, outcome.status_label());
            metrics::record_message_duration(metrics_channel, engine_duration.as_secs_f64());
        }
    }

    match outcome {
        crate::engine::RunOutcome::Ok => {
            persist_success(
                &ctx,
                &effective_trace,
                &trace_id,
                &dlq,
                FinishedRun {
                    message: &message,
                    profile: profile.as_ref(),
                    duration_ms,
                    task_trace_json,
                },
            )
            .await;
        }
        crate::engine::RunOutcome::Timeout(ms) => {
            tracing::warn!(
                trace_id = %trace_id,
                channel = %channel,
                timeout_ms = ms,
                "Async trace processing timed out"
            );
            metrics::record_error("engine");
            handle_failure(
                &ctx,
                trace_mode,
                &dlq,
                &format!("Processing timed out after {ms}ms"),
            )
            .await;
        }
        // A workflow that failed its tasks routes to the DLQ exactly like an
        // engine failure: the async caller has no response to read the errors
        // out of, so the retry is the only way the work happens.
        crate::engine::RunOutcome::WorkflowErrors(summary) => {
            metrics::record_error("engine");
            handle_failure(&ctx, trace_mode, &dlq, &summary).await;
        }
        crate::engine::RunOutcome::EngineError(e) => {
            metrics::record_error("engine");
            handle_failure(&ctx, trace_mode, &dlq, &e.to_string()).await;
        }
    }
}

/// Mark the trace as running before the engine runs. The sync mode writes
/// inline; async/batch enqueue and off is a no-op, via the non-sync arms of
/// [`ProcessingContext::set_trace_status`]. Returns `false` when processing
/// must stop: a failed sync-mode write routes the message to the DLQ (Q5)
/// instead of dropping it, so the retry worker re-runs it once the DB recovers.
async fn mark_running(
    ctx: &ProcessingContext,
    trace_mode: TraceStorageMode,
    dlq: &DlqCandidate<'_>,
) -> bool {
    let trace_id = dlq.trace_id;
    if matches!(trace_mode, TraceStorageMode::Sync) {
        if let Err(e) = ctx
            .trace_repo
            .update_status(trace_id, models::TRACE_STATUS_RUNNING, None)
            .await
        {
            // Q5: a transient DB error here used to drop the message
            // entirely — trace stuck `pending` forever, work silently
            // undone. Route it through the DLQ instead so the retry
            // worker re-runs it once the DB recovers. Best-effort: the
            // enqueue writes to the same DB, but it happens later and
            // retries again from the DLQ poll loop.
            tracing::error!(
                trace_id = %trace_id,
                error = %e,
                "Failed to update trace status to running — routing to DLQ"
            );
            metrics::record_error("trace_status_write");
            settle_dlq_on_failure(
                &ctx.dlq_repo,
                dlq,
                &format!("Failed to mark trace running: {e}"),
                dlq.dlq_max_retries.unwrap_or(ctx.dlq_max_retries),
            )
            .await;
            let _ = ctx
                .trace_repo
                .update_status(
                    trace_id,
                    models::TRACE_STATUS_FAILED,
                    Some("Could not start processing; routed to DLQ"),
                )
                .await;
            return false;
        }
    } else {
        ctx.set_trace_status(trace_mode, trace_id, models::TRACE_STATUS_RUNNING, None)
            .await;
    }
    true
}

/// What a successful run left to persist: the finished message, its profile
/// when one was requested, how long it took, and its task trace.
struct FinishedRun<'a> {
    message: &'a dataflow_rs::Message,
    profile: Option<&'a Arc<crate::engine::profile::ProfileCollector>>,
    duration_ms: f64,
    task_trace_json: Option<String>,
}

/// The success arm of [`process_trace`]: serialize the finished message
/// (embedding the profile when requested), enforce the result size limit,
/// route the result write through the configured persistence mode (with
/// inline retries in sync mode), set the final trace status, and settle any
/// leased DLQ retry entry (`TD-05`).
async fn persist_success(
    ctx: &ProcessingContext,
    effective_trace: &crate::channel::registry::EffectiveTraceConfig,
    trace_id: &str,
    dlq: &DlqCandidate<'_>,
    run: FinishedRun<'_>,
) {
    let FinishedRun {
        message,
        profile,
        duration_ms,
        task_trace_json,
    } = run;
    let trace_mode = effective_trace.mode;

    let result_json = match serialize_result_with_profile(message, profile) {
        Ok(json) => json,
        Err(e) => {
            let err_msg = format!("Result serialization failed: {e}");
            tracing::error!(trace_id = %trace_id, error = %e, "Failed to serialize trace result");
            ctx.set_trace_status(
                trace_mode,
                trace_id,
                models::TRACE_STATUS_FAILED,
                Some(&err_msg),
            )
            .await;
            settle_dlq_on_failure(
                &ctx.dlq_repo,
                dlq,
                &err_msg,
                dlq.dlq_max_retries.unwrap_or(ctx.dlq_max_retries),
            )
            .await;
            return;
        }
    };

    // Enforce result size limit
    if ctx.max_result_size_bytes > 0 && result_json.len() > ctx.max_result_size_bytes {
        let err_msg = format!(
            "Result size {} bytes exceeds limit of {} bytes",
            result_json.len(),
            ctx.max_result_size_bytes
        );
        tracing::warn!(
            trace_id = %trace_id,
            result_bytes = result_json.len(),
            limit_bytes = ctx.max_result_size_bytes,
            "Trace result exceeds size limit"
        );
        metrics::record_error("result_size_exceeded");
        ctx.set_trace_status(
            trace_mode,
            trace_id,
            models::TRACE_STATUS_FAILED,
            Some(&err_msg),
        )
        .await;
        settle_dlq_on_failure(
            &ctx.dlq_repo,
            dlq,
            &err_msg,
            dlq.dlq_max_retries.unwrap_or(ctx.dlq_max_retries),
        )
        .await;
        return;
    }

    // Through the shared `TracePlan::decide`, which owns the drop decision and
    // the `orion_traces_dropped_total` reason label for every transport — the
    // async path spelled the same match out for itself, so a new drop reason
    // would have reached two transports of three.
    // This branch handles the success path → no errors. The sampling draw is
    // deterministic here: `for_async_submission` pins `sample_rate` to 1.0
    // (N22), so only `errors_only` can drop an async result — a sampled-out
    // trace with a live status row cannot happen on this path.
    let should_persist_result =
        super::trace_record::TracePlan::decide(effective_trace, false).persists();

    // The result and the `completed` status are one write (`complete`), so
    // the trace never shows `completed` without its result. A trace whose
    // result is not persisted still gets its status.
    let result_saved = if !should_persist_result {
        ctx.set_trace_status(trace_mode, trace_id, models::TRACE_STATUS_COMPLETED, None)
            .await;
        true
    } else if matches!(trace_mode, TraceStorageMode::Sync) {
        // Sync mode: write inline, under the same bounded backoff every other
        // persistence write uses (Q6), so the retry policy lives in one place.
        let completion = crate::storage::repositories::traces::TraceCompletion {
            result_json: &result_json,
            duration_ms,
            task_trace_json: task_trace_json.as_deref(),
            status: models::TRACE_STATUS_COMPLETED,
            error_message: None,
        };
        match crate::queue::trace_persistence::with_write_retries(|| async {
            match ctx.trace_repo.complete(trace_id, completion).await {
                // No row to complete is not a transient failure — retrying
                // cannot make one appear — and the run itself succeeded, so
                // it is reported and the run settles as a success, as when
                // the status write was the one to miss.
                Err(crate::errors::OrionError::Http {
                    status: orion_common::errors::StatusCode::NOT_FOUND,
                    message: missing,
                    ..
                }) => Ok(Some(missing)),
                other => other.map(|()| None),
            }
        })
        .await
        {
            Ok(None) => true,
            Ok(Some(missing)) => {
                tracing::error!(
                    trace_id = %trace_id,
                    error = %missing,
                    "Failed to update trace status to {}",
                    models::TRACE_STATUS_COMPLETED
                );
                true
            }
            Err(e) => {
                // The helper retries at debug level, so this is the only place
                // the database error itself is reported — without it a failed
                // result write shows up as a FAILED trace with no cause.
                tracing::warn!(
                    trace_id = %trace_id,
                    error = %e,
                    "Failed to save trace result, giving up after the bounded retries"
                );
                false
            }
        }
    } else {
        // Async / batch / off: the queue accepted (or off mode skipped).
        route_complete(
            trace_mode,
            &ctx.persistence_queue,
            trace_id,
            result_json,
            duration_ms,
            task_trace_json,
        )
        .await;
        true
    };

    if result_saved {
        settle_dlq_on_success(&ctx.dlq_repo, dlq.dlq_entry_id).await;
    } else {
        let err_msg = "Result persistence failed after retries";
        tracing::error!(trace_id = %trace_id, "Failed to save trace result after 3 attempts, marking as failed");
        ctx.set_trace_status(
            trace_mode,
            trace_id,
            models::TRACE_STATUS_FAILED,
            Some(err_msg),
        )
        .await;
        settle_dlq_on_failure(
            &ctx.dlq_repo,
            dlq,
            err_msg,
            dlq.dlq_max_retries.unwrap_or(ctx.dlq_max_retries),
        )
        .await;
    }
}

/// Settle a leased DLQ entry after its retry execution succeeded (`TD-05`).
pub(crate) async fn settle_dlq_on_success(
    dlq_repo: &Option<Arc<dyn TraceDlqRepository>>,
    dlq_entry_id: Option<&str>,
) {
    let (Some(dlq), Some(dlq_id)) = (dlq_repo, dlq_entry_id) else {
        return;
    };
    if let Err(e) = dlq.remove(dlq_id).await {
        tracing::error!(
            dlq_id = %dlq_id,
            error = %e,
            "DLQ retry: failed to remove leased entry after successful execution"
        );
    } else {
        tracing::info!(
            dlq_id = %dlq_id,
            "DLQ retry: removed leased entry after successful execution"
        );
    }
}

/// Settle a failed trace against the DLQ (`TD-05`).
///
/// - When `candidate.dlq_entry_id` is `Some(dlq_id)`, the failure came from a
///   leased DLQ retry: we update the existing row in place (`mark_exhausted`
///   when `retry_count >= dlq_max_retries`, or `record_retry` to release the
///   lease and advance `retry_count`).
/// - When `candidate.dlq_entry_id` is `None`, this is an initial async trace
///   failure and a new DLQ row is enqueued via [`enqueue_dlq_row`].
async fn settle_dlq_on_failure(
    dlq_repo: &Option<Arc<dyn TraceDlqRepository>>,
    candidate: &DlqCandidate<'_>,
    error_str: &str,
    dlq_max_retries: i64,
) {
    if let Some(dlq_id) = candidate.dlq_entry_id {
        let Some(dlq) = dlq_repo else { return };
        if candidate.retry_count >= dlq_max_retries {
            metrics::record_error("dlq_exhausted");
            metrics::record_trace_dlq_retry("exhausted");
            if let Err(e) = dlq.mark_exhausted(dlq_id).await {
                tracing::error!(
                    dlq_id = %dlq_id,
                    trace_id = %candidate.trace_id,
                    error = %e,
                    "DLQ retry: failed to mark leased entry exhausted after execution failure"
                );
            } else {
                tracing::warn!(
                    dlq_id = %dlq_id,
                    trace_id = %candidate.trace_id,
                    retry_count = candidate.retry_count,
                    max_retries = dlq_max_retries,
                    "DLQ retry: max retries exhausted after execution failure"
                );
            }
        } else {
            let next_retry = chrono::Utc::now().naive_utc();
            if let Err(e) = dlq.record_retry(dlq_id, next_retry).await {
                tracing::error!(
                    dlq_id = %dlq_id,
                    trace_id = %candidate.trace_id,
                    error = %e,
                    "DLQ retry: failed to record retry on leased entry after execution failure"
                );
            } else {
                tracing::info!(
                    dlq_id = %dlq_id,
                    trace_id = %candidate.trace_id,
                    retry_count = candidate.retry_count,
                    "DLQ retry: released lease and recorded retry after execution failure"
                );
            }
        }
    } else {
        enqueue_dlq_row(dlq_repo, candidate, error_str, dlq_max_retries).await;
    }
}

/// The failure arm of [`process_trace`]: mark the trace failed through the
/// configured persistence mode and settle or enqueue the message in the DLQ.
async fn handle_failure(
    ctx: &ProcessingContext,
    trace_mode: TraceStorageMode,
    dlq: &DlqCandidate<'_>,
    error_str: &str,
) {
    ctx.set_trace_status(
        trace_mode,
        dlq.trace_id,
        models::TRACE_STATUS_FAILED,
        Some(error_str),
    )
    .await;

    settle_dlq_on_failure(
        &ctx.dlq_repo,
        dlq,
        error_str,
        dlq.dlq_max_retries.unwrap_or(ctx.dlq_max_retries),
    )
    .await;
}

/// Enqueue a failed message into the trace DLQ. Shared by the engine-error
/// arm and the Q5 early-failure path (status write failed before the engine
/// ran). A row born at `retry_count >= max_retries` is exhausted by the same
/// predicate `mark_exhausted` writes — invisible to `claim_pending`, still
/// visible to operators.
async fn enqueue_dlq_row(
    dlq_repo: &Option<Arc<dyn TraceDlqRepository>>,
    candidate: &DlqCandidate<'_>,
    error_str: &str,
    dlq_max_retries: i64,
) {
    let Some(dlq) = dlq_repo else { return };
    // Serialized only here: on the success path — and when no DLQ is
    // configured — nothing ever reads these.
    let Ok(payload) = serde_json::to_string(candidate.payload) else {
        return;
    };
    let metadata = serde_json::to_string(candidate.metadata).ok();
    let metadata = metadata.as_deref().unwrap_or("{}");
    let trace_id = candidate.trace_id;
    let dlq_retry_count = candidate.retry_count;
    let exhausted = dlq_retry_count >= dlq_max_retries;
    if let Err(dlq_err) = dlq
        .enqueue(crate::storage::repositories::trace_dlq::DlqEnqueue {
            trace_id,
            channel: candidate.channel,
            payload_json: &payload,
            metadata_json: metadata,
            error_message: error_str,
            retry_count: dlq_retry_count,
            max_retries: dlq_max_retries,
        })
        .await
    {
        tracing::error!(
            trace_id = %trace_id,
            error = %dlq_err,
            "Failed to enqueue failed trace to DLQ"
        );
    } else if exhausted {
        metrics::record_trace_dlq_retry("exhausted");
        tracing::warn!(
            trace_id = %trace_id,
            retry_count = dlq_retry_count,
            max_retries = dlq_max_retries,
            "Failed trace exhausted its DLQ retries, no further attempts"
        );
    } else {
        tracing::info!(
            trace_id = %trace_id,
            retry_count = dlq_retry_count,
            "Failed trace enqueued to DLQ for retry"
        );
    }
}

#[cfg(test)]
mod tests {
    // The panic below is the subject of the test, not an accident.
    #![expect(clippy::panic)]

    use super::*;

    fn claim(
        active: &Arc<AtomicUsize>,
        memory: &Arc<AtomicUsize>,
        reserved: usize,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> ActiveTrace {
        // What the dispatcher does before handing the item to a task.
        active.fetch_add(1, Ordering::Relaxed);
        memory.fetch_add(reserved, Ordering::Relaxed);
        ActiveTrace {
            active: active.clone(),
            memory_bytes: memory.clone(),
            reserved,
            trace_id: "t-panic".to_string(),
            _permit: permit,
        }
    }

    /// Q6: a trace whose processing panics must give back everything it was
    /// holding.
    ///
    /// `memory_bytes` is what makes this more than a cosmetic gauge leak: it is
    /// the value `TraceQueue::enqueue` compares against `max_memory_bytes`, so
    /// bytes stranded by a panic are capacity the node never gets back. Enough
    /// panics and every submission answers 503 while the queue is empty.
    #[tokio::test]
    async fn a_panicking_trace_releases_its_capacity() {
        let active = Arc::new(AtomicUsize::new(0));
        let memory = Arc::new(AtomicUsize::new(0));
        let semaphore = Arc::new(Semaphore::new(1));
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("a free permit");

        let guard = claim(&active, &memory, 4096, permit);
        assert_eq!(memory.load(Ordering::Relaxed), 4096, "reserved up front");

        let panicked = tokio::spawn(async move {
            let _claim = guard;
            panic!("a handler panicked mid-trace");
        })
        .await;

        assert!(panicked.is_err(), "the task must actually have panicked");
        assert_eq!(active.load(Ordering::Relaxed), 0, "worker slot leaked");
        assert_eq!(memory.load(Ordering::Relaxed), 0, "reserved bytes leaked");
        assert_eq!(
            semaphore.available_permits(),
            1,
            "the concurrency permit must come back too"
        );
    }

    /// The ordinary path releases exactly the same things — the guard is not a
    /// panic-only mechanism bolted beside a normal one.
    #[tokio::test]
    async fn a_completed_trace_releases_the_same_capacity() {
        let active = Arc::new(AtomicUsize::new(0));
        let memory = Arc::new(AtomicUsize::new(0));
        let semaphore = Arc::new(Semaphore::new(1));
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("a free permit");

        let guard = claim(&active, &memory, 4096, permit);
        tokio::spawn(async move {
            let _claim = guard;
        })
        .await
        .expect("no panic");

        assert_eq!(active.load(Ordering::Relaxed), 0);
        assert_eq!(memory.load(Ordering::Relaxed), 0);
        assert_eq!(semaphore.available_permits(), 1);
    }

    /// Two traces' claims are independent: releasing one must not release the
    /// other's bytes, which a shared "reset to zero" fix would.
    #[tokio::test]
    async fn one_panicking_trace_does_not_disturb_its_neighbour() {
        let active = Arc::new(AtomicUsize::new(0));
        let memory = Arc::new(AtomicUsize::new(0));
        let semaphore = Arc::new(Semaphore::new(2));

        let doomed = claim(
            &active,
            &memory,
            100,
            semaphore.clone().acquire_owned().await.expect("permit"),
        );
        let survivor = claim(
            &active,
            &memory,
            900,
            semaphore.clone().acquire_owned().await.expect("permit"),
        );

        let _ = tokio::spawn(async move {
            let _claim = doomed;
            panic!("boom");
        })
        .await;

        assert_eq!(active.load(Ordering::Relaxed), 1, "the survivor is active");
        assert_eq!(
            memory.load(Ordering::Relaxed),
            900,
            "only the panicking trace's reservation is released"
        );
        drop(survivor);
        assert_eq!(memory.load(Ordering::Relaxed), 0);
    }

    #[derive(Default)]
    struct TestTraceSink {
        updates: std::sync::Mutex<Vec<(String, String, Option<String>)>>,
    }

    #[async_trait::async_trait]
    impl TraceSink for TestTraceSink {
        async fn create_pending(
            &self,
            _channel: &str,
            _channel_id: Option<&str>,
            _mode: &str,
            _input_json: Option<&str>,
            _access_token_hash: Option<&str>,
        ) -> Result<models::Trace, crate::errors::OrionError> {
            unimplemented!()
        }

        async fn update_status(
            &self,
            id: &str,
            status: &str,
            error_message: Option<&str>,
        ) -> Result<(), crate::errors::OrionError> {
            self.updates.lock().expect("test").push((
                id.to_string(),
                status.to_string(),
                error_message.map(str::to_string),
            ));
            Ok(())
        }

        async fn complete(
            &self,
            _id: &str,
            _completion: crate::storage::repositories::traces::TraceCompletion<'_>,
        ) -> Result<(), crate::errors::OrionError> {
            unimplemented!()
        }

        async fn store_completed(
            &self,
            _row: crate::storage::repositories::traces::TraceCompletedRef<'_>,
        ) -> Result<String, crate::errors::OrionError> {
            unimplemented!()
        }
    }

    #[derive(Default)]
    struct TestDlqRepo {
        removed: std::sync::Mutex<Vec<String>>,
        exhausted: std::sync::Mutex<Vec<String>>,
        retried: std::sync::Mutex<Vec<String>>,
        enqueued: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl TraceDlqRepository for TestDlqRepo {
        async fn enqueue(
            &self,
            row: crate::storage::repositories::trace_dlq::DlqEnqueue<'_>,
        ) -> Result<models::TraceDlqEntry, crate::errors::OrionError> {
            self.enqueued
                .lock()
                .expect("test")
                .push(row.trace_id.to_string());
            let now = chrono::Utc::now().naive_utc();
            Ok(models::TraceDlqEntry {
                id: "dlq-new".to_string(),
                trace_id: row.trace_id.to_string(),
                channel: row.channel.to_string(),
                payload_json: row.payload_json.to_string(),
                metadata_json: row.metadata_json.to_string(),
                error_message: row.error_message.to_string(),
                retry_count: row.retry_count,
                max_retries: row.max_retries,
                next_retry_at: now,
                created_at: now,
                updated_at: now,
            })
        }

        async fn claim_pending(
            &self,
            _claimant: &str,
            _limit: i64,
            _lease_secs: u64,
        ) -> Result<Vec<models::TraceDlqEntry>, crate::errors::OrionError> {
            unimplemented!()
        }

        async fn record_retry(
            &self,
            id: &str,
            _next_retry_at: chrono::NaiveDateTime,
        ) -> Result<(), crate::errors::OrionError> {
            self.retried.lock().expect("test").push(id.to_string());
            Ok(())
        }

        async fn remove(&self, id: &str) -> Result<(), crate::errors::OrionError> {
            self.removed.lock().expect("test").push(id.to_string());
            Ok(())
        }

        async fn mark_exhausted(&self, id: &str) -> Result<(), crate::errors::OrionError> {
            self.exhausted.lock().expect("test").push(id.to_string());
            Ok(())
        }

        async fn list_paginated(
            &self,
            _filter: &crate::storage::repositories::trace_dlq::TraceDlqFilter,
        ) -> Result<
            crate::storage::repositories::helpers::PaginatedResult<models::TraceDlqSummary>,
            crate::errors::OrionError,
        > {
            unimplemented!()
        }

        async fn count(
            &self,
            _filter: &crate::storage::repositories::trace_dlq::TraceDlqFilter,
        ) -> Result<i64, crate::errors::OrionError> {
            unimplemented!()
        }

        async fn get_by_id(
            &self,
            _id: &str,
        ) -> Result<models::TraceDlqEntry, crate::errors::OrionError> {
            unimplemented!()
        }

        async fn requeue(
            &self,
            _id: &str,
        ) -> Result<models::TraceDlqEntry, crate::errors::OrionError> {
            unimplemented!()
        }

        async fn purge_exhausted(
            &self,
            _older_than_hours: u64,
        ) -> Result<u64, crate::errors::OrionError> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn panicking_trace_records_terminal_failed_status_and_exhausts_leased_dlq_entry() {
        let trace_sink = Arc::new(TestTraceSink::default());
        let dlq_repo = Arc::new(TestDlqRepo::default());
        let dlq_trait: Option<Arc<dyn TraceDlqRepository>> = Some(dlq_repo.clone());
        let ctx = ProcessingContext {
            runtime: crate::runtime::generation::test_handle(),
            trace_repo: trace_sink.clone(),
            dlq_repo: dlq_trait,
            processing_timeout_ms: 1000,
            max_result_size_bytes: 1024 * 1024,
            dlq_max_retries: 3,
            rollout_sticky_header: Arc::from("x-request-id"),
            persistence_queue: crate::queue::TracePersistenceQueue::disabled(),
            global_trace_storage: crate::config::TraceStorageConfig::default(),
        };

        settle_panicked_trace(&ctx, "trace-panicked-1", "orders", Some("dlq-leased-1")).await;

        let updates = trace_sink.updates.lock().expect("test");
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0, "trace-panicked-1");
        assert_eq!(updates[0].1, models::TRACE_STATUS_FAILED);
        assert!(
            updates[0]
                .2
                .as_deref()
                .is_some_and(|m| m.contains("panicked")),
            "expected panic failure message, got {:?}",
            updates[0].2
        );

        let exhausted = dlq_repo.exhausted.lock().expect("test");
        assert_eq!(*exhausted, vec!["dlq-leased-1".to_string()]);
        assert!(dlq_repo.enqueued.lock().expect("test").is_empty());
    }

    #[tokio::test]
    async fn settle_dlq_on_failure_updates_leased_entry_or_exhausts_at_ceiling() {
        let dlq_repo = Arc::new(TestDlqRepo::default());
        let dlq_trait: Option<Arc<dyn TraceDlqRepository>> = Some(dlq_repo.clone());

        // Attempt 1 of 3 on a leased DLQ row -> record_retry in place (no duplicate enqueue)
        let candidate_retry = DlqCandidate {
            trace_id: "t-retry-1",
            channel: "orders",
            payload: &serde_json::json!({"a": 1}),
            metadata: &serde_json::json!({}),
            retry_count: 1,
            dlq_entry_id: Some("dlq-e1"),
            dlq_max_retries: Some(3),
        };
        settle_dlq_on_failure(&dlq_trait, &candidate_retry, "transient err", 3).await;
        assert_eq!(
            *dlq_repo.retried.lock().expect("test"),
            vec!["dlq-e1".to_string()]
        );
        assert!(dlq_repo.enqueued.lock().expect("test").is_empty());

        // Attempt 3 of 3 on a leased DLQ row -> mark_exhausted in place
        let candidate_exhausted = DlqCandidate {
            trace_id: "t-retry-3",
            channel: "orders",
            payload: &serde_json::json!({"a": 1}),
            metadata: &serde_json::json!({}),
            retry_count: 3,
            dlq_entry_id: Some("dlq-e1"),
            dlq_max_retries: Some(3),
        };
        settle_dlq_on_failure(&dlq_trait, &candidate_exhausted, "still failing", 3).await;
        assert_eq!(
            *dlq_repo.exhausted.lock().expect("test"),
            vec!["dlq-e1".to_string()]
        );
    }
}
