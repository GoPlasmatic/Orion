use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{RwLock, Semaphore, mpsc};

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
    // The rollout key can only be nulled, not deleted (see
    // engine::utils::remove_rollout_bucket), so strip it here or it is
    // persisted into traces.result_json as a field the caller never sent (F31).
    if let Some(data) = v
        .get_mut("context")
        .and_then(|c| c.get_mut("data"))
        .and_then(|d| d.as_object_mut())
    {
        data.remove("_rollout_bucket");
    }
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
    pub(super) engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
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

        // Estimate payload size for memory accounting
        let estimated_size =
            item.msg.payload.to_string().len() + item.msg.metadata.to_string().len();

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

/// Update trace status, logging an error if the DB call fails.
async fn set_trace_status(
    trace_repo: &dyn TraceRepository,
    trace_id: &str,
    status: &str,
    message: Option<&str>,
) {
    if let Err(e) = trace_repo.update_status(trace_id, status, message).await {
        tracing::error!(trace_id = %trace_id, error = %e, "Failed to update trace status to {}", status);
    }
}

/// Mode-aware variant of `set_trace_status`. Sync mode writes inline; async
/// and batch modes enqueue to the persistence queue; off mode is a no-op.
async fn route_set_trace_status(
    mode: crate::config::TraceStorageMode,
    trace_repo: &dyn TraceRepository,
    persistence_queue: &crate::queue::TracePersistenceQueue,
    trace_id: &str,
    status: &str,
    message: Option<&str>,
) {
    match mode {
        TraceStorageMode::Sync => {
            set_trace_status(trace_repo, trace_id, status, message).await;
        }
        TraceStorageMode::Async | TraceStorageMode::Batch => {
            persistence_queue
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

/// Mode-aware result write. `Sync` writes inline (with retries kept by the
/// caller); `Async`/`Batch` enqueue; `Off` skips.
async fn route_set_result(
    mode: crate::config::TraceStorageMode,
    persistence_queue: &crate::queue::TracePersistenceQueue,
    trace_id: &str,
    result_json: String,
    duration_ms: f64,
    task_trace_json: Option<String>,
) -> bool {
    match mode {
        TraceStorageMode::Sync => false, // caller handles inline write
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
            true
        }
        TraceStorageMode::Off => true,
    }
}

/// Process a single queued trace.
#[tracing::instrument(skip_all, fields(trace_id = %item.msg.trace_id, channel = %item.msg.channel))]
async fn process_trace(item: QueuedItem, ctx: ProcessingContext) {
    let QueuedItem {
        mut msg,
        dlq_retry_count,
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
    let channel_runtime = ctx.channel_registry.get_by_name(&msg.channel);
    // O1: unregistered channel names (arbitrary path segments on the async
    // route) must not become Prometheus label values.
    let channel_registered = channel_runtime.is_some();
    let effective_trace = channel_runtime
        .map(|c| c.trace_storage)
        .unwrap_or_else(|| {
            crate::channel::registry::EffectiveTraceConfig::resolve(&ctx.global_trace_storage, None)
        })
        .for_async_submission();
    let trace_mode = effective_trace.mode;
    // Restore W3C trace context from the originating request so this span
    // appears as a child in the caller's distributed trace.
    let _cx = crate::server::trace_context::set_parent_from_map(&msg.trace_headers);

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

    // Capture payload/metadata for potential DLQ enqueue before consuming
    let payload_json_for_dlq = serde_json::to_string(&msg.payload).ok();
    let metadata_json_for_dlq = serde_json::to_string(&msg.metadata).ok();

    // Mark as running. In sync mode this blocks; in async/batch it enqueues;
    // in off mode it's a no-op since no DB row exists.
    if !mark_running(
        &ctx,
        trace_mode,
        &trace_id,
        &channel,
        &payload_json_for_dlq,
        &metadata_json_for_dlq,
        dlq_retry_count,
    )
    .await
    {
        return;
    }

    // Build message
    let mut message = dataflow_rs::Message::from_value(&msg.payload);
    crate::engine::utils::merge_metadata(&mut message, &msg.metadata);
    let sticky_identity =
        crate::engine::utils::rollout_identity(&msg.metadata, &ctx.rollout_sticky_header);
    crate::engine::utils::inject_rollout_bucket(&mut message, sticky_identity);

    // Clone the inner Arc<Engine> and release the lock immediately
    let engine_ref = crate::engine::acquire_engine_read(&ctx.engine).await;
    // A2: capture the per-task execution trace when the channel opted in via
    // `config.tracing.task_details = true`. Both arms resolve to the same
    // `(Result, Option<ExecutionTrace>)` shape so the timeout handling is shared.
    let capture_trace = effective_trace.task_details;
    let processing_timeout_ms = ctx.processing_timeout_ms;
    let workflow_start = Instant::now();
    let timeout_outcome = crate::engine::run_for_channel(
        &engine_ref,
        &channel,
        &mut message,
        Some(processing_timeout_ms),
        profile.as_ref(),
        capture_trace,
    )
    .await;
    if let Some(ref p) = profile {
        p.set_workflow_total(workflow_start.elapsed());
    }
    let (result, task_trace) = match timeout_outcome {
        Ok(inner) => inner,
        Err(_) => {
            tracing::warn!(
                trace_id = %trace_id,
                channel = %channel,
                timeout_ms = processing_timeout_ms,
                "Async trace processing timed out"
            );
            (
                Err(dataflow_rs::DataflowError::Timeout(format!(
                    "Processing timed out after {processing_timeout_ms}ms"
                ))),
                None,
            )
        }
    };
    let task_trace_json = crate::engine::utils::serialize_task_trace_capped(
        task_trace.as_ref(),
        ctx.max_result_size_bytes,
        &trace_id,
    );

    crate::engine::utils::remove_rollout_bucket(&mut message);

    let duration = start.elapsed();
    let duration_secs = duration.as_secs_f64();
    let duration_ms = duration.as_secs_f64() * 1000.0;

    // v3 contract: process_message_for_channel returns Ok(()) even when
    // individual workflows fail — failures are pushed into message.errors().
    // Treat any captured error as a processing failure so it routes to the DLQ.
    let result = match result {
        Ok(()) if message.has_errors() => {
            let summary = message
                .errors()
                .iter()
                .map(|e| format!("{}: {}", e.code, e.message))
                .collect::<Vec<_>>()
                .join("; ");
            Err(dataflow_rs::DataflowError::Workflow(summary))
        }
        other => other,
    };

    match result {
        Ok(()) => {
            metrics::record_message(metrics_channel, "ok");
            metrics::record_message_duration(metrics_channel, duration_secs);

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
        Err(e) => {
            metrics::record_message(metrics_channel, "error");
            metrics::record_error("engine");

            handle_failure(
                &ctx,
                trace_mode,
                &trace_id,
                &channel,
                &e.to_string(),
                &payload_json_for_dlq,
                &metadata_json_for_dlq,
                dlq_retry_count,
            )
            .await;
        }
    }
}

/// Mark the trace as running before the engine runs. The sync mode writes
/// inline; async/batch enqueue and off is a no-op, via the non-sync arms of
/// [`route_set_trace_status`]. Returns `false` when processing must stop:
/// a failed sync-mode write routes the message to the DLQ (Q5) instead of
/// dropping it, so the retry worker re-runs it once the DB recovers.
async fn mark_running(
    ctx: &ProcessingContext,
    trace_mode: TraceStorageMode,
    trace_id: &str,
    channel: &str,
    payload_json_for_dlq: &Option<String>,
    metadata_json_for_dlq: &Option<String>,
    dlq_retry_count: i64,
) -> bool {
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
                payload_json_for_dlq,
                metadata_json_for_dlq,
                trace_id,
                channel,
                &format!("Failed to mark trace running: {e}"),
                dlq_retry_count,
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
        route_set_trace_status(
            trace_mode,
            ctx.trace_repo.as_ref(),
            &ctx.persistence_queue,
            trace_id,
            models::TRACE_STATUS_RUNNING,
            None,
        )
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
            route_set_trace_status(
                trace_mode,
                ctx.trace_repo.as_ref(),
                &ctx.persistence_queue,
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
        route_set_trace_status(
            trace_mode,
            ctx.trace_repo.as_ref(),
            &ctx.persistence_queue,
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
    } else if route_set_result(
        trace_mode,
        &ctx.persistence_queue,
        trace_id,
        result_json.clone(),
        duration_ms,
        task_trace_json.clone(),
    )
    .await
    {
        // Async / batch / off: the queue accepted (or off mode skipped).
        true
    } else {
        // Sync mode: retry inline with backoff.
        let mut ok = false;
        for attempt in 0..3 {
            match ctx
                .trace_repo
                .set_result(
                    trace_id,
                    &result_json,
                    duration_ms,
                    task_trace_json.as_deref(),
                )
                .await
            {
                Ok(_) => {
                    ok = true;
                    break;
                }
                Err(e) => {
                    tracing::warn!(
                        trace_id = %trace_id, error = %e, attempt = attempt + 1,
                        "Failed to save trace result, retrying"
                    );
                    tokio::time::sleep(Duration::from_millis(100 * (attempt + 1))).await;
                }
            }
        }
        ok
    };

    if result_saved {
        route_set_trace_status(
            trace_mode,
            ctx.trace_repo.as_ref(),
            &ctx.persistence_queue,
            trace_id,
            models::TRACE_STATUS_COMPLETED,
            None,
        )
        .await;
    } else {
        tracing::error!(trace_id = %trace_id, "Failed to save trace result after 3 attempts, marking as failed");
        route_set_trace_status(
            trace_mode,
            ctx.trace_repo.as_ref(),
            &ctx.persistence_queue,
            trace_id,
            models::TRACE_STATUS_FAILED,
            Some("Result persistence failed after retries"),
        )
        .await;
    }
}

/// The failure arm of [`process_trace`]: mark the trace failed through the
/// configured persistence mode and enqueue the message to the DLQ for retry.
#[allow(clippy::too_many_arguments)]
async fn handle_failure(
    ctx: &ProcessingContext,
    trace_mode: TraceStorageMode,
    trace_id: &str,
    channel: &str,
    error_str: &str,
    payload_json_for_dlq: &Option<String>,
    metadata_json_for_dlq: &Option<String>,
    dlq_retry_count: i64,
) {
    route_set_trace_status(
        trace_mode,
        ctx.trace_repo.as_ref(),
        &ctx.persistence_queue,
        trace_id,
        models::TRACE_STATUS_FAILED,
        Some(error_str),
    )
    .await;

    // Enqueue to DLQ for retry. The new row starts at the retry count
    // this message's lineage already spent, so `dlq_max_retries`
    // converges instead of resetting on every failure (Q3).
    enqueue_dlq_row(
        &ctx.dlq_repo,
        payload_json_for_dlq,
        metadata_json_for_dlq,
        trace_id,
        channel,
        error_str,
        dlq_retry_count,
        ctx.dlq_max_retries,
    )
    .await;
}

/// Enqueue a failed message into the trace DLQ. Shared by the engine-error
/// arm and the Q5 early-failure path (status write failed before the engine
/// ran). A row born at `retry_count >= max_retries` is exhausted by the same
/// predicate `mark_exhausted` writes — invisible to `claim_pending`, still
/// visible to operators.
#[allow(clippy::too_many_arguments)]
async fn enqueue_dlq_row(
    dlq_repo: &Option<Arc<dyn TraceDlqRepository>>,
    payload_json: &Option<String>,
    metadata_json: &Option<String>,
    trace_id: &str,
    channel: &str,
    error_str: &str,
    dlq_retry_count: i64,
    dlq_max_retries: i64,
) {
    let Some(dlq) = dlq_repo else { return };
    let Some(payload) = payload_json else { return };
    let metadata = metadata_json.as_deref().unwrap_or("{}");
    let exhausted = dlq_retry_count >= dlq_max_retries;
    if let Err(dlq_err) = dlq
        .enqueue(
            trace_id,
            channel,
            payload,
            metadata,
            error_str,
            dlq_retry_count,
            dlq_max_retries,
        )
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
