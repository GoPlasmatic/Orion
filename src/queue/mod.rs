use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{RwLock, Semaphore, mpsc};

use crate::metrics;
use crate::storage::models;
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
}

impl TraceQueue {
    /// Submit a trace to the queue for background processing.
    pub async fn submit(&self, msg: QueueMessage) -> Result<(), crate::errors::OrionError> {
        self.sender
            .send(msg)
            .await
            .map_err(|_| crate::errors::OrionError::Queue("Trace queue is closed".to_string()))
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
pub fn start_workers(
    max_workers: usize,
    buffer_size: usize,
    shutdown_timeout_secs: u64,
    engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    trace_repo: Arc<dyn TraceRepository>,
) -> (TraceQueue, WorkerHandle) {
    let (tx, rx) = mpsc::channel::<QueueMessage>(buffer_size);

    let handle = tokio::spawn(dispatcher_loop(
        rx,
        max_workers,
        shutdown_timeout_secs,
        engine,
        trace_repo,
    ));

    let queue = TraceQueue { sender: tx.clone() };
    let worker_handle = WorkerHandle {
        _sender: tx,
        join_handle: handle,
        shutdown_timeout_secs,
    };

    (queue, worker_handle)
}

/// Main dispatcher loop: receives traces from the channel and spawns processing
/// tasks, limited by a semaphore to `max_workers` concurrent traces.
async fn dispatcher_loop(
    mut rx: mpsc::Receiver<QueueMessage>,
    max_workers: usize,
    shutdown_timeout_secs: u64,
    engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    trace_repo: Arc<dyn TraceRepository>,
) {
    let semaphore = Arc::new(Semaphore::new(max_workers));

    while let Some(msg) = rx.recv().await {
        // Acquire a permit — blocks if all workers are busy
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break, // Semaphore closed
        };

        let engine = engine.clone();
        let trace_repo = trace_repo.clone();

        tokio::spawn(async move {
            let _permit = permit; // guard: dropped on scope exit, even on panic
            process_trace(msg, engine, trace_repo).await;
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
#[tracing::instrument(skip(msg, engine, trace_repo), fields(trace_id = %msg.trace_id, channel = %msg.channel))]
async fn process_trace(
    msg: QueueMessage,
    engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    trace_repo: Arc<dyn TraceRepository>,
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
    let engine_ref = engine.read().await.clone();
    let result = engine_ref
        .process_message_for_channel(&channel, &mut message)
        .await;

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

            set_trace_status(
                trace_repo.as_ref(),
                &trace_id,
                models::TRACE_STATUS_FAILED,
                Some(&e.to_string()),
            )
            .await;
        }
    }
}
