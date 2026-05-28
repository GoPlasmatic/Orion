use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{RwLock, Semaphore, mpsc};

use crate::metrics;
use crate::storage::models;
use crate::storage::repositories::trace_dlq::TraceDlqRepository;
use crate::storage::repositories::traces::TraceRepository;

use super::QueueMessage;

/// Serialize a finished message to JSON, embedding the per-request profile
/// JSON under `_orion.profile` when one is provided (B3 shape lock —
/// matches the sync response envelope).
fn serialize_result_with_profile(
    message: &dataflow_rs::Message,
    profile: Option<&Arc<crate::engine::profile::ProfileCollector>>,
) -> Result<String, serde_json::Error> {
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
    pub(super) channel_registry: Arc<crate::channel::ChannelRegistry>,
    pub(super) persistence_queue: crate::queue::TracePersistenceQueue,
    pub(super) global_trace_storage: crate::config::TracingStorageConfig,
}

/// Main dispatcher loop: receives traces from the channel and spawns processing
/// tasks, limited by a semaphore to `max_workers` concurrent traces.
pub(super) async fn dispatcher_loop(mut rx: mpsc::Receiver<QueueMessage>, ctx: DispatcherContext) {
    let semaphore = Arc::new(Semaphore::new(ctx.max_workers));

    while let Some(msg) = rx.recv().await {
        // Acquire a permit — blocks if all workers are busy
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break, // Semaphore closed
        };

        // Estimate payload size for memory accounting
        let estimated_size = msg.payload.to_string().len() + msg.metadata.to_string().len();

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
            process_trace(msg, processing).await;
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
        crate::config::TraceStorageMode::Sync => {
            set_trace_status(trace_repo, trace_id, status, message).await;
        }
        crate::config::TraceStorageMode::Async | crate::config::TraceStorageMode::Batch => {
            persistence_queue
                .submit(crate::queue::TracePersistenceTask::UpdateStatus {
                    id: trace_id.to_string(),
                    status: status.to_string(),
                    error_message: message.map(str::to_string),
                })
                .await;
        }
        crate::config::TraceStorageMode::Off => {}
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
        crate::config::TraceStorageMode::Sync => false, // caller handles inline write
        crate::config::TraceStorageMode::Async | crate::config::TraceStorageMode::Batch => {
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
        crate::config::TraceStorageMode::Off => true,
    }
}

/// Process a single queued trace.
#[tracing::instrument(skip_all, fields(trace_id = %msg.trace_id, channel = %msg.channel))]
async fn process_trace(msg: QueueMessage, ctx: ProcessingContext) {
    let ProcessingContext {
        engine,
        trace_repo,
        dlq_repo,
        processing_timeout_ms,
        max_result_size_bytes,
        channel_registry,
        persistence_queue,
        global_trace_storage,
    } = ctx;

    // Resolve effective trace-storage config for this channel (channel
    // override > global default). In `Off` mode skip all trace writes; the
    // workflow still runs but no DB rows are touched.
    let effective_trace = match channel_registry.get_by_name(&msg.channel).await {
        Some(c) => c.trace_storage,
        None => {
            crate::channel::registry::EffectiveTraceConfig::resolve(&global_trace_storage, None)
        }
    };
    let trace_mode = effective_trace.mode;
    // Restore W3C trace context from the originating request so this span
    // appears as a child in the caller's distributed trace.
    {
        use opentelemetry::propagation::TextMapPropagator;
        use opentelemetry_sdk::propagation::TraceContextPropagator;
        use tracing_opentelemetry::OpenTelemetrySpanExt;

        struct MapExtractor<'a>(&'a std::collections::HashMap<String, String>);
        impl opentelemetry::propagation::Extractor for MapExtractor<'_> {
            fn get(&self, key: &str) -> Option<&str> {
                self.0.get(key).map(|v| v.as_str())
            }
            fn keys(&self) -> Vec<&str> {
                self.0.keys().map(|k| k.as_str()).collect()
            }
        }

        let propagator = TraceContextPropagator::new();
        let cx = propagator.extract(&MapExtractor(&msg.trace_headers));
        let _ = tracing::Span::current().set_parent(cx);
    }

    let trace_id = msg.trace_id;
    let channel = msg.channel;
    let profile = msg
        .profile_requested
        .then(crate::engine::profile::ProfileCollector::new);
    let start = Instant::now();

    // Capture payload/metadata for potential DLQ enqueue before consuming
    let payload_json_for_dlq = serde_json::to_string(&msg.payload).ok();
    let metadata_json_for_dlq = serde_json::to_string(&msg.metadata).ok();

    // Mark as running. In sync mode this blocks; in async/batch it enqueues;
    // in off mode it's a no-op since no DB row exists.
    if matches!(trace_mode, crate::config::TraceStorageMode::Sync) {
        if let Err(e) = trace_repo
            .update_status(&trace_id, models::TRACE_STATUS_RUNNING, None)
            .await
        {
            tracing::error!(trace_id = %trace_id, error = %e, "Failed to update trace status to running");
            return;
        }
    } else if matches!(
        trace_mode,
        crate::config::TraceStorageMode::Async | crate::config::TraceStorageMode::Batch
    ) {
        persistence_queue
            .submit(crate::queue::TracePersistenceTask::UpdateStatus {
                id: trace_id.clone(),
                status: models::TRACE_STATUS_RUNNING.to_string(),
                error_message: None,
            })
            .await;
    }

    // Build message
    let mut message = dataflow_rs::Message::from_value(&msg.payload);
    crate::engine::utils::merge_metadata(&mut message, &msg.metadata);
    crate::engine::utils::inject_rollout_bucket(&mut message);

    // Clone the inner Arc<Engine> and release the lock immediately
    let engine_ref = crate::engine::acquire_engine_read(&engine).await;
    // A2: capture the per-task execution trace when the channel opted in via
    // `config.tracing.task_details = true`. Both arms resolve to the same
    // `(Result, Option<ExecutionTrace>)` shape so the timeout handling is shared.
    let capture_trace = effective_trace.task_details;
    let workflow_start = Instant::now();
    let engine_fut = async {
        tokio::time::timeout(Duration::from_millis(processing_timeout_ms), async {
            if capture_trace {
                match engine_ref
                    .process_message_for_channel_with_trace(&channel, &mut message)
                    .await
                {
                    Ok(trace) => (Ok(()), Some(trace)),
                    Err(e) => (Err(e), None),
                }
            } else {
                let r = engine_ref
                    .process_message_for_channel(&channel, &mut message)
                    .await;
                (r, None)
            }
        })
        .await
    };
    let timeout_outcome = if let Some(ref p) = profile {
        use crate::engine::profile::ORION_PROFILE;
        ORION_PROFILE.scope(p.clone(), engine_fut).await
    } else {
        engine_fut.await
    };
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
    let task_trace_json = task_trace
        .as_ref()
        .and_then(|t| serde_json::to_string(t).ok());

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
            metrics::record_message(&channel, "ok");
            metrics::record_message_duration(&channel, duration_secs);
            metrics::record_channel_execution(&channel);

            let result_json = match serialize_result_with_profile(&message, profile.as_ref()) {
                Ok(json) => json,
                Err(e) => {
                    tracing::error!(trace_id = %trace_id, error = %e, "Failed to serialize trace result");
                    route_set_trace_status(
                        trace_mode,
                        trace_repo.as_ref(),
                        &persistence_queue,
                        &trace_id,
                        models::TRACE_STATUS_FAILED,
                        Some(&format!("Result serialization failed: {e}")),
                    )
                    .await;
                    return;
                }
            };

            // Enforce result size limit
            if max_result_size_bytes > 0 && result_json.len() > max_result_size_bytes {
                tracing::warn!(
                    trace_id = %trace_id,
                    result_bytes = result_json.len(),
                    limit_bytes = max_result_size_bytes,
                    "Trace result exceeds size limit"
                );
                metrics::record_error("result_size_exceeded");
                route_set_trace_status(
                    trace_mode,
                    trace_repo.as_ref(),
                    &persistence_queue,
                    &trace_id,
                    models::TRACE_STATUS_FAILED,
                    Some(&format!(
                        "Result size {} bytes exceeds limit of {} bytes",
                        result_json.len(),
                        max_result_size_bytes
                    )),
                )
                .await;
                return;
            }

            // Apply filters via the shared `EffectiveTraceConfig::should_drop`.
            // This branch handles the success path → no errors.
            let should_persist_result = match effective_trace.should_drop(false) {
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
                &persistence_queue,
                &trace_id,
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
                    match trace_repo
                        .set_result(
                            &trace_id,
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
                    trace_repo.as_ref(),
                    &persistence_queue,
                    &trace_id,
                    models::TRACE_STATUS_COMPLETED,
                    None,
                )
                .await;
            } else {
                tracing::error!(trace_id = %trace_id, "Failed to save trace result after 3 attempts, marking as failed");
                route_set_trace_status(
                    trace_mode,
                    trace_repo.as_ref(),
                    &persistence_queue,
                    &trace_id,
                    models::TRACE_STATUS_FAILED,
                    Some("Result persistence failed after retries"),
                )
                .await;
            }
        }
        Err(e) => {
            metrics::record_message(&channel, "error");
            metrics::record_error("engine");

            let error_str = e.to_string();
            route_set_trace_status(
                trace_mode,
                trace_repo.as_ref(),
                &persistence_queue,
                &trace_id,
                models::TRACE_STATUS_FAILED,
                Some(&error_str),
            )
            .await;

            // Enqueue to DLQ for retry
            if let Some(ref dlq) = dlq_repo
                && let Some(ref payload) = payload_json_for_dlq
            {
                let metadata = metadata_json_for_dlq.as_deref().unwrap_or("{}");
                if let Err(dlq_err) = dlq
                    .enqueue(&trace_id, &channel, payload, metadata, &error_str, 5)
                    .await
                {
                    tracing::error!(
                        trace_id = %trace_id,
                        error = %dlq_err,
                        "Failed to enqueue failed trace to DLQ"
                    );
                } else {
                    tracing::info!(trace_id = %trace_id, "Failed trace enqueued to DLQ for retry");
                }
            }
        }
    }
}
