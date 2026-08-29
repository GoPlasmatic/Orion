use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{Semaphore, mpsc};

use crate::config::TraceStorageMode;
use crate::metrics;
use crate::storage::models;
use crate::storage::repositories::trace_dlq::TraceDlqRepository;
use crate::storage::repositories::traces::TraceRepository;

use super::QueuedItem;

/// Serialize a finished message to JSON, embedding the per-request profile
/// JSON under `_orion.profile` when one is provided (B3 shape lock —
/// matches the sync response envelope).
fn serialize_result_with_profile(
    message: &dataflow_rs::Message,
    profile: Option<&Arc<crate::engine::profile::ProfileCollector>>,
) -> Result<String, serde_json::Error> {
    let mut v = serde_json::to_value(message)?;
    if let Some(p) = profile
        && let Some(obj) = v.as_object_mut()
    {
        obj.insert(
            "_orion".to_string(),
            serde_json::json!({ "profile": p.to_json() }),
        );
    }
    serde_json::to_string(&v)
}

/// Shared counters for queue observability metrics.
pub(super) struct QueueCounters {
    pub(super) pending: Arc<AtomicUsize>,
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
    pub(super) engine: Arc<crate::engine::EngineHandle>,
    pub(super) trace_repo: Arc<dyn TraceRepository>,
    pub(super) dlq_repo: Option<Arc<dyn TraceDlqRepository>>,
    pub(super) processing_timeout_ms: u64,
    pub(super) max_result_size_bytes: usize,
    pub(super) dlq_max_retries: i64,
    /// `Arc<str>` so the per-message context clone is a refcount bump.
    pub(super) rollout_sticky_header: std::sync::Arc<str>,
    pub(super) channel_registry: Arc<crate::channel::ChannelRegistry>,
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
}

/// Main dispatcher loop: receives traces from the channel and spawns processing
/// tasks, limited by a semaphore to `max_workers` concurrent traces.
pub(super) async fn dispatcher_loop(mut rx: mpsc::Receiver<QueuedItem>, ctx: DispatcherContext) {
    let semaphore = Arc::new(Semaphore::new(ctx.max_workers));

    while let Some(item) = rx.recv().await {
        // Acquire a permit — blocks if all workers are busy
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break, // Semaphore closed
        };

        // Release exactly what `enqueue` reserved for this item
        let estimated_size = item.payload_size;

        // Dequeued — decrement pending, increment active
        let pending = ctx
            .counters
            .pending
            .fetch_sub(1, Ordering::Relaxed)
            .saturating_sub(1);
        metrics::set_trace_queue_depth(pending as f64);
        let active = ctx.counters.active.fetch_add(1, Ordering::Relaxed) + 1;
        metrics::set_trace_workers_active(active as f64);

        let processing = ctx.processing.clone();
        let active_counter = ctx.counters.active.clone();
        let memory_counter = ctx.counters.memory_bytes.clone();

        tokio::spawn(async move {
            let _permit = permit; // guard: dropped on scope exit, even on panic
            process_trace(item, processing).await;
            let active = active_counter
                .fetch_sub(1, Ordering::Relaxed)
                .saturating_sub(1);
            metrics::set_trace_workers_active(active as f64);
            // Release memory accounting
            let mem = memory_counter
                .fetch_sub(estimated_size, Ordering::Relaxed)
                .saturating_sub(estimated_size);
            metrics::set_trace_queue_memory_bytes(mem as f64);
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

/// Mode-aware result write for the non-sync modes. `Async`/`Batch` enqueue;
/// `Off` skips. `Sync` never reaches here — the caller writes it inline so it
/// can keep the result and task trace by value.
async fn route_set_result(
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
                .submit(crate::queue::TracePersistenceTask::SetResult(
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

/// Process a single queued trace.
#[tracing::instrument(skip_all, fields(trace_id = %item.msg.trace_id, channel = %item.msg.channel))]
async fn process_trace(item: QueuedItem, ctx: ProcessingContext) {
    let QueuedItem {
        mut msg,
        dlq_retry_count,
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
    let (channel_runtime, quarantine_reason) =
        match ctx.channel_registry.require_serviceable(&msg.channel) {
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
    let execution = crate::engine::execute_admitted(
        &ctx.engine,
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
    metrics::record_message(metrics_channel, outcome.status_label());
    metrics::record_message_duration(metrics_channel, engine_duration.as_secs_f64());

    match outcome {
        crate::engine::RunOutcome::Ok => {
            persist_success(
                &ctx,
                &effective_trace,
                &trace_id,
                &message,
                profile.as_ref(),
                duration_ms,
                task_trace_json,
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
            enqueue_dlq_row(
                &ctx.dlq_repo,
                dlq,
                &format!("Failed to mark trace running: {e}"),
                ctx.dlq_max_retries,
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

/// The success arm of [`process_trace`]: serialize the finished message
/// (embedding the profile when requested), enforce the result size limit,
/// route the result write through the configured persistence mode (with
/// inline retries in sync mode), and set the final trace status.
async fn persist_success(
    ctx: &ProcessingContext,
    effective_trace: &crate::channel::registry::EffectiveTraceConfig,
    trace_id: &str,
    message: &dataflow_rs::Message,
    profile: Option<&Arc<crate::engine::profile::ProfileCollector>>,
    duration_ms: f64,
    task_trace_json: Option<String>,
) {
    let trace_mode = effective_trace.mode;

    let result_json = match serialize_result_with_profile(message, profile) {
        Ok(json) => json,
        Err(e) => {
            tracing::error!(trace_id = %trace_id, error = %e, "Failed to serialize trace result");
            ctx.set_trace_status(
                trace_mode,
                trace_id,
                models::TRACE_STATUS_FAILED,
                Some(&format!("Result serialization failed: {e}")),
            )
            .await;
            return;
        }
    };

    // Enforce result size limit
    if ctx.max_result_size_bytes > 0 && result_json.len() > ctx.max_result_size_bytes {
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
            Some(&format!(
                "Result size {} bytes exceeds limit of {} bytes",
                result_json.len(),
                ctx.max_result_size_bytes
            )),
        )
        .await;
        return;
    }

    // Apply filters via the shared `EffectiveTraceConfig::should_drop`.
    // This branch handles the success path → no errors. The sampling draw is
    // deterministic here: `for_async_submission` pins `sample_rate` to 1.0
    // (N22), so only `errors_only` can drop an async result — a sampled-out
    // trace with a live status row cannot happen on this path.
    let should_persist_result =
        match effective_trace.should_drop(false, effective_trace.draw_sample()) {
            Some(reason) => {
                metrics::record_trace_dropped(reason);
                false
            }
            None => true,
        };

    let result_saved = if !should_persist_result {
        // Treat as saved for state-machine purposes — we won't write,
        // but we also don't want to mark FAILED.
        true
    } else if matches!(trace_mode, TraceStorageMode::Sync) {
        // Sync mode: write inline, under the same bounded backoff every other
        // persistence write uses (Q6), so the retry policy lives in one place.
        match crate::queue::trace_persistence::with_write_retries(|| async {
            ctx.trace_repo
                .set_result(
                    trace_id,
                    &result_json,
                    duration_ms,
                    task_trace_json.as_deref(),
                )
                .await
        })
        .await
        {
            Ok(_) => true,
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
        route_set_result(
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
        ctx.set_trace_status(trace_mode, trace_id, models::TRACE_STATUS_COMPLETED, None)
            .await;
    } else {
        tracing::error!(trace_id = %trace_id, "Failed to save trace result after 3 attempts, marking as failed");
        ctx.set_trace_status(
            trace_mode,
            trace_id,
            models::TRACE_STATUS_FAILED,
            Some("Result persistence failed after retries"),
        )
        .await;
    }
}

/// The failure arm of [`process_trace`]: mark the trace failed through the
/// configured persistence mode and enqueue the message to the DLQ for retry.
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

    // Enqueue to DLQ for retry. The new row starts at the retry count
    // this message's lineage already spent, so `dlq_max_retries`
    // converges instead of resetting on every failure (Q3).
    enqueue_dlq_row(&ctx.dlq_repo, dlq, error_str, ctx.dlq_max_retries).await;
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
