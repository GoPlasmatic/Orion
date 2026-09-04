//! Queue durability (proposal workstream Q).
//!
//! Q5: a transient DB error on the "mark running" write must route the
//! message to the DLQ, not drop it with the trace stuck `pending`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use orion::errors::OrionError;
use orion::storage::models::Trace;
use orion::storage::repositories::trace_dlq::{SqlTraceDlqRepository, TraceDlqFilter};
use orion::storage::repositories::traces::{
    SqlTraceRepository, TraceCompletedRef, TraceCompletedRow, TraceReader, TraceResultRow,
    TraceSink,
};

/// Delegating wrapper that fails the first `update_status(_, "running", _)`
/// call, simulating a DB blip at the worst moment.
struct FailFirstRunningWrite {
    inner: Arc<SqlTraceRepository>,
    armed: AtomicBool,
}

#[async_trait]
impl TraceSink for FailFirstRunningWrite {
    async fn create_pending(
        &self,
        channel: &str,
        channel_id: Option<&str>,
        mode: &str,
        input_json: Option<&str>,
        access_token_hash: Option<&str>,
    ) -> Result<Trace, OrionError> {
        self.inner
            .create_pending(channel, channel_id, mode, input_json, access_token_hash)
            .await
    }
    async fn update_status(
        &self,
        id: &str,
        status: &str,
        error_message: Option<&str>,
    ) -> Result<Trace, OrionError> {
        if status == "running" && self.armed.swap(false, Ordering::SeqCst) {
            return Err(OrionError::Storage(sqlx::Error::PoolTimedOut));
        }
        self.inner.update_status(id, status, error_message).await
    }
    async fn set_result(
        &self,
        id: &str,
        result_json: &str,
        duration_ms: f64,
        task_trace_json: Option<&str>,
    ) -> Result<(), OrionError> {
        self.inner
            .set_result(id, result_json, duration_ms, task_trace_json)
            .await
    }
    async fn store_completed(&self, row: TraceCompletedRef<'_>) -> Result<String, OrionError> {
        self.inner.store_completed(row).await
    }
    async fn store_completed_batch(
        &self,
        rows: &[TraceCompletedRow],
    ) -> Result<Vec<String>, OrionError> {
        self.inner.store_completed_batch(rows).await
    }
    async fn set_result_batch(&self, rows: &[TraceResultRow]) -> Result<(), OrionError> {
        self.inner.set_result_batch(rows).await
    }
}

#[tokio::test]
async fn failed_running_write_routes_message_to_dlq() {
    sqlx::any::install_default_drivers();
    let pool = orion::storage::init_pool(&orion::config::StorageConfig {
        url: "sqlite::memory:".to_string(),
        max_connections: 5,
        ..Default::default()
    })
    .await
    .unwrap();

    let sql_repo = Arc::new(SqlTraceRepository::new(pool.clone()));
    let trace_repo: Arc<dyn TraceSink> = Arc::new(FailFirstRunningWrite {
        inner: sql_repo.clone(),
        armed: AtomicBool::new(true),
    });
    let dlq_repo = Arc::new(SqlTraceDlqRepository::new(pool.clone()));

    let runtime = crate::common::empty_runtime();
    // Pinned rather than defaulted: this test asserts on the trace row right
    // after processing, so the write has to land inline.
    let tracing_storage = orion::config::TraceStorageConfig {
        mode: orion::config::TraceStorageMode::Sync,
        ..Default::default()
    };
    let tasks = orion::runtime::TaskRegistry::new();
    let (persistence_queue, _persistence_handle) =
        orion::queue::trace_persistence::start(&tasks, &tracing_storage, trace_repo.clone());
    let queue_config = orion::config::TraceQueueConfig {
        workers: 1,
        buffer_size: 10,
        ..Default::default()
    };
    let (trace_queue, _worker_handle) = orion::queue::start_workers(
        &tasks,
        &queue_config,
        orion::queue::WorkerDeps {
            runtime,
            trace_repo: trace_repo.clone(),
            dlq_repo: Some(dlq_repo.clone()
                as Arc<dyn orion::storage::repositories::trace_dlq::TraceDlqRepository>),
            persistence_queue,
            global_trace_storage: tracing_storage,
            rollout_sticky_header: String::new(),
        },
    );

    // Seed the pending row exactly as the async submit path does, then queue.
    let trace = trace_repo
        .create_pending("q5-channel", None, "async", Some("{\"n\":1}"), None)
        .await
        .unwrap();
    trace_queue
        .submit(orion::queue::QueueMessage {
            trace_id: trace.id.clone(),
            channel: "q5-channel".to_string(),
            payload: serde_json::json!({"n": 1}),
            metadata: serde_json::json!({}),
            trace_headers: Default::default(),
            profile_requested: false,
            backpressure_permit: None,
        })
        .await
        .unwrap();

    // The failed running-write must produce a DLQ row for this trace…
    let mut dlq_rows = Vec::new();
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        use orion::storage::repositories::trace_dlq::TraceDlqRepository;
        dlq_rows = dlq_repo
            .list_paginated(&TraceDlqFilter::default())
            .await
            .unwrap()
            .data;
        if !dlq_rows.is_empty() {
            break;
        }
    }
    let row = dlq_rows
        .iter()
        .find(|r| r.trace_id == trace.id)
        .unwrap_or_else(|| panic!("expected a DLQ row for {} (Q5), got {dlq_rows:?}", trace.id));
    assert!(
        row.error_message.contains("Failed to mark trace running"),
        "DLQ row must carry the status-write error, got {:?}",
        row.error_message
    );

    // …and the trace row must not be stuck `pending` forever.
    let stored = sql_repo.get_by_id(&trace.id).await.unwrap();
    assert_eq!(
        stored.status, "failed",
        "trace must leave `pending` once its message is routed to the DLQ"
    );
}

/// Delegating wrapper that fails every `set_result`, exercising the
/// 3-attempt inline retry and its marked-failed fallback.
struct FailAllResultWrites {
    inner: Arc<SqlTraceRepository>,
}

#[async_trait]
impl TraceSink for FailAllResultWrites {
    async fn create_pending(
        &self,
        channel: &str,
        channel_id: Option<&str>,
        mode: &str,
        input_json: Option<&str>,
        access_token_hash: Option<&str>,
    ) -> Result<Trace, OrionError> {
        self.inner
            .create_pending(channel, channel_id, mode, input_json, access_token_hash)
            .await
    }
    async fn update_status(
        &self,
        id: &str,
        status: &str,
        error_message: Option<&str>,
    ) -> Result<Trace, OrionError> {
        self.inner.update_status(id, status, error_message).await
    }
    async fn set_result(
        &self,
        _id: &str,
        _result_json: &str,
        _duration_ms: f64,
        _task_trace_json: Option<&str>,
    ) -> Result<(), OrionError> {
        Err(OrionError::Storage(sqlx::Error::PoolTimedOut))
    }
    async fn store_completed(&self, row: TraceCompletedRef<'_>) -> Result<String, OrionError> {
        self.inner.store_completed(row).await
    }
    async fn store_completed_batch(
        &self,
        rows: &[TraceCompletedRow],
    ) -> Result<Vec<String>, OrionError> {
        self.inner.store_completed_batch(rows).await
    }
    async fn set_result_batch(&self, rows: &[TraceResultRow]) -> Result<(), OrionError> {
        self.inner.set_result_batch(rows).await
    }
}

/// Shared harness for the persist_success failure-arm tests: start one
/// worker over `trace_repo` with the given queue config, submit one async
/// message, and poll the trace row until it leaves pending/running.
///
/// `reader` is separate from `trace_repo` because they are separate roles: the
/// workers write through the (possibly failure-injecting) sink, and the
/// assertion reads the row back from the real store behind it.
async fn run_one_message_to_terminal_status(
    trace_repo: Arc<dyn TraceSink>,
    reader: &dyn TraceReader,
    queue_config: orion::config::TraceQueueConfig,
) -> Trace {
    let runtime = crate::common::empty_runtime();
    // Pinned rather than defaulted: this test asserts on the trace row right
    // after processing, so the write has to land inline.
    let tracing_storage = orion::config::TraceStorageConfig {
        mode: orion::config::TraceStorageMode::Sync,
        ..Default::default()
    };
    let tasks = orion::runtime::TaskRegistry::new();
    let (persistence_queue, _persistence_handle) =
        orion::queue::trace_persistence::start(&tasks, &tracing_storage, trace_repo.clone());
    let (trace_queue, _worker_handle) = orion::queue::start_workers(
        &tasks,
        &queue_config,
        orion::queue::WorkerDeps {
            runtime,
            trace_repo: trace_repo.clone(),
            dlq_repo: None,
            persistence_queue,
            global_trace_storage: tracing_storage,
            rollout_sticky_header: String::new(),
        },
    );

    let trace = trace_repo
        .create_pending("arm-channel", None, "async", Some("{}"), None)
        .await
        .unwrap();
    trace_queue
        .submit(orion::queue::QueueMessage {
            trace_id: trace.id.clone(),
            channel: "arm-channel".to_string(),
            payload: serde_json::json!({"n": 1}),
            metadata: serde_json::json!({}),
            trace_headers: Default::default(),
            profile_requested: false,
            backpressure_permit: None,
        })
        .await
        .unwrap();

    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let stored = reader.get_by_id(&trace.id).await.unwrap();
        if stored.status != "pending" && stored.status != "running" {
            return stored;
        }
    }
    panic!("trace never reached a terminal status");
}

/// The 3-attempt inline set_result retry must give up and mark the trace
/// FAILED with the retries message — not leave it running or lie completed.
#[tokio::test]
async fn persistent_set_result_failure_marks_trace_failed_after_retries() {
    sqlx::any::install_default_drivers();
    let pool = orion::storage::init_pool(&orion::config::StorageConfig {
        url: "sqlite::memory:".to_string(),
        max_connections: 5,
        ..Default::default()
    })
    .await
    .unwrap();
    let sql_repo = Arc::new(SqlTraceRepository::new(pool));
    let trace_repo: Arc<dyn TraceSink> = Arc::new(FailAllResultWrites {
        inner: sql_repo.clone(),
    });

    let stored = run_one_message_to_terminal_status(
        trace_repo,
        sql_repo.as_ref(),
        orion::config::TraceQueueConfig {
            workers: 1,
            buffer_size: 10,
            ..Default::default()
        },
    )
    .await;

    assert_eq!(stored.status, "failed");
    assert!(
        stored
            .error_message
            .as_deref()
            .unwrap_or("")
            .contains("Result persistence failed after retries"),
        "expected the retry-exhaustion message, got {:?}",
        stored.error_message
    );
}

/// A result over `queue.max_result_size_bytes` must fail the trace with the
/// size message instead of storing an oversized row (or lying completed).
#[tokio::test]
async fn oversized_result_marks_trace_failed_with_size_message() {
    sqlx::any::install_default_drivers();
    let pool = orion::storage::init_pool(&orion::config::StorageConfig {
        url: "sqlite::memory:".to_string(),
        max_connections: 5,
        ..Default::default()
    })
    .await
    .unwrap();
    // No failure injection here — the same store is both the sink and the
    // reader.
    let sql_repo = Arc::new(SqlTraceRepository::new(pool));
    let trace_repo: Arc<dyn TraceSink> = sql_repo.clone();

    let stored = run_one_message_to_terminal_status(
        trace_repo,
        sql_repo.as_ref(),
        orion::config::TraceQueueConfig {
            workers: 1,
            buffer_size: 10,
            // Any serialized result envelope exceeds 8 bytes.
            max_result_size_bytes: 8,
            ..Default::default()
        },
    )
    .await;

    assert_eq!(stored.status, "failed");
    assert!(
        stored
            .error_message
            .as_deref()
            .unwrap_or("")
            .contains("exceeds limit of 8 bytes"),
        "expected the size-cap message, got {:?}",
        stored.error_message
    );
    assert!(
        stored.result_json.is_none(),
        "the oversized result must not be stored"
    );
}

// ============================================================
// Q7: batch/async workers must actually run in parallel
// ============================================================

/// Records the maximum number of concurrently in-flight `update_status`
/// calls. With the old shared `Arc<Mutex<Receiver>>`, N workers serialized
/// behind one lock and this could never exceed 1.
struct ConcurrencyProbeRepo {
    current: Arc<std::sync::atomic::AtomicUsize>,
    max_seen: Arc<std::sync::atomic::AtomicUsize>,
}

fn fake_trace(id: &str) -> Trace {
    let now = chrono::Utc::now().naive_utc();
    Trace {
        id: id.to_string(),
        channel: "probe".to_string(),
        channel_id: None,
        mode: "async".to_string(),
        status: "running".to_string(),
        input_json: None,
        result_json: None,
        error_message: None,
        duration_ms: None,
        started_at: None,
        completed_at: None,
        created_at: now,
        updated_at: now,
        task_trace_json: None,
        access_token_hash: None,
    }
}

#[async_trait]
impl TraceSink for ConcurrencyProbeRepo {
    async fn create_pending(
        &self,
        _channel: &str,
        _channel_id: Option<&str>,
        _mode: &str,
        _input_json: Option<&str>,
        _access_token_hash: Option<&str>,
    ) -> Result<Trace, OrionError> {
        unimplemented!()
    }
    async fn update_status(
        &self,
        id: &str,
        _status: &str,
        _error_message: Option<&str>,
    ) -> Result<Trace, OrionError> {
        let now = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_seen.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        self.current.fetch_sub(1, Ordering::SeqCst);
        Ok(fake_trace(id))
    }
    async fn set_result(
        &self,
        _id: &str,
        _result_json: &str,
        _duration_ms: f64,
        _task_trace_json: Option<&str>,
    ) -> Result<(), OrionError> {
        unimplemented!()
    }
    async fn store_completed(&self, _row: TraceCompletedRef<'_>) -> Result<String, OrionError> {
        unimplemented!()
    }
}

#[tokio::test]
async fn persistence_workers_run_in_parallel() {
    let current = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let max_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let repo: Arc<dyn TraceSink> = Arc::new(ConcurrencyProbeRepo {
        current: current.clone(),
        max_seen: max_seen.clone(),
    });

    let config = orion::config::TraceStorageConfig {
        mode: orion::config::TraceStorageMode::Async,
        async_workers: 2,
        ..Default::default()
    };
    let (queue, handle) =
        orion::queue::trace_persistence::start(&orion::runtime::TaskRegistry::new(), &config, repo);

    for i in 0..2 {
        assert!(
            queue
                .submit(orion::queue::TracePersistenceTask::UpdateStatus {
                    id: format!("t{i}"),
                    status: "completed".to_string(),
                    error_message: None,
                })
                .await
        );
    }
    // Drop the queue's sender clones so the workers see channel-closed and
    // drain instead of idling until the shutdown timeout.
    drop(queue);
    handle.shutdown().await;

    assert_eq!(
        max_seen.load(Ordering::SeqCst),
        2,
        "2 async_workers must process 2 queued writes concurrently (Q7)"
    );
}

// ============================================================
// Q6: transient persistence-write failures retry before dropping
// ============================================================

/// Fails the first `update_status`, succeeds afterwards, counting calls.
struct FlakyUpdateStatusRepo {
    fails_remaining: Arc<std::sync::atomic::AtomicUsize>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl TraceSink for FlakyUpdateStatusRepo {
    async fn create_pending(
        &self,
        _channel: &str,
        _channel_id: Option<&str>,
        _mode: &str,
        _input_json: Option<&str>,
        _access_token_hash: Option<&str>,
    ) -> Result<Trace, OrionError> {
        unimplemented!()
    }
    async fn update_status(
        &self,
        id: &str,
        _status: &str,
        _error_message: Option<&str>,
    ) -> Result<Trace, OrionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self
            .fails_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(OrionError::Storage(sqlx::Error::PoolTimedOut));
        }
        Ok(fake_trace(id))
    }
    async fn set_result(
        &self,
        _id: &str,
        _result_json: &str,
        _duration_ms: f64,
        _task_trace_json: Option<&str>,
    ) -> Result<(), OrionError> {
        unimplemented!()
    }
    async fn store_completed(&self, _row: TraceCompletedRef<'_>) -> Result<String, OrionError> {
        unimplemented!()
    }
}

#[tokio::test]
async fn transient_persistence_failure_is_retried_not_dropped() {
    let fails_remaining = Arc::new(std::sync::atomic::AtomicUsize::new(1));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let repo: Arc<dyn TraceSink> = Arc::new(FlakyUpdateStatusRepo {
        fails_remaining: fails_remaining.clone(),
        calls: calls.clone(),
    });

    let config = orion::config::TraceStorageConfig {
        mode: orion::config::TraceStorageMode::Async,
        async_workers: 1,
        ..Default::default()
    };
    let (queue, handle) =
        orion::queue::trace_persistence::start(&orion::runtime::TaskRegistry::new(), &config, repo);
    assert!(
        queue
            .submit(orion::queue::TracePersistenceTask::UpdateStatus {
                id: "q6".to_string(),
                status: "completed".to_string(),
                error_message: None,
            })
            .await
    );
    drop(queue);
    handle.shutdown().await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "one failure then one successful retry (Q6) — the write must not be dropped"
    );
    assert_eq!(fails_remaining.load(Ordering::SeqCst), 0);
}

// ============================================================
// Q11: a burst commits as one transaction, not one per row
// ============================================================

/// Records the row count of every `store_completed_batch` call.
struct BatchSizeProbeRepo {
    flushes: Arc<std::sync::Mutex<Vec<usize>>>,
}

#[async_trait]
impl TraceSink for BatchSizeProbeRepo {
    async fn create_pending(
        &self,
        _channel: &str,
        _channel_id: Option<&str>,
        _mode: &str,
        _input_json: Option<&str>,
        _access_token_hash: Option<&str>,
    ) -> Result<Trace, OrionError> {
        unimplemented!()
    }
    async fn update_status(
        &self,
        _id: &str,
        _status: &str,
        _error_message: Option<&str>,
    ) -> Result<Trace, OrionError> {
        unimplemented!()
    }
    async fn set_result(
        &self,
        _id: &str,
        _result_json: &str,
        _duration_ms: f64,
        _task_trace_json: Option<&str>,
    ) -> Result<(), OrionError> {
        unimplemented!()
    }
    async fn store_completed(&self, _row: TraceCompletedRef<'_>) -> Result<String, OrionError> {
        // The whole point of batch mode: never the per-row path.
        unimplemented!("batch mode must not fall back to per-row writes")
    }
    async fn store_completed_batch(
        &self,
        rows: &[TraceCompletedRow],
    ) -> Result<Vec<String>, OrionError> {
        self.flushes.lock().unwrap().push(rows.len());
        Ok(rows.iter().map(|_| String::new()).collect())
    }
}

/// A burst that fits inside `batch_size` must reach the DB as **one** INSERT.
///
/// Q11: transaction count is what sets the drain rate — the same rows cost 26k
/// rows/s at 100 per flush and 45k rows/s at 1000 — so "rows queued together
/// are committed together" is the property the throughput rests on, and it was
/// resting on it untested. A regression that split a batch into per-row writes
/// would not fail any other test; it would just quietly drain 10x slower and
/// start dropping traces under load.
///
/// `batch_flush_interval_ms` is set far above the test's runtime so the only
/// flush trigger is the channel closing, which makes the count deterministic
/// rather than a race against the timer.
#[tokio::test]
async fn a_burst_of_traces_commits_as_a_single_batch() {
    const BURST: usize = 250;

    let flushes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let repo: Arc<dyn TraceSink> = Arc::new(BatchSizeProbeRepo {
        flushes: flushes.clone(),
    });

    let config = orion::config::TraceStorageConfig {
        mode: orion::config::TraceStorageMode::Batch,
        batch_workers: 1,
        batch_size: 1000,
        batch_flush_interval_ms: 600_000,
        ..Default::default()
    };
    let (queue, handle) =
        orion::queue::trace_persistence::start(&orion::runtime::TaskRegistry::new(), &config, repo);

    for i in 0..BURST {
        assert!(
            queue
                .submit(orion::queue::TracePersistenceTask::StoreCompleted(
                    TraceCompletedRow {
                        channel: "probe".to_string(),
                        channel_id: None,
                        mode: "sync".to_string(),
                        input_json: None,
                        result_json: format!("{{\"n\":{i}}}"),
                        duration_ms: 1.0,
                        task_trace_json: None,
                    }
                ))
                .await,
            "submit {i} must be accepted: the burst fits in max_pending"
        );
    }
    drop(queue);
    handle.shutdown().await;

    let flushes = flushes.lock().unwrap().clone();
    assert_eq!(
        flushes.iter().sum::<usize>(),
        BURST,
        "every queued trace must be persisted, not just the ones that fit a chunk"
    );
    assert_eq!(
        flushes,
        vec![BURST],
        "a burst under batch_size must commit in one transaction (Q11)"
    );
}

// ============================================================
// Q6: a panicking trace must not permanently shrink the queue
// ============================================================

/// A task handler that panics instead of returning an error.
///
/// `AlwaysFail` in `trace_dlq_poison_test` covers the *error* path, which the
/// DLQ already handles. This is the other one: a handler that unwinds, which
/// is what any `expect`, slice index or arithmetic overflow inside a handler
/// or the engine does.
struct AlwaysPanic;

#[async_trait]
impl dataflow_rs::engine::functions::AsyncFunctionHandler for AlwaysPanic {
    type Input = serde_json::Value;

    async fn execute(
        &self,
        _ctx: &mut dataflow_rs::engine::task_context::TaskContext<'_>,
        _input: &Self::Input,
    ) -> dataflow_rs::Result<dataflow_rs::engine::task_outcome::TaskOutcome> {
        panic!("a handler panicked mid-trace");
    }
}

fn panicking_runtime() -> Arc<orion::runtime::RuntimeHandle> {
    let workflow = dataflow_rs::Workflow::from_json(
        r#"{
            "id": "panic-wf",
            "name": "Panic",
            "priority": 0,
            "channel": "panic-ch",
            "tasks": [
                {"id": "t1", "name": "Boom", "function": {"name": "always_panic", "input": {}}}
            ]
        }"#,
    )
    .expect("panic workflow parses");

    let engine = dataflow_rs::Engine::builder()
        .with_workflow(workflow)
        .register("always_panic", AlwaysPanic)
        .build()
        .expect("engine builds");

    Arc::new(orion::runtime::RuntimeHandle::new(
        Arc::new(engine),
        Arc::new(orion::channel::ChannelSnapshot::empty()),
        orion::engine::FunctionRegistry::builtin().clone(),
    ))
}

/// Q6: the queue's memory reservation survives a panicking trace.
///
/// `max_queue_memory_bytes` is enforced against a running total that the
/// dispatcher decrements when a trace finishes. Those decrements used to run
/// *after* `process_trace(...).await` returned, so a panic skipped them and the
/// bytes stayed reserved for work that had already ended. The counter is the
/// admission authority, so the damage is cumulative and permanent: enough
/// panics and every submission answers 503 while the queue sits empty.
///
/// The ceiling here fits one payload and not two, which is what makes the
/// second submission the assertion. It is retried because releasing the
/// reservation and accepting the next message are on different tasks — with
/// the leak it never succeeds, with the fix it succeeds almost at once.
///
/// The panic backtrace this prints to stderr is the test doing its job.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicking_trace_does_not_permanently_consume_queue_memory() {
    // One `QueueMessage`'s worth of accounting, used both to size the ceiling
    // and to submit, so the two cannot drift apart.
    static PAYLOAD: std::sync::LazyLock<serde_json::Value> =
        std::sync::LazyLock::new(|| serde_json::json!({ "n": 1 }));
    static METADATA: std::sync::LazyLock<serde_json::Value> =
        std::sync::LazyLock::new(|| serde_json::json!({}));

    sqlx::any::install_default_drivers();
    let pool = orion::storage::init_pool(&orion::config::StorageConfig {
        url: "sqlite::memory:".to_string(),
        max_connections: 5,
        ..Default::default()
    })
    .await
    .unwrap();

    let trace_repo: Arc<dyn TraceSink> = Arc::new(SqlTraceRepository::new(pool.clone()));
    let tasks = orion::runtime::TaskRegistry::new();
    let trace_storage = orion::config::TraceStorageConfig::default();
    let (persistence_queue, _persistence_handle) =
        orion::queue::trace_persistence::start(&tasks, &trace_storage, trace_repo.clone());

    // The ceiling is derived from the payload rather than guessed, and admits
    // exactly one message: a second is refused while the first is in flight,
    // and a first whose reservation is *stranded* refuses it forever. A
    // round-number ceiling silently stops discriminating — at 100 bytes this
    // 9-byte payload can leak eleven times before the test notices.
    let reservation = PAYLOAD.to_string().len() + METADATA.to_string().len();
    let (trace_queue, _worker_handle) = orion::queue::start_workers(
        &tasks,
        &orion::config::TraceQueueConfig {
            workers: 2,
            buffer_size: 10,
            max_queue_memory_bytes: reservation + 1,
            ..Default::default()
        },
        orion::queue::WorkerDeps {
            runtime: panicking_runtime(),
            trace_repo: trace_repo.clone(),
            dlq_repo: None,
            persistence_queue,
            global_trace_storage: trace_storage,
            rollout_sticky_header: String::new(),
        },
    );

    // The row has to exist: `process_trace` marks it `running` before it
    // dispatches, and returns early if that write fails — which is how an
    // earlier version of this test passed against the bug it was written for.
    let submit = || {
        let queue = trace_queue.clone();
        let repo = trace_repo.clone();
        async move {
            let trace = repo
                .create_pending("panic-ch", None, "async", Some("{}"), None)
                .await
                .expect("pending row");
            queue
                .submit(orion::queue::QueueMessage {
                    trace_id: trace.id,
                    channel: "panic-ch".to_string(),
                    payload: PAYLOAD.clone(),
                    metadata: METADATA.clone(),
                    trace_headers: Default::default(),
                    profile_requested: false,
                    backpressure_permit: None,
                })
                .await
        }
    };

    submit().await.expect("the first message fits");

    // Once the panicking trace has unwound, its reservation is free again.
    let mut accepted = false;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        if submit().await.is_ok() {
            accepted = true;
            break;
        }
    }
    assert!(
        accepted,
        "a panicking trace stranded its memory reservation: the queue is empty \
         but still refusing submissions at its ceiling"
    );
}
