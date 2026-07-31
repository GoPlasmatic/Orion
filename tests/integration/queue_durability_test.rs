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
    SqlTraceRepository, TraceCompletedRow, TraceFilter, TracePage, TraceRepository, TraceResultRow,
};
use tokio::sync::RwLock;

/// Delegating wrapper that fails the first `update_status(_, "running", _)`
/// call, simulating a DB blip at the worst moment.
struct FailFirstRunningWrite {
    inner: Arc<SqlTraceRepository>,
    armed: AtomicBool,
}

#[async_trait]
impl TraceRepository for FailFirstRunningWrite {
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
    async fn get_by_id(&self, id: &str) -> Result<Trace, OrionError> {
        self.inner.get_by_id(id).await
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
    async fn store_completed(
        &self,
        channel: &str,
        channel_id: Option<&str>,
        mode: &str,
        input_json: Option<&str>,
        result_json: &str,
        duration_ms: f64,
        task_trace_json: Option<&str>,
    ) -> Result<String, OrionError> {
        self.inner
            .store_completed(
                channel,
                channel_id,
                mode,
                input_json,
                result_json,
                duration_ms,
                task_trace_json,
            )
            .await
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
    async fn list_paginated(&self, filter: &TraceFilter) -> Result<TracePage, OrionError> {
        self.inner.list_paginated(filter).await
    }
    async fn delete_older_than(&self, hours: u64) -> Result<u64, OrionError> {
        self.inner.delete_older_than(hours).await
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
    let trace_repo: Arc<dyn TraceRepository> = Arc::new(FailFirstRunningWrite {
        inner: sql_repo.clone(),
        armed: AtomicBool::new(true),
    });
    let dlq_repo = Arc::new(SqlTraceDlqRepository::new(pool.clone()));

    let engine = Arc::new(RwLock::new(Arc::new(
        dataflow_rs::Engine::builder().build().unwrap(),
    )));
    let channel_registry = Arc::new(orion::channel::ChannelRegistry::new());
    // Pinned rather than defaulted: this test asserts on the trace row right
    // after processing, so the write has to land inline.
    let tracing_storage = orion::config::TraceStorageConfig {
        mode: orion::config::TraceStorageMode::Sync,
        ..Default::default()
    };
    let (persistence_queue, _persistence_handle) =
        orion::queue::trace_persistence::start(&tracing_storage, trace_repo.clone());
    let queue_config = orion::config::TraceQueueConfig {
        workers: 1,
        buffer_size: 10,
        ..Default::default()
    };
    let (trace_queue, _worker_handle) = orion::queue::start_workers(
        &queue_config,
        engine,
        trace_repo.clone(),
        Some(dlq_repo.clone()
            as Arc<dyn orion::storage::repositories::trace_dlq::TraceDlqRepository>),
        channel_registry,
        persistence_queue,
        tracing_storage,
        String::new(),
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
    let stored = trace_repo.get_by_id(&trace.id).await.unwrap();
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
impl TraceRepository for FailAllResultWrites {
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
    async fn get_by_id(&self, id: &str) -> Result<Trace, OrionError> {
        self.inner.get_by_id(id).await
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
    async fn store_completed(
        &self,
        channel: &str,
        channel_id: Option<&str>,
        mode: &str,
        input_json: Option<&str>,
        result_json: &str,
        duration_ms: f64,
        task_trace_json: Option<&str>,
    ) -> Result<String, OrionError> {
        self.inner
            .store_completed(
                channel,
                channel_id,
                mode,
                input_json,
                result_json,
                duration_ms,
                task_trace_json,
            )
            .await
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
    async fn list_paginated(&self, filter: &TraceFilter) -> Result<TracePage, OrionError> {
        self.inner.list_paginated(filter).await
    }
    async fn delete_older_than(&self, hours: u64) -> Result<u64, OrionError> {
        self.inner.delete_older_than(hours).await
    }
}

/// Shared harness for the persist_success failure-arm tests: start one
/// worker over `trace_repo` with the given queue config, submit one async
/// message, and poll the trace row until it leaves pending/running.
async fn run_one_message_to_terminal_status(
    trace_repo: Arc<dyn TraceRepository>,
    queue_config: orion::config::TraceQueueConfig,
) -> Trace {
    let engine = Arc::new(RwLock::new(Arc::new(
        dataflow_rs::Engine::builder().build().unwrap(),
    )));
    let channel_registry = Arc::new(orion::channel::ChannelRegistry::new());
    // Pinned rather than defaulted: this test asserts on the trace row right
    // after processing, so the write has to land inline.
    let tracing_storage = orion::config::TraceStorageConfig {
        mode: orion::config::TraceStorageMode::Sync,
        ..Default::default()
    };
    let (persistence_queue, _persistence_handle) =
        orion::queue::trace_persistence::start(&tracing_storage, trace_repo.clone());
    let (trace_queue, _worker_handle) = orion::queue::start_workers(
        &queue_config,
        engine,
        trace_repo.clone(),
        None,
        channel_registry,
        persistence_queue,
        tracing_storage,
        String::new(),
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
        let stored = trace_repo.get_by_id(&trace.id).await.unwrap();
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
    let trace_repo: Arc<dyn TraceRepository> = Arc::new(FailAllResultWrites {
        inner: Arc::new(SqlTraceRepository::new(pool)),
    });

    let stored = run_one_message_to_terminal_status(
        trace_repo,
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
    let trace_repo: Arc<dyn TraceRepository> = Arc::new(SqlTraceRepository::new(pool));

    let stored = run_one_message_to_terminal_status(
        trace_repo,
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
impl TraceRepository for ConcurrencyProbeRepo {
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
    async fn get_by_id(&self, _id: &str) -> Result<Trace, OrionError> {
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
    async fn store_completed(
        &self,
        _channel: &str,
        _channel_id: Option<&str>,
        _mode: &str,
        _input_json: Option<&str>,
        _result_json: &str,
        _duration_ms: f64,
        _task_trace_json: Option<&str>,
    ) -> Result<String, OrionError> {
        unimplemented!()
    }
    async fn list_paginated(&self, _filter: &TraceFilter) -> Result<TracePage, OrionError> {
        unimplemented!()
    }
    async fn delete_older_than(&self, _hours: u64) -> Result<u64, OrionError> {
        unimplemented!()
    }
}

#[tokio::test]
async fn persistence_workers_run_in_parallel() {
    let current = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let max_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let repo: Arc<dyn TraceRepository> = Arc::new(ConcurrencyProbeRepo {
        current: current.clone(),
        max_seen: max_seen.clone(),
    });

    let config = orion::config::TraceStorageConfig {
        mode: orion::config::TraceStorageMode::Async,
        async_workers: 2,
        ..Default::default()
    };
    let (queue, handle) = orion::queue::trace_persistence::start(&config, repo);

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
impl TraceRepository for FlakyUpdateStatusRepo {
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
    async fn get_by_id(&self, _id: &str) -> Result<Trace, OrionError> {
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
    async fn store_completed(
        &self,
        _channel: &str,
        _channel_id: Option<&str>,
        _mode: &str,
        _input_json: Option<&str>,
        _result_json: &str,
        _duration_ms: f64,
        _task_trace_json: Option<&str>,
    ) -> Result<String, OrionError> {
        unimplemented!()
    }
    async fn list_paginated(&self, _filter: &TraceFilter) -> Result<TracePage, OrionError> {
        unimplemented!()
    }
    async fn delete_older_than(&self, _hours: u64) -> Result<u64, OrionError> {
        unimplemented!()
    }
}

#[tokio::test]
async fn transient_persistence_failure_is_retried_not_dropped() {
    let fails_remaining = Arc::new(std::sync::atomic::AtomicUsize::new(1));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let repo: Arc<dyn TraceRepository> = Arc::new(FlakyUpdateStatusRepo {
        fails_remaining: fails_remaining.clone(),
        calls: calls.clone(),
    });

    let config = orion::config::TraceStorageConfig {
        mode: orion::config::TraceStorageMode::Async,
        async_workers: 1,
        ..Default::default()
    };
    let (queue, handle) = orion::queue::trace_persistence::start(&config, repo);
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
