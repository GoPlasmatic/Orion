mod dlq_retry;
mod processing;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{RwLock, mpsc};

use crate::metrics;
use crate::storage::repositories::trace_dlq::TraceDlqRepository;
use crate::storage::repositories::traces::TraceRepository;

pub use dlq_retry::start_dlq_retry;

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
/// Scalar config parameters (workers, buffer_size, timeouts, limits) are read
/// from `config`. The Arc dependencies (engine, repos) are passed separately
/// because they have independent lifetimes.
pub fn start_workers(
    config: &crate::config::QueueConfig,
    engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    trace_repo: Arc<dyn TraceRepository>,
    dlq_repo: Option<Arc<dyn TraceDlqRepository>>,
) -> (TraceQueue, WorkerHandle) {
    let max_workers = config.workers;
    let buffer_size = config.buffer_size;
    let shutdown_timeout_secs = config.shutdown_timeout_secs;
    let max_queue_memory_bytes = config.max_queue_memory_bytes;

    let (tx, rx) = mpsc::channel::<QueueMessage>(buffer_size);
    let pending_count = Arc::new(AtomicUsize::new(0));
    let active_workers = Arc::new(AtomicUsize::new(0));
    let memory_bytes = Arc::new(AtomicUsize::new(0));

    metrics::set_trace_workers_total(max_workers as f64);

    let dispatcher_ctx = processing::DispatcherContext {
        max_workers,
        shutdown_timeout_secs,
        processing_timeout_ms: config.processing_timeout_ms,
        max_result_size_bytes: config.max_result_size_bytes,
        engine,
        trace_repo,
        dlq_repo,
        counters: processing::QueueCounters {
            pending: pending_count.clone(),
            active: active_workers,
            memory_bytes: memory_bytes.clone(),
        },
    };

    let handle = tokio::spawn(processing::dispatcher_loop(rx, dispatcher_ctx));

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
