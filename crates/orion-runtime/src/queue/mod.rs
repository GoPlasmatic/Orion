pub mod audit_cleanup;
pub mod audit_queue;
mod bounded;
mod dlq_retry;
mod processing;
pub mod trace_record;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::Value;

use self::bounded::{BoundedWorker, DrainHandle, DrainOutcome, DrainWitness, Rejected};
use crate::metrics;
use crate::storage::repositories::trace_dlq::TraceDlqRepository;

pub mod trace_persistence;
use crate::storage::repositories::traces::{TraceRetention, TraceSink};
pub use trace_persistence::{PersistenceWorkerHandle, TracePersistenceQueue, TracePersistenceTask};

pub use dlq_retry::{DlqRetryOptions, start_dlq_retry};

/// The shared body of the periodic retention jobs (trace cleanup here, audit
/// cleanup in [`audit_cleanup`]): skip the first immediate tick, single-flight
/// each tick through the lease gate, run one pass, stamp the job health gauge.
///
/// `delete` runs one retention pass and reports how many rows it removed;
/// `report` logs that outcome. Logging stays with the caller because tracing
/// field names and messages must be literals and each job names its own
/// retention unit.
///
/// `lease_gate` (cluster mode) single-flights the job: without it every
/// replica issues the same DELETE every tick. `None` on a single node.
async fn run_retention_job<F, Fut, R>(
    job: &'static str,
    interval_secs: u64,
    lease_gate: Option<Arc<crate::cluster::JobLeaseGate>>,
    delete: Arc<F>,
    report: Arc<R>,
    mut shutdown: crate::runtime::Shutdown,
) where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<u64, crate::errors::OrionError>> + Send + 'static,
    R: Fn(Result<u64, crate::errors::OrionError>) + Send + Sync + 'static,
{
    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
    // Skip the first immediate tick
    interval.tick().await;
    let lease_ttl = interval_secs + 60;

    loop {
        // The tick races the shutdown signal, so stopping the node costs at
        // most one pass rather than one whole interval — retention intervals
        // are measured in hours. Stopping with `JoinHandle::abort()` instead
        // could cut a pass between its DELETE and its metric.
        tokio::select! {
            _ = interval.tick() => {}
            _ = shutdown.signalled() => return,
        }
        if let Some(ref gate) = lease_gate
            && !gate.try_acquire(job, lease_ttl).await
        {
            continue;
        }
        let outcome = delete().await;
        if outcome.is_ok() {
            metrics::record_job_success(job);
        }
        report(outcome);
    }
}

/// Start a background task that periodically deletes old traces.
///
/// Registered with the supervisor, which restarts it after a capped backoff
/// if it ever stops early.
/// If `retention_hours` is 0, no cleanup task is started.
///
/// `lease_gate` (cluster mode) single-flights the job: without it every
/// replica issues the same DELETE every tick. `None` on a single node.
pub fn start_trace_cleanup(
    tasks: &crate::runtime::TaskRegistry,
    retention_hours: u64,
    interval_secs: u64,
    trace_repo: Arc<dyn TraceRetention>,
    lease_gate: Option<Arc<crate::cluster::JobLeaseGate>>,
) {
    if retention_hours == 0 {
        tracing::info!("Trace retention disabled (retention_hours = 0)");
        return;
    }

    supervise_retention_job(
        tasks,
        "trace_cleanup",
        interval_secs,
        lease_gate,
        // Cloned inside the closure: `async_trait` ties the returned future to
        // `&self`, so the borrow has to live in the future, not the closure.
        move || {
            let repo = trace_repo.clone();
            async move { repo.delete_older_than(retention_hours).await }
        },
        move |outcome| match outcome {
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
        },
    );

    tracing::info!(
        retention_hours = retention_hours,
        interval_secs = interval_secs,
        "Trace cleanup task started"
    );
}

/// Register a retention job with the supervisor.
///
/// The `Arc`s are what make the body a *factory*: [`TaskRegistry::supervise`]
/// re-runs it after a failure, so the closures cannot be moved into it — each
/// attempt clones them instead.
///
/// [`TaskRegistry::supervise`]: crate::runtime::TaskRegistry::supervise
pub(crate) fn supervise_retention_job<F, Fut, R>(
    tasks: &crate::runtime::TaskRegistry,
    job: &'static str,
    interval_secs: u64,
    lease_gate: Option<Arc<crate::cluster::JobLeaseGate>>,
    delete: F,
    report: R,
) where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<u64, crate::errors::OrionError>> + Send + 'static,
    R: Fn(Result<u64, crate::errors::OrionError>) + Send + Sync + 'static,
{
    let delete = Arc::new(delete);
    let report = Arc::new(report);
    // Retention is Optional: a node that has stopped expiring old rows still
    // answers every request correctly, so this is a `/health` degradation and
    // not a reason to take the node out of rotation.
    tasks.supervise(
        job,
        crate::runtime::Criticality::Optional,
        move |shutdown| {
            run_retention_job(
                job,
                interval_secs,
                lease_gate.clone(),
                delete.clone(),
                report.clone(),
                shutdown,
            )
        },
    );
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
    /// embeds the result under the top-level `_orion.profile` key of the
    /// trace's persisted `result_json` (see `serialize_result_with_profile`).
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
/// enforceable (Q3).
///
/// `dlq_entry_id` and `dlq_max_retries` identify the durable `trace_dlq` row
/// whose claim lease backs a retry attempt (`TD-05`). The row is kept under
/// lease while the item is queued and executing, and settled (`remove`,
/// `record_retry`, or `mark_exhausted`) only after execution finishes.
pub(crate) struct QueuedItem {
    pub(crate) msg: QueueMessage,
    pub(crate) dlq_retry_count: i64,
    pub(crate) dlq_entry_id: Option<String>,
    pub(crate) dlq_max_retries: Option<i64>,
    /// Bytes reserved for this item by `enqueue`, carried so the dispatcher
    /// releases exactly what was reserved instead of re-serializing the
    /// payload to recompute it. Set by `enqueue`; submitters leave it 0.
    pub(crate) payload_size: usize,
}

/// Zero-allocation [`std::io::Write`] sink that counts serialized JSON bytes
/// without materializing an intermediate `String` or `Vec<u8>`.
#[derive(Default)]
struct JsonByteCounter(usize);

impl std::io::Write for JsonByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn json_serialized_len(value: &serde_json::Value) -> usize {
    let mut counter = JsonByteCounter::default();
    let _ = serde_json::to_writer(&mut counter, value);
    counter.0
}

/// A queued message's memory estimate: the serialized length of its payload
/// and metadata (approximate — it excludes struct overhead), counted with a
/// zero-allocation byte counter rather than materializing two heap strings
/// just to read `.len()`.
///
/// `payload_json` is the payload already serialized, when the caller has it —
/// the async submission serializes it once for the pending trace row, and its
/// length is exactly what the counter would count, so the payload is not
/// walked again. Measured once per submission and carried to the admission
/// check and the reservation, so neither serializes it afresh.
pub fn message_size(payload: &Value, payload_json: Option<&str>, metadata: &Value) -> usize {
    payload_json.map_or_else(|| json_serialized_len(payload), str::len)
        + json_serialized_len(metadata)
}

/// In-memory trace queue backed by a tokio mpsc channel.
///
/// Traces are submitted via `submit()` and processed by a semaphore-limited
/// worker pool that runs in the background.
#[derive(Clone)]
pub struct TraceQueue {
    queue: BoundedWorker<QueuedItem>,
    memory_bytes: Arc<AtomicUsize>,
    max_memory_bytes: usize,
}

impl TraceQueue {
    /// Create a TraceQueue for testing, with its receiver handed back so a test
    /// can hold it (a full queue) or drop it (a closed one).
    #[cfg(test)]
    pub(crate) fn new_for_test(capacity: usize) -> (Self, bounded::WorkerReceiver<QueuedItem>) {
        let (queue, mut receivers) =
            BoundedWorker::new(1, capacity, metrics::set_trace_queue_depth);
        let rx = receivers.pop().expect("one shard was requested");
        (
            Self {
                queue,
                memory_bytes: Arc::new(AtomicUsize::new(0)),
                max_memory_bytes: 100_000_000,
            },
            rx,
        )
    }

    /// Submit a trace to the queue for background processing.
    pub async fn submit(&self, msg: QueueMessage) -> Result<(), crate::errors::OrionError> {
        let size = message_size(&msg.payload, None, &msg.metadata);
        self.submit_sized(msg, size).await
    }

    /// [`Self::submit`] for a caller that already knows the message's
    /// [`message_size`] — the async submission, which measured it once for
    /// [`Self::check_admissible_sized`] and must reserve exactly that.
    pub async fn submit_sized(
        &self,
        msg: QueueMessage,
        size: usize,
    ) -> Result<(), crate::errors::OrionError> {
        self.enqueue(QueuedItem {
            msg,
            dlq_retry_count: 0,
            dlq_entry_id: None,
            dlq_max_retries: None,
            payload_size: size,
        })
        .await
    }

    /// Fast pre-persistence admission check (`PERF-03`).
    ///
    /// Rejects submissions when the queue is closed, all shards are full, or
    /// the payload + metadata would exceed `max_memory_bytes` before callers
    /// allocate tokens or execute database `INSERT` + compensating `UPDATE`
    /// round-trips for a pending trace row. The authoritative reservation in
    /// `enqueue` still runs on `submit` to close any concurrent race.
    pub fn check_admissible(
        &self,
        payload: &Value,
        metadata: &Value,
    ) -> Result<(), crate::errors::OrionError> {
        self.check_admissible_sized(message_size(payload, None, metadata))
    }

    /// [`Self::check_admissible`] against a [`message_size`] the caller has
    /// already measured.
    pub fn check_admissible_sized(&self, size: usize) -> Result<(), crate::errors::OrionError> {
        if self.queue.is_all_closed() {
            return Err(crate::errors::OrionError::unavailable(
                crate::errors::Unavailable::QueueClosed,
                "Trace queue is closed",
            ));
        }
        if !self.queue.has_capacity() {
            metrics::record_trace_queue_rejected("full");
            return Err(crate::errors::OrionError::unavailable(
                crate::errors::Unavailable::AtCapacity,
                format!(
                    "Trace queue is full ({} messages pending)",
                    self.queue.depth()
                ),
            ));
        }
        if self.max_memory_bytes > 0 {
            let payload_size = size;
            let current = self.memory_bytes.load(Ordering::Acquire);
            if current.saturating_add(payload_size) > self.max_memory_bytes {
                metrics::record_trace_queue_rejected("memory");
                return Err(crate::errors::OrionError::unavailable(
                    crate::errors::Unavailable::AtCapacity,
                    format!(
                        "Trace queue memory limit exceeded ({} + {} > {} bytes)",
                        current, payload_size, self.max_memory_bytes
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Re-submit a message claimed from the DLQ, carrying the retry count its
    /// lineage has already spent and the durable DLQ entry ID/max-retries so
    /// post-execution settlement updates or removes the leased DLQ row only
    /// after processing completes (Q3, `TD-05`).
    pub(crate) async fn submit_dlq_retry(
        &self,
        msg: QueueMessage,
        dlq_entry_id: String,
        dlq_retry_count: i64,
        dlq_max_retries: i64,
    ) -> Result<(), crate::errors::OrionError> {
        let payload_size = message_size(&msg.payload, None, &msg.metadata);
        self.enqueue(QueuedItem {
            msg,
            dlq_retry_count,
            dlq_entry_id: Some(dlq_entry_id),
            dlq_max_retries: Some(dlq_max_retries),
            payload_size,
        })
        .await
    }

    /// Sheds rather than waits. `buffer_size` is a shed threshold, not a
    /// waiting room: awaiting capacity here parks the calling HTTP handler for
    /// as long as the workers stay behind, turning saturation into unbounded
    /// request latency instead of the documented 503 (Q1).
    async fn enqueue(&self, item: QueuedItem) -> Result<(), crate::errors::OrionError> {
        // The payload memory estimate the caller measured (`message_size`).
        let payload_size = item.payload_size;

        // Q2: reserve first, then validate. The previous shape was
        // load -> compare -> send -> fetch_add, so N concurrent submitters all
        // read the same pre-add value, all passed the check, and the accounted
        // total overshot the configured ceiling by up to N x payload_size.
        // Reserving up front makes the check authoritative; the reservation is
        // released again on rejection. The reservation is unconditional — the
        // counter feeds the gauge even when no ceiling is configured, so only
        // the ceiling test is gated on `max_memory_bytes`.
        let prev = self.memory_bytes.fetch_add(payload_size, Ordering::AcqRel);
        let total = prev + payload_size;
        if self.max_memory_bytes > 0 && total > self.max_memory_bytes {
            self.memory_bytes.fetch_sub(payload_size, Ordering::AcqRel);
            metrics::record_trace_queue_rejected("memory");
            return Err(crate::errors::OrionError::unavailable(
                crate::errors::Unavailable::AtCapacity,
                format!(
                    "Trace queue memory limit exceeded ({} + {} > {} bytes)",
                    prev, payload_size, self.max_memory_bytes
                ),
            ));
        }
        metrics::set_trace_queue_memory_bytes(total as f64);

        // Sheds rather than waits — see the doc comment above. The depth
        // reservation and its release on refusal are `BoundedWorker`'s; the
        // memory reservation released here is this queue's own.
        match self.queue.try_submit(item) {
            Ok(()) => Ok(()),
            Err(rejected) => {
                // The item never entered the queue, so nothing downstream will
                // subtract the bytes reserved for it.
                self.memory_bytes.fetch_sub(payload_size, Ordering::AcqRel);
                Err(match rejected {
                    // The rejected message is dropped here, releasing the
                    // backpressure permit it carried — a shed submission must
                    // not hold a slice of the channel's `max_concurrent_per_node`.
                    Rejected::Full(_) => {
                        metrics::record_trace_queue_rejected("full");
                        crate::errors::OrionError::unavailable(
                            crate::errors::Unavailable::AtCapacity,
                            format!(
                                "Trace queue is full ({} messages pending)",
                                self.queue.depth()
                            ),
                        )
                    }
                    // Not `AtCapacity`: a closed channel means the dispatcher is
                    // gone, which no amount of waiting fixes. The node reports it
                    // on `/health` as a dead background task, and this refusal
                    // sends no `Retry-After` rather than inviting a loop.
                    Rejected::Closed(_) => crate::errors::OrionError::unavailable(
                        crate::errors::Unavailable::QueueClosed,
                        "Trace queue is closed",
                    ),
                })
            }
        }
    }
}

/// Handle returned from `start_workers` to manage the worker lifecycle.
pub struct WorkerHandle {
    drain: DrainHandle<QueuedItem>,
    join_handle: tokio::task::JoinHandle<()>,
    shutdown_timeout_secs: u64,
}

impl WorkerHandle {
    /// Gracefully shut down the worker pool.
    ///
    /// Releases this handle's producer clone (the `TraceQueue` on `AppState`
    /// holds one too), so call this only after the HTTP server has stopped
    /// accepting new requests. The returned future resolves when all in-flight
    /// traces are complete — the dispatcher waits for its own spawned workers
    /// before exiting, which is why `DrainWitness::TasksExit` is the right
    /// witness here and a depth of zero is not.
    pub async fn shutdown(self) {
        let timeout = Duration::from_secs(self.shutdown_timeout_secs);
        match self
            .drain
            .drain(vec![self.join_handle], DrainWitness::TasksExit, timeout)
            .await
        {
            DrainOutcome::Drained => {}
            DrainOutcome::WorkerPanicked => {
                tracing::error!("Trace queue dispatcher panicked")
            }
            DrainOutcome::TimedOut { .. } => {
                tracing::warn!(
                    timeout_secs = self.shutdown_timeout_secs,
                    "Trace queue workers did not shut down within timeout, proceeding with exit"
                );
            }
        }
    }
}

/// What the worker pool needs from the running instance, beyond the scalar
/// settings it reads from `TraceQueueConfig`.
///
/// Named fields rather than a positional list: the two repositories are both
/// `Arc<dyn …>` and one of them is optional, so a transposition compiles.
pub struct WorkerDeps {
    pub runtime: Arc<crate::runtime::RuntimeHandle>,
    pub trace_repo: Arc<dyn TraceSink>,
    pub dlq_repo: Option<Arc<dyn TraceDlqRepository>>,
    pub persistence_queue: TracePersistenceQueue,
    pub global_trace_storage: crate::config::TraceStorageConfig,
    pub rollout_sticky_header: String,
}

/// Start the background worker pool and return a (TraceQueue, WorkerHandle) pair.
///
/// Scalar config parameters (workers, buffer_size, timeouts, limits) are read
/// from `config`. The `Arc` dependencies (runtime, repos) arrive in [`WorkerDeps`]
/// because they have independent lifetimes.
pub fn start_workers(
    tasks: &crate::runtime::TaskRegistry,
    config: &crate::config::TraceQueueConfig,
    deps: WorkerDeps,
) -> (TraceQueue, WorkerHandle) {
    let WorkerDeps {
        runtime,
        trace_repo,
        dlq_repo,
        persistence_queue,
        global_trace_storage,
        rollout_sticky_header,
    } = deps;
    let max_workers = config.workers;
    let buffer_size = config.buffer_size;
    let shutdown_timeout_secs = config.shutdown_timeout_secs;
    let max_queue_memory_bytes = config.max_queue_memory_bytes;

    let (bounded, mut receivers) =
        BoundedWorker::<QueuedItem>::new(1, buffer_size, metrics::set_trace_queue_depth);
    let rx = receivers.pop().expect("one shard was requested");
    let drain = bounded.drain_handle();
    let active_workers = Arc::new(AtomicUsize::new(0));
    let memory_bytes = Arc::new(AtomicUsize::new(0));

    metrics::set_trace_workers_total(max_workers as f64);

    let dispatcher_ctx = processing::DispatcherContext {
        max_workers,
        shutdown_timeout_secs,
        counters: processing::QueueCounters {
            active: active_workers,
            memory_bytes: memory_bytes.clone(),
        },
        processing: processing::ProcessingContext {
            runtime,
            trace_repo,
            dlq_repo,
            processing_timeout_ms: config.processing_timeout_ms,
            max_result_size_bytes: config.max_result_size_bytes,
            dlq_max_retries: config.dlq_max_retries,
            rollout_sticky_header: Arc::from(rollout_sticky_header.as_str()),
            persistence_queue,
            global_trace_storage,
        },
    };

    // Required: the dispatcher is the only consumer of the async trace queue,
    // so its death turns every `/async` submission into a 503 with nothing to
    // say why. The join stays with `WorkerHandle` because the drain is ordered
    // (see `bootstrap::TaskHandles`).
    let guard = tasks.guard("trace_dispatcher", crate::runtime::Criticality::Required);
    let handle = tokio::spawn(guard.run(processing::dispatcher_loop(rx, dispatcher_ctx)));

    let queue = TraceQueue {
        queue: bounded,
        memory_bytes,
        max_memory_bytes: max_queue_memory_bytes,
    };
    let worker_handle = WorkerHandle {
        drain,
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

    /// The async submission measures its message from the payload it already
    /// serialized for the trace row; that must be the size the counter would
    /// have counted, or admission and reservation would disagree with every
    /// other enqueue path.
    #[test]
    fn a_pre_serialized_payload_measures_what_the_counter_counts() {
        let payload = serde_json::json!({"a": [1, 2.5, "x"], "b": {"c": null}, "d": "é"});
        let metadata = serde_json::json!({"headers": {"x-y": "z"}});
        let json = serde_json::to_string(&payload).expect("serializes");
        assert_eq!(
            message_size(&payload, Some(&json), &metadata),
            message_size(&payload, None, &metadata)
        );
        assert_eq!(
            message_size(&payload, None, &metadata),
            json.len() + serde_json::to_string(&metadata).expect("serializes").len()
        );
    }

    #[tokio::test]
    async fn submit_rejects_when_buffer_is_full() {
        let (queue, _rx) = TraceQueue::new_for_test(1);

        queue.submit(test_message("t1")).await.expect("first fits");

        // Must resolve immediately with 503 rather than parking the caller
        // until a worker drains the buffer.
        let err =
            tokio::time::timeout(Duration::from_millis(250), queue.submit(test_message("t2")))
                .await
                .expect("submit must not block on a full queue")
                .expect_err("full queue must be rejected");

        assert!(
            matches!(err, crate::errors::OrionError::ServiceUnavailable { .. }),
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

        let (queue, _rx) = TraceQueue::new_for_test(1);
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
        let (queue, rx) = TraceQueue::new_for_test(1);
        drop(rx);

        let err = queue
            .submit(test_message("t1"))
            .await
            .expect_err("closed queue must be rejected");
        assert!(
            matches!(err, crate::errors::OrionError::ServiceUnavailable { .. }),
            "expected Queue error, got: {err:?}"
        );
    }

    /// The retention half of the trace store, and only that.
    ///
    /// The cleanup job touches only `TraceRetention`, so the double implements
    /// that and nothing else — no `unimplemented!()` bodies standing in for
    /// methods it never calls. Splitting the trait is what lets a double say
    /// what it is a double *of*.
    struct MockCleanupTraceRepo;

    #[async_trait::async_trait]
    impl TraceRetention for MockCleanupTraceRepo {
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
                    let repo: Arc<dyn TraceRetention> = Arc::new(MockCleanupTraceRepo);
                    let tasks = crate::runtime::TaskRegistry::new();
                    start_trace_cleanup(&tasks, 24, 1, repo, None);
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
                    tasks.shutdown(Duration::from_secs(5)).await;
                });
        });
        let out = handle.render();
        assert!(
            out.contains(r#"orion_job_last_success_timestamp_seconds{job="trace_cleanup"}"#),
            "a successful cleanup tick must stamp the job health gauge:\n{out}"
        );
    }
}
