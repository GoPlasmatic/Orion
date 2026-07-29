pub mod audit_cleanup;
mod dlq_retry;
mod processing;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{RwLock, mpsc};

use crate::metrics;
use crate::storage::repositories::trace_dlq::TraceDlqRepository;

pub mod trace_persistence;
use crate::storage::repositories::traces::TraceRepository;
pub use trace_persistence::{PersistenceWorkerHandle, TracePersistenceQueue, TracePersistenceTask};

pub use dlq_retry::{DlqRetryOptions, start_dlq_retry};

/// Start a background task that periodically deletes old traces.
///
/// Returns a `JoinHandle` that can be aborted on shutdown.
/// If `retention_hours` is 0, no cleanup task is started.
///
/// `lease_gate` (cluster mode) single-flights the job: without it every
/// replica issues the same DELETE every tick. `None` on a single node.
pub fn start_trace_cleanup(
    retention_hours: u64,
    interval_secs: u64,
    trace_repo: Arc<dyn TraceRepository>,
    lease_gate: Option<Arc<crate::cluster::JobLeaseGate>>,
) -> Option<tokio::task::JoinHandle<()>> {
    if retention_hours == 0 {
        tracing::info!("Trace retention disabled (retention_hours = 0)");
        return None;
    }

    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
        // Skip the first immediate tick
        interval.tick().await;
        let lease_ttl = interval_secs + 60;

        loop {
            interval.tick().await;
            if let Some(ref gate) = lease_gate
                && !gate.try_acquire("trace_cleanup", lease_ttl).await
            {
                continue;
            }
            match trace_repo.delete_older_than(retention_hours).await {
                Ok(count) => {
                    metrics::record_job_success("trace_cleanup");
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
    /// `true` when the original request asked for profile data (header or
    /// query). The worker creates a per-request `ProfileCollector` and
    /// embeds the result under `metadata._orion_profile` before persisting
    /// the trace's `result_json`.
    pub profile_requested: bool,
    /// Per-channel backpressure permit acquired at submission time (S1).
    /// The worker holds it for the duration of processing so a channel's
    /// `max_concurrent_per_node` bounds sync and async work together. `None`
    /// when the channel has no backpressure config, or for DLQ resubmissions.
    pub backpressure_permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

/// A queued message plus the bookkeeping submitters never set themselves.
///
/// `dlq_retry_count` is how many DLQ cycles this message's lineage has already
/// burned: 0 for a fresh submission, the originating row's count + 1 for a DLQ
/// resubmission. Carrying it forward is what makes `queue.dlq_max_retries`
/// enforceable — the retry loop deletes the DLQ row once resubmission
/// succeeds, so without it every workflow failure re-entered the DLQ at 0 and
/// a deterministically-failing message looped forever (Q3).
pub(crate) struct QueuedItem {
    pub(crate) msg: QueueMessage,
    pub(crate) dlq_retry_count: i64,
}

/// In-memory trace queue backed by a tokio mpsc channel.
///
/// Traces are submitted via `submit()` and processed by a semaphore-limited
/// worker pool that runs in the background.
#[derive(Clone)]
pub struct TraceQueue {
    sender: mpsc::Sender<QueuedItem>,
    pending_count: Arc<AtomicUsize>,
    memory_bytes: Arc<AtomicUsize>,
    max_memory_bytes: usize,
}

impl TraceQueue {
    /// Create a TraceQueue for testing. The receiver must be consumed elsewhere.
    #[cfg(test)]
    pub(crate) fn new_for_test(sender: mpsc::Sender<QueuedItem>) -> Self {
        Self {
            sender,
            pending_count: Arc::new(AtomicUsize::new(0)),
            memory_bytes: Arc::new(AtomicUsize::new(0)),
            max_memory_bytes: 100_000_000,
        }
    }

    /// Submit a trace to the queue for background processing.
    pub async fn submit(&self, msg: QueueMessage) -> Result<(), crate::errors::OrionError> {
        self.enqueue(QueuedItem {
            msg,
            dlq_retry_count: 0,
        })
        .await
    }

    /// Re-submit a message claimed from the DLQ, carrying the retry count its
    /// lineage has already spent so a repeat failure re-enters the DLQ one
    /// step closer to exhaustion instead of back at zero (Q3).
    pub(crate) async fn submit_dlq_retry(
        &self,
        msg: QueueMessage,
        dlq_retry_count: i64,
    ) -> Result<(), crate::errors::OrionError> {
        self.enqueue(QueuedItem {
            msg,
            dlq_retry_count,
        })
        .await
    }

    /// Sheds rather than waits. `buffer_size` is a shed threshold, not a
    /// waiting room: awaiting capacity here parks the calling HTTP handler for
    /// as long as the workers stay behind, turning saturation into unbounded
    /// request latency instead of the documented 503 (Q1).
    async fn enqueue(&self, item: QueuedItem) -> Result<(), crate::errors::OrionError> {
        // Estimate payload memory (approximate — excludes struct overhead).
        let payload_size = item.msg.payload.to_string().len() + item.msg.metadata.to_string().len();

        // Q2: reserve first, then validate. The previous shape was
        // load -> compare -> send -> fetch_add, so N concurrent submitters all
        // read the same pre-add value, all passed the check, and the accounted
        // total overshot the configured ceiling by up to N x payload_size.
        // Reserving up front makes the check authoritative; the reservation is
        // released again on rejection.
        if self.max_memory_bytes > 0 {
            let prev = self.memory_bytes.fetch_add(payload_size, Ordering::AcqRel);
            let total = prev + payload_size;
            if total > self.max_memory_bytes {
                self.memory_bytes.fetch_sub(payload_size, Ordering::AcqRel);
                metrics::record_trace_queue_rejected("memory");
                return Err(crate::errors::OrionError::ServiceUnavailable(format!(
                    "Trace queue memory limit exceeded ({} + {} > {} bytes)",
                    prev, payload_size, self.max_memory_bytes
                )));
            }
            metrics::set_trace_queue_memory_bytes(total as f64);
        }

        if let Err(err) = self.sender.try_send(item) {
            // Release the reservation taken above — the item never entered the
            // queue, so nothing downstream will subtract it.
            if self.max_memory_bytes > 0 {
                self.memory_bytes.fetch_sub(payload_size, Ordering::AcqRel);
            }
            return Err(match err {
                // The rejected message is dropped here, releasing the
                // backpressure permit it carried — a shed submission must not
                // hold a slice of the channel's `max_concurrent_per_node`.
                mpsc::error::TrySendError::Full(_) => {
                    metrics::record_trace_queue_rejected("full");
                    crate::errors::OrionError::ServiceUnavailable(format!(
                        "Trace queue is full ({} messages pending)",
                        self.pending_count.load(Ordering::Relaxed)
                    ))
                }
                mpsc::error::TrySendError::Closed(_) => {
                    crate::errors::OrionError::Queue("Trace queue is closed".to_string())
                }
            });
        }

        let pending = self.pending_count.fetch_add(1, Ordering::Relaxed) + 1;
        metrics::set_trace_queue_depth(pending as f64);

        // When the limit is disabled the counter is still maintained for the
        // gauge, but no reservation was taken above.
        if self.max_memory_bytes == 0 {
            let mem = self.memory_bytes.fetch_add(payload_size, Ordering::AcqRel) + payload_size;
            metrics::set_trace_queue_memory_bytes(mem as f64);
        }

        Ok(())
    }
}

/// Handle returned from `start_workers` to manage the worker lifecycle.
pub struct WorkerHandle {
    _sender: mpsc::Sender<QueuedItem>,
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
#[allow(clippy::too_many_arguments)]
pub fn start_workers(
    config: &crate::config::TraceQueueConfig,
    engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    trace_repo: Arc<dyn TraceRepository>,
    dlq_repo: Option<Arc<dyn TraceDlqRepository>>,
    channel_registry: Arc<crate::channel::ChannelRegistry>,
    persistence_queue: TracePersistenceQueue,
    global_trace_storage: crate::config::TraceStorageConfig,
    rollout_sticky_header: String,
) -> (TraceQueue, WorkerHandle) {
    let max_workers = config.workers;
    let buffer_size = config.buffer_size;
    let shutdown_timeout_secs = config.shutdown_timeout_secs;
    let max_queue_memory_bytes = config.max_queue_memory_bytes;

    let (tx, rx) = mpsc::channel::<QueuedItem>(buffer_size);
    let pending_count = Arc::new(AtomicUsize::new(0));
    let active_workers = Arc::new(AtomicUsize::new(0));
    let memory_bytes = Arc::new(AtomicUsize::new(0));

    metrics::set_trace_workers_total(max_workers as f64);

    let dispatcher_ctx = processing::DispatcherContext {
        max_workers,
        shutdown_timeout_secs,
        counters: processing::QueueCounters {
            pending: pending_count.clone(),
            active: active_workers,
            memory_bytes: memory_bytes.clone(),
        },
        processing: processing::ProcessingContext {
            engine,
            trace_repo,
            dlq_repo,
            processing_timeout_ms: config.processing_timeout_ms,
            max_result_size_bytes: config.max_result_size_bytes,
            dlq_max_retries: config.dlq_max_retries,
            rollout_sticky_header: Arc::from(rollout_sticky_header.as_str()),
            channel_registry,
            persistence_queue,
            global_trace_storage,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_message(trace_id: &str) -> QueueMessage {
        QueueMessage {
            trace_id: trace_id.to_string(),
            channel: "orders".to_string(),
            payload: serde_json::json!({"a": 1}),
            metadata: serde_json::json!({}),
            trace_headers: std::collections::HashMap::new(),
            profile_requested: false,
            backpressure_permit: None,
        }
    }

    #[tokio::test]
    async fn submit_rejects_when_buffer_is_full() {
        let (tx, _rx) = mpsc::channel::<QueuedItem>(1);
        let queue = TraceQueue::new_for_test(tx);

        queue.submit(test_message("t1")).await.expect("first fits");

        // Must resolve immediately with 503 rather than parking the caller
        // until a worker drains the buffer.
        let err =
            tokio::time::timeout(Duration::from_millis(250), queue.submit(test_message("t2")))
                .await
                .expect("submit must not block on a full queue")
                .expect_err("full queue must be rejected");

        assert!(
            matches!(err, crate::errors::OrionError::ServiceUnavailable(_)),
            "expected ServiceUnavailable, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn submit_rejection_releases_backpressure_permit() {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = semaphore
            .clone()
            .try_acquire_owned()
            .expect("permit available");

        let (tx, _rx) = mpsc::channel::<QueuedItem>(1);
        let queue = TraceQueue::new_for_test(tx);
        queue.submit(test_message("t1")).await.expect("first fits");

        let mut msg = test_message("t2");
        msg.backpressure_permit = Some(permit);
        assert!(queue.submit(msg).await.is_err(), "second must be shed");

        assert_eq!(
            semaphore.available_permits(),
            1,
            "a shed submission must not retain the channel's backpressure permit"
        );
    }

    #[tokio::test]
    async fn submit_reports_closed_queue_separately() {
        let (tx, rx) = mpsc::channel::<QueuedItem>(1);
        drop(rx);
        let queue = TraceQueue::new_for_test(tx);

        let err = queue
            .submit(test_message("t1"))
            .await
            .expect_err("closed queue must be rejected");
        assert!(
            matches!(err, crate::errors::OrionError::Queue(_)),
            "expected Queue error, got: {err:?}"
        );
    }

    /// `delete_older_than` succeeds and nothing else; every other method is
    /// off the cleanup path.
    struct MockCleanupTraceRepo;

    #[async_trait::async_trait]
    impl TraceRepository for MockCleanupTraceRepo {
        async fn create_pending(
            &self,
            _channel: &str,
            _channel_id: Option<&str>,
            _mode: &str,
            _input_json: Option<&str>,
            _access_token_hash: Option<&str>,
        ) -> Result<crate::storage::models::Trace, crate::errors::OrionError> {
            unimplemented!("not used by trace cleanup")
        }
        async fn get_by_id(
            &self,
            _id: &str,
        ) -> Result<crate::storage::models::Trace, crate::errors::OrionError> {
            unimplemented!("not used by trace cleanup")
        }
        async fn update_status(
            &self,
            _id: &str,
            _status: &str,
            _error_message: Option<&str>,
        ) -> Result<crate::storage::models::Trace, crate::errors::OrionError> {
            unimplemented!("not used by trace cleanup")
        }
        async fn set_result(
            &self,
            _id: &str,
            _result_json: &str,
            _duration_ms: f64,
            _task_trace_json: Option<&str>,
        ) -> Result<(), crate::errors::OrionError> {
            unimplemented!("not used by trace cleanup")
        }
        async fn store_completed(
            &self,
            _channel: &str,
            _channel_id: Option<&str>,
            _mode: &str,
            _input_json: Option<&str>,
            _result_json: &str,
            _duration_ms: f64,
            _task_trace_json: Option<&str>,
        ) -> Result<String, crate::errors::OrionError> {
            unimplemented!("not used by trace cleanup")
        }
        async fn list_paginated(
            &self,
            _filter: &crate::storage::repositories::traces::TraceFilter,
        ) -> Result<
            crate::storage::repositories::helpers::PaginatedResult<
                crate::storage::models::TraceListRow,
            >,
            crate::errors::OrionError,
        > {
            unimplemented!("not used by trace cleanup")
        }
        async fn delete_older_than(&self, _hours: u64) -> Result<u64, crate::errors::OrionError> {
            // "Nothing to delete" is still a successful tick.
            Ok(0)
        }
    }

    /// O3: a successful cleanup tick must stamp `job_last_success_timestamp`
    /// — the gauge whose staleness is the only alertable signal that the
    /// cleanup loop is silently failing. Same paused-clock local-recorder
    /// pattern as the audit_cleanup and dlq_retry tests.
    #[test]
    fn test_successful_tick_stamps_the_job_health_gauge() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        ::metrics::with_local_recorder(&recorder, || {
            crate::metrics::set_enabled(true);
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .start_paused(true)
                .build()
                .expect("test runtime")
                .block_on(async {
                    let repo: Arc<dyn TraceRepository> = Arc::new(MockCleanupTraceRepo);
                    let job = start_trace_cleanup(24, 1, repo, None).expect("job started");
                    // One advance consumes the skipped immediate tick, the
                    // next fires the first real one.
                    tokio::time::advance(Duration::from_secs(1)).await;
                    for _ in 0..20 {
                        tokio::task::yield_now().await;
                    }
                    tokio::time::advance(Duration::from_secs(1)).await;
                    for _ in 0..20 {
                        tokio::task::yield_now().await;
                    }
                    job.abort();
                });
        });
        let out = handle.render();
        assert!(
            out.contains(r#"orion_job_last_success_timestamp_seconds{job="trace_cleanup"}"#),
            "a successful cleanup tick must stamp the job health gauge:\n{out}"
        );
    }
}
