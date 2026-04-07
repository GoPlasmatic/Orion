use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{RwLock, Semaphore, mpsc};

use crate::metrics;
use crate::storage::models;
use crate::storage::repositories::trace_dlq::TraceDlqRepository;
use crate::storage::repositories::traces::TraceRepository;

/// Start a background task that periodically deletes old traces.
///
/// Returns a `JoinHandle` that can be aborted on shutdown.
/// If `retention_hours` is 0, no cleanup task is started.
pub fn start_trace_cleanup(
    retention_hours: u64,
    interval_secs: u64,
    trace_repo: Arc<dyn TraceRepository>,
) -> Option<tokio::task::JoinHandle<()>> {
    if retention_hours == 0 {
        tracing::info!("Trace retention disabled (trace_retention_hours = 0)");
        return None;
    }

    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
        // Skip the first immediate tick
        interval.tick().await;

        loop {
            interval.tick().await;
            match trace_repo.delete_older_than(retention_hours).await {
                Ok(count) => {
                    if count > 0 {
                        tracing::info!(
                            deleted = count,
                            retention_hours = retention_hours,
                            "Trace cleanup completed"
                        );
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Trace cleanup failed");
                }
            }
        }
    });

    tracing::info!(
        retention_hours = retention_hours,
        interval_secs = interval_secs,
        "Trace cleanup task started"
    );

    Some(handle)
}

/// A message submitted to the trace queue for async processing.
pub struct QueueMessage {
    pub trace_id: String,
    pub channel: String,
    pub payload: Value,
    pub metadata: Value,
    /// Serialized W3C trace context headers captured at submission time.
    /// Used to link async processing spans back to the originating request.
    #[cfg(feature = "otel")]
    pub trace_headers: std::collections::HashMap<String, String>,
}

/// In-memory trace queue backed by a tokio mpsc channel.
///
/// Traces are submitted via `submit()` and processed by a semaphore-limited
/// worker pool that runs in the background.
#[derive(Clone)]
pub struct TraceQueue {
    sender: mpsc::Sender<QueueMessage>,
    pending_count: Arc<AtomicUsize>,
    memory_bytes: Arc<AtomicUsize>,
    max_memory_bytes: usize,
}

impl TraceQueue {
    /// Submit a trace to the queue for background processing.
    pub async fn submit(&self, msg: QueueMessage) -> Result<(), crate::errors::OrionError> {
        // Estimate payload memory (approximate — excludes struct overhead)
        let payload_size = msg.payload.to_string().len() + msg.metadata.to_string().len();

        // Check memory limit before enqueueing
        if self.max_memory_bytes > 0 {
            let current = self.memory_bytes.load(Ordering::Relaxed);
            if current + payload_size > self.max_memory_bytes {
                return Err(crate::errors::OrionError::ServiceUnavailable(format!(
                    "Trace queue memory limit exceeded ({} + {} > {} bytes)",
                    current, payload_size, self.max_memory_bytes
                )));
            }
        }

        self.sender
            .send(msg)
            .await
            .map_err(|_| crate::errors::OrionError::Queue("Trace queue is closed".to_string()))?;

        let pending = self.pending_count.fetch_add(1, Ordering::Relaxed) + 1;
        metrics::set_trace_queue_depth(pending as f64);

        let mem = self.memory_bytes.fetch_add(payload_size, Ordering::Relaxed) + payload_size;
        metrics::set_trace_queue_memory_bytes(mem as f64);

        Ok(())
    }
}

/// Handle returned from `start_workers` to manage the worker lifecycle.
pub struct WorkerHandle {
    _sender: mpsc::Sender<QueueMessage>,
    join_handle: tokio::task::JoinHandle<()>,
    shutdown_timeout_secs: u64,
}

impl WorkerHandle {
    /// Gracefully shut down the worker pool.
    ///
    /// Drops the sender (the TraceQueue clone also holds one), so call this
    /// only after the HTTP server has stopped accepting new requests.
    /// The returned future resolves when all in-flight traces are complete.
    pub async fn shutdown(self) {
        drop(self._sender);
        // Wait for the dispatcher with a timeout to prevent hanging on stuck traces
        let timeout = Duration::from_secs(self.shutdown_timeout_secs);
        if tokio::time::timeout(timeout, self.join_handle)
            .await
            .is_err()
        {
            tracing::warn!(
                timeout_secs = self.shutdown_timeout_secs,
                "Trace queue workers did not shut down within timeout, proceeding with exit"
            );
        }
    }
}

/// Start the background worker pool and return a (TraceQueue, WorkerHandle) pair.
///
/// `max_workers` controls the maximum number of concurrent traces.
/// `buffer_size` controls the channel buffer (pending traces waiting to be picked up).
/// `shutdown_timeout_secs` controls how long to wait for in-flight traces during shutdown.
#[allow(clippy::too_many_arguments)]
pub fn start_workers(
    max_workers: usize,
    buffer_size: usize,
    shutdown_timeout_secs: u64,
    processing_timeout_ms: u64,
    max_result_size_bytes: usize,
    max_queue_memory_bytes: usize,
    engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    trace_repo: Arc<dyn TraceRepository>,
    dlq_repo: Option<Arc<dyn TraceDlqRepository>>,
) -> (TraceQueue, WorkerHandle) {
    let (tx, rx) = mpsc::channel::<QueueMessage>(buffer_size);
    let pending_count = Arc::new(AtomicUsize::new(0));
    let active_workers = Arc::new(AtomicUsize::new(0));
    let memory_bytes = Arc::new(AtomicUsize::new(0));

    metrics::set_trace_workers_total(max_workers as f64);

    let handle = tokio::spawn(dispatcher_loop(
        rx,
        max_workers,
        shutdown_timeout_secs,
        processing_timeout_ms,
        max_result_size_bytes,
        engine,
        trace_repo,
        dlq_repo,
        QueueCounters {
            pending: pending_count.clone(),
            active: active_workers,
            memory_bytes: memory_bytes.clone(),
        },
    ));

    let queue = TraceQueue {
        sender: tx.clone(),
        pending_count,
        memory_bytes,
        max_memory_bytes: max_queue_memory_bytes,
    };
    let worker_handle = WorkerHandle {
        _sender: tx,
        join_handle: handle,
        shutdown_timeout_secs,
    };

    (queue, worker_handle)
}

/// Shared counters for queue observability metrics.
struct QueueCounters {
    pending: Arc<AtomicUsize>,
    active: Arc<AtomicUsize>,
    memory_bytes: Arc<AtomicUsize>,
}

/// Main dispatcher loop: receives traces from the channel and spawns processing
/// tasks, limited by a semaphore to `max_workers` concurrent traces.
#[allow(clippy::too_many_arguments)]
async fn dispatcher_loop(
    mut rx: mpsc::Receiver<QueueMessage>,
    max_workers: usize,
    shutdown_timeout_secs: u64,
    processing_timeout_ms: u64,
    max_result_size_bytes: usize,
    engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    trace_repo: Arc<dyn TraceRepository>,
    dlq_repo: Option<Arc<dyn TraceDlqRepository>>,
    counters: QueueCounters,
) {
    let semaphore = Arc::new(Semaphore::new(max_workers));

    while let Some(msg) = rx.recv().await {
        // Acquire a permit — blocks if all workers are busy
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break, // Semaphore closed
        };

        // Estimate payload size for memory accounting
        let estimated_size = msg.payload.to_string().len() + msg.metadata.to_string().len();

        // Dequeued — decrement pending, increment active
        let pending = counters
            .pending
            .fetch_sub(1, Ordering::Relaxed)
            .saturating_sub(1);
        metrics::set_trace_queue_depth(pending as f64);
        let active = counters.active.fetch_add(1, Ordering::Relaxed) + 1;
        metrics::set_trace_workers_active(active as f64);

        let engine = engine.clone();
        let trace_repo = trace_repo.clone();
        let dlq_repo = dlq_repo.clone();
        let active_counter = counters.active.clone();
        let memory_counter = counters.memory_bytes.clone();

        tokio::spawn(async move {
            let _permit = permit; // guard: dropped on scope exit, even on panic
            process_trace(
                msg,
                engine,
                trace_repo,
                dlq_repo,
                processing_timeout_ms,
                max_result_size_bytes,
            )
            .await;
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
        Duration::from_secs(shutdown_timeout_secs),
        semaphore.acquire_many(max_workers as u32),
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

/// Process a single queued trace.
#[tracing::instrument(skip(msg, engine, trace_repo, dlq_repo, processing_timeout_ms, max_result_size_bytes), fields(trace_id = %msg.trace_id, channel = %msg.channel))]
async fn process_trace(
    msg: QueueMessage,
    engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    trace_repo: Arc<dyn TraceRepository>,
    dlq_repo: Option<Arc<dyn TraceDlqRepository>>,
    processing_timeout_ms: u64,
    max_result_size_bytes: usize,
) {
    // Restore W3C trace context from the originating request so this span
    // appears as a child in the caller's distributed trace.
    #[cfg(feature = "otel")]
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
        tracing::Span::current().set_parent(cx);
    }

    let trace_id = msg.trace_id;
    let channel = msg.channel;
    let start = Instant::now();

    // Capture payload/metadata for potential DLQ enqueue before consuming
    let payload_json_for_dlq = serde_json::to_string(&msg.payload).ok();
    let metadata_json_for_dlq = serde_json::to_string(&msg.metadata).ok();

    // Mark as running
    if let Err(e) = trace_repo
        .update_status(&trace_id, models::TRACE_STATUS_RUNNING, None)
        .await
    {
        tracing::error!(trace_id = %trace_id, error = %e, "Failed to update trace status to running");
        return;
    }

    // Build message
    let mut message = dataflow_rs::Message::from_value(&msg.payload);
    crate::engine::utils::merge_metadata(&mut message, &msg.metadata);
    crate::engine::utils::inject_rollout_bucket(&mut message);

    // Clone the inner Arc<Engine> and release the lock immediately
    let engine_ref = crate::engine::acquire_engine_read(&engine).await;
    let result = match tokio::time::timeout(
        Duration::from_millis(processing_timeout_ms),
        engine_ref.process_message_for_channel(&channel, &mut message),
    )
    .await
    {
        Ok(inner) => inner,
        Err(_) => {
            tracing::warn!(
                trace_id = %trace_id,
                channel = %channel,
                timeout_ms = processing_timeout_ms,
                "Async trace processing timed out"
            );
            Err(dataflow_rs::DataflowError::Timeout(format!(
                "Processing timed out after {}ms",
                processing_timeout_ms
            )))
        }
    };

    crate::engine::utils::remove_rollout_bucket(&mut message);

    let duration = start.elapsed();
    let duration_secs = duration.as_secs_f64();
    let duration_ms = duration.as_secs_f64() * 1000.0;

    match result {
        Ok(()) => {
            metrics::record_message(&channel, "ok");
            metrics::record_message_duration(&channel, duration_secs);
            metrics::record_channel_execution(&channel);

            let result_json = match serde_json::to_string(&message) {
                Ok(json) => json,
                Err(e) => {
                    tracing::error!(trace_id = %trace_id, error = %e, "Failed to serialize trace result");
                    set_trace_status(
                        trace_repo.as_ref(),
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
                set_trace_status(
                    trace_repo.as_ref(),
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

            let mut result_saved = false;
            for attempt in 0..3 {
                match trace_repo
                    .set_result(&trace_id, &result_json, duration_ms)
                    .await
                {
                    Ok(_) => {
                        result_saved = true;
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

            if result_saved {
                set_trace_status(
                    trace_repo.as_ref(),
                    &trace_id,
                    models::TRACE_STATUS_COMPLETED,
                    None,
                )
                .await;
            } else {
                tracing::error!(trace_id = %trace_id, "Failed to save trace result after 3 attempts, marking as failed");
                set_trace_status(
                    trace_repo.as_ref(),
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
            set_trace_status(
                trace_repo.as_ref(),
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

// ============================================================
// DLQ Retry Consumer
// ============================================================

/// Start a background task that polls the trace DLQ and re-submits entries
/// for processing via the trace queue.
///
/// Uses exponential backoff: delay = base_delay * 2^retry_count (1s, 2s, 4s, 8s, 16s, ...).
/// After `max_retries`, the entry is marked as exhausted and no longer retried.
pub fn start_dlq_retry(
    poll_interval_secs: u64,
    dlq_repo: Arc<dyn TraceDlqRepository>,
    trace_queue: TraceQueue,
    trace_repo: Arc<dyn TraceRepository>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(poll_interval_secs));
        // Skip the first immediate tick
        interval.tick().await;

        loop {
            interval.tick().await;

            let entries = match dlq_repo.list_pending(20).await {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!(error = %e, "DLQ retry: failed to fetch pending entries");
                    continue;
                }
            };

            for entry in entries {
                let payload: serde_json::Value = match serde_json::from_str(&entry.payload_json) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(
                            dlq_id = %entry.id,
                            error = %e,
                            "DLQ retry: corrupt payload, marking exhausted"
                        );
                        let _ = dlq_repo.mark_exhausted(&entry.id).await;
                        continue;
                    }
                };

                let metadata: serde_json::Value =
                    serde_json::from_str(&entry.metadata_json).unwrap_or_default();

                // Create a new pending trace for the retry
                let new_trace = match trace_repo
                    .create_pending(&entry.channel, "async", Some(&entry.payload_json))
                    .await
                {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!(
                            dlq_id = %entry.id,
                            error = %e,
                            "DLQ retry: failed to create pending trace"
                        );
                        continue;
                    }
                };

                // Submit to the trace queue
                let submit_result = trace_queue
                    .submit(QueueMessage {
                        trace_id: new_trace.id.clone(),
                        channel: entry.channel.clone(),
                        payload,
                        metadata,
                        #[cfg(feature = "otel")]
                        trace_headers: std::collections::HashMap::new(),
                    })
                    .await;

                match submit_result {
                    Ok(()) => {
                        // Successfully re-submitted — remove from DLQ
                        if let Err(e) = dlq_repo.remove(&entry.id).await {
                            tracing::error!(
                                dlq_id = %entry.id,
                                error = %e,
                                "DLQ retry: failed to remove entry after successful resubmit"
                            );
                        } else {
                            tracing::info!(
                                dlq_id = %entry.id,
                                retry = entry.retry_count + 1,
                                new_trace_id = %new_trace.id,
                                "DLQ retry: trace re-submitted successfully"
                            );
                        }
                    }
                    Err(e) => {
                        // Queue is full or closed — bump retry count with backoff
                        tracing::warn!(
                            dlq_id = %entry.id,
                            error = %e,
                            "DLQ retry: failed to resubmit, scheduling next retry"
                        );
                        let new_retry_count = entry.retry_count + 1;
                        if new_retry_count >= entry.max_retries {
                            metrics::record_error("dlq_exhausted");
                            let _ = dlq_repo.mark_exhausted(&entry.id).await;
                            tracing::warn!(
                                dlq_id = %entry.id,
                                "DLQ retry: max retries exhausted, giving up"
                            );
                        } else {
                            // Exponential backoff: 1s, 2s, 4s, 8s, 16s, ...
                            let delay_secs = 1i64 << new_retry_count;
                            let next_retry = chrono::Utc::now()
                                .naive_utc()
                                .checked_add_signed(chrono::Duration::seconds(delay_secs))
                                .unwrap_or(chrono::Utc::now().naive_utc())
                                .to_string();
                            let _ = dlq_repo.record_retry(&entry.id, &next_retry).await;
                        }
                    }
                }
            }
        }
    })
}
