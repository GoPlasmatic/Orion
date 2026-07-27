//! Background queue for trace persistence writes.
//!
//! Routes `store_completed`, `set_result`, and `update_status` calls off the
//! request path so the HTTP response doesn't block on a single-writer SQLite
//! INSERT. Two flavours of worker behind the same submission interface:
//!
//! - **`async` workers** drain one task at a time and issue one DB call per
//!   task. Lower mean latency per row, more transactions overall.
//! - **`batch` workers** accumulate tasks up to `batch_size` or
//!   `batch_flush_interval_ms` and commit them in a single transaction via
//!   the repository's `*_batch` methods. Much higher throughput on the
//!   single-writer DB backends.
//!
//! When the bounded mpsc is full, the submission path follows
//! [`AsyncOnOverflow`]: drop the task immediately (metric only) or block for
//! up to `overflow_block_timeout_ms` before dropping.
//!
//! [`AsyncOnOverflow`]: crate::config::AsyncOnOverflow

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::config::{AsyncOnOverflow, TraceStorageMode, TracingStorageConfig};
use crate::metrics;
use crate::storage::repositories::traces::{TraceCompletedRow, TraceRepository, TraceResultRow};

/// A unit of trace-persistence work to run on a background worker.
#[derive(Debug)]
pub enum TracePersistenceTask {
    /// Equivalent to `trace_repo.store_completed(...)`.
    StoreCompleted(TraceCompletedRow),
    /// Equivalent to `trace_repo.set_result(id, result_json, duration_ms, task_trace_json)`.
    SetResult(TraceResultRow),
    /// Equivalent to `trace_repo.update_status(id, status, error_message)`.
    UpdateStatus {
        id: String,
        status: String,
        error_message: Option<String>,
    },
}

/// Handle clients use to submit work. Cheap to clone — shares the underlying
/// senders. When `disabled` is true (mode = `Sync` or `Off`), `submit` is a
/// no-op so call sites can stay shape-uniform.
///
/// Q7: each worker owns its own receiver (one channel per worker) and
/// `submit` fans out round-robin. The previous shared `Arc<Mutex<Receiver>>`
/// serialized all workers behind one lock — which the batch worker held
/// across its flush-interval `timeout(recv)` — so `async_workers`/
/// `batch_workers` > 1 delivered no parallelism at all.
#[derive(Clone)]
pub struct TracePersistenceQueue {
    senders: Vec<mpsc::Sender<TracePersistenceTask>>,
    next: Arc<AtomicUsize>,
    pending: Arc<AtomicUsize>,
    overflow_policy: AsyncOnOverflow,
    overflow_block_timeout: Duration,
}

impl TracePersistenceQueue {
    /// Create a no-op queue (used by `Sync` and `Off` modes). Submits return
    /// `Ok(false)`.
    pub fn disabled() -> Self {
        Self {
            senders: Vec::new(),
            next: Arc::new(AtomicUsize::new(0)),
            pending: Arc::new(AtomicUsize::new(0)),
            overflow_policy: AsyncOnOverflow::Drop,
            overflow_block_timeout: Duration::ZERO,
        }
    }

    /// Submit a task. Returns `Ok(true)` when accepted, `Ok(false)` when
    /// dropped (queue full or queue disabled). Never errors — overflow is
    /// surfaced via `trace_dropped_total{reason="overflow"}`.
    pub async fn submit(&self, task: TracePersistenceTask) -> bool {
        if self.senders.is_empty() {
            return false;
        }
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        let send_result = match self.overflow_policy {
            AsyncOnOverflow::Drop => {
                // Round-robin, falling through to the other workers when the
                // preferred one is full — a stalled worker should not drop
                // tasks while its siblings sit idle.
                let mut task = task;
                let mut outcome = Err("full");
                for i in 0..self.senders.len() {
                    let sender = &self.senders[(start + i) % self.senders.len()];
                    match sender.try_send(task) {
                        Ok(()) => {
                            outcome = Ok(());
                            break;
                        }
                        Err(mpsc::error::TrySendError::Full(t))
                        | Err(mpsc::error::TrySendError::Closed(t)) => task = t,
                    }
                }
                outcome
            }
            AsyncOnOverflow::Block => {
                let sender = &self.senders[start % self.senders.len()];
                match tokio::time::timeout(self.overflow_block_timeout, sender.send(task)).await {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(_)) => Err("closed"),
                    Err(_) => Err("timeout"),
                }
            }
        };
        match send_result {
            Ok(()) => {
                let n = self.pending.fetch_add(1, Ordering::Relaxed) + 1;
                metrics::set_trace_persistence_queue_depth(n as f64);
                true
            }
            Err(_) => {
                metrics::record_trace_dropped("overflow");
                false
            }
        }
    }
}

/// Lifecycle handle. Drop the inner senders on shutdown and await drain.
pub struct PersistenceWorkerHandle {
    _senders: Vec<mpsc::Sender<TracePersistenceTask>>,
    join: Vec<tokio::task::JoinHandle<()>>,
    shutdown_timeout: Duration,
}

impl PersistenceWorkerHandle {
    pub fn noop() -> Self {
        Self {
            _senders: Vec::new(),
            join: Vec::new(),
            shutdown_timeout: Duration::ZERO,
        }
    }

    /// Drop the producer side and wait for workers to drain, bounded by
    /// `shutdown_timeout`.
    pub async fn shutdown(self) {
        drop(self._senders);
        if self.join.is_empty() {
            return;
        }
        let deadline = tokio::time::Instant::now() + self.shutdown_timeout;
        for handle in self.join {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                handle.abort();
                continue;
            }
            if tokio::time::timeout(remaining, handle).await.is_err() {
                tracing::warn!("Trace persistence worker did not finish within shutdown timeout");
            }
        }
    }
}

/// Start the persistence queue. Returns a no-op queue + handle when
/// `mode = Sync` or `mode = Off` (callers don't dispatch through the queue
/// in those modes).
pub fn start(
    config: &TracingStorageConfig,
    trace_repo: Arc<dyn TraceRepository>,
) -> (TracePersistenceQueue, PersistenceWorkerHandle) {
    let (worker_count, is_batch) = match config.mode {
        TraceStorageMode::Async => (config.async_workers.max(1), false),
        TraceStorageMode::Batch => (config.batch_workers.max(1), true),
        TraceStorageMode::Sync | TraceStorageMode::Off => {
            return (
                TracePersistenceQueue::disabled(),
                PersistenceWorkerHandle::noop(),
            );
        }
    };

    // One channel per worker (Q7); `max_pending` stays the total bound by
    // splitting the capacity across workers.
    let per_worker_capacity = (config.max_pending.max(1) / worker_count).max(1);
    let pending = Arc::new(AtomicUsize::new(0));

    let mut senders = Vec::with_capacity(worker_count);
    let mut join = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let (tx, rx) = mpsc::channel::<TracePersistenceTask>(per_worker_capacity);
        senders.push(tx);
        let pending = pending.clone();
        let trace_repo = trace_repo.clone();
        let batch_size = config.batch_size.max(1);
        let flush_interval = Duration::from_millis(config.batch_flush_interval_ms.max(1));
        join.push(tokio::spawn(async move {
            if is_batch {
                run_batch_worker(rx, pending, trace_repo, batch_size, flush_interval).await;
            } else {
                run_async_worker(rx, pending, trace_repo).await;
            }
        }));
    }

    let queue = TracePersistenceQueue {
        senders: senders.clone(),
        next: Arc::new(AtomicUsize::new(0)),
        pending,
        overflow_policy: config.async_on_overflow,
        overflow_block_timeout: Duration::from_millis(config.overflow_block_timeout_ms),
    };
    let handle = PersistenceWorkerHandle {
        _senders: senders,
        join,
        shutdown_timeout: Duration::from_secs(30),
    };
    (queue, handle)
}

async fn run_async_worker(
    mut rx: mpsc::Receiver<TracePersistenceTask>,
    pending: Arc<AtomicUsize>,
    trace_repo: Arc<dyn TraceRepository>,
) {
    while let Some(task) = rx.recv().await {
        let n = pending.fetch_sub(1, Ordering::Relaxed).saturating_sub(1);
        metrics::set_trace_persistence_queue_depth(n as f64);
        dispatch_one(&trace_repo, task).await;
    }
}

async fn run_batch_worker(
    mut rx: mpsc::Receiver<TracePersistenceTask>,
    pending: Arc<AtomicUsize>,
    trace_repo: Arc<dyn TraceRepository>,
    batch_size: usize,
    flush_interval: Duration,
) {
    let mut completed: Vec<TraceCompletedRow> = Vec::new();
    let mut results: Vec<TraceResultRow> = Vec::new();
    let mut deadline = Instant::now() + flush_interval;

    loop {
        let now = Instant::now();
        let until = deadline.saturating_duration_since(now);
        let recv = tokio::time::timeout(until, rx.recv()).await;

        match recv {
            Ok(Some(task)) => {
                let n = pending.fetch_sub(1, Ordering::Relaxed).saturating_sub(1);
                metrics::set_trace_persistence_queue_depth(n as f64);
                match task {
                    TracePersistenceTask::StoreCompleted(row) => completed.push(row),
                    TracePersistenceTask::SetResult(row) => results.push(row),
                    // UpdateStatus is rare and per-row by nature — flush directly.
                    TracePersistenceTask::UpdateStatus {
                        id,
                        status,
                        error_message,
                    } => {
                        if let Err(e) = trace_repo
                            .update_status(&id, &status, error_message.as_deref())
                            .await
                        {
                            tracing::warn!(error = %e, "trace_persistence: update_status failed");
                        }
                    }
                }
                if completed.len() >= batch_size || results.len() >= batch_size {
                    flush_batches(&trace_repo, &mut completed, &mut results).await;
                    deadline = Instant::now() + flush_interval;
                }
            }
            Ok(None) => {
                // Channel closed: drain remaining batches and exit.
                flush_batches(&trace_repo, &mut completed, &mut results).await;
                return;
            }
            Err(_) => {
                // Deadline elapsed: flush whatever we have.
                flush_batches(&trace_repo, &mut completed, &mut results).await;
                deadline = Instant::now() + flush_interval;
            }
        }
    }
}

/// Backoff schedule for failed persistence writes (Q6): a transient DB blip
/// should cost a short stall on this worker, not silently discarded traces.
/// After the last attempt the write is dropped — with the failure counter
/// and a warn so the loss is visible.
const WRITE_RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(50), Duration::from_millis(250)];

/// Run `op` with the bounded retry schedule. Returns `Err` only after every
/// attempt failed.
async fn with_write_retries<F, Fut>(mut op: F) -> Result<(), crate::errors::OrionError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), crate::errors::OrionError>>,
{
    let mut last_err = None;
    for (attempt, delay) in std::iter::once(Duration::ZERO)
        .chain(WRITE_RETRY_DELAYS)
        .enumerate()
    {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
            tracing::debug!(attempt, "trace_persistence: retrying failed write");
        }
        match op().await {
            Ok(()) => return Ok(()),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.expect("at least one attempt always runs"))
}

async fn dispatch_one(trace_repo: &Arc<dyn TraceRepository>, task: TracePersistenceTask) {
    let result = with_write_retries(|| async {
        match &task {
            TracePersistenceTask::StoreCompleted(row) => trace_repo
                .store_completed(
                    &row.channel,
                    row.channel_id.as_deref(),
                    &row.mode,
                    row.input_json.as_deref(),
                    &row.result_json,
                    row.duration_ms,
                    row.task_trace_json.as_deref(),
                )
                .await
                .map(|_| ()),
            TracePersistenceTask::SetResult(row) => {
                trace_repo
                    .set_result(
                        &row.id,
                        &row.result_json,
                        row.duration_ms,
                        row.task_trace_json.as_deref(),
                    )
                    .await
            }
            TracePersistenceTask::UpdateStatus {
                id,
                status,
                error_message,
            } => trace_repo
                .update_status(id, status, error_message.as_deref())
                .await
                .map(|_| ()),
        }
    })
    .await;
    if let Err(e) = result {
        crate::metrics::record_trace_persistence_failure();
        tracing::warn!(error = %e, "trace_persistence: write failed after retries, dropping");
    }
}

async fn flush_batches(
    trace_repo: &Arc<dyn TraceRepository>,
    completed: &mut Vec<TraceCompletedRow>,
    results: &mut Vec<TraceResultRow>,
) {
    // Each arm retries the whole batch (Q6) and clears it only afterwards —
    // success or exhausted retries — so the buffer cannot grow unbounded
    // while the DB is down. Exhaustion is a counted, logged drop.
    if !completed.is_empty() {
        if let Err(e) = with_write_retries(|| async {
            trace_repo
                .store_completed_batch(completed)
                .await
                .map(|_| ())
        })
        .await
        {
            crate::metrics::record_trace_persistence_failure();
            tracing::warn!(
                error = %e,
                dropped = completed.len(),
                "trace_persistence: store_completed_batch failed after retries, dropping"
            );
        }
        completed.clear();
    }
    if !results.is_empty() {
        if let Err(e) =
            with_write_retries(|| async { trace_repo.set_result_batch(results).await }).await
        {
            crate::metrics::record_trace_persistence_failure();
            tracing::warn!(
                error = %e,
                dropped = results.len(),
                "trace_persistence: set_result_batch failed after retries, dropping"
            );
        }
        results.clear();
    }
}
