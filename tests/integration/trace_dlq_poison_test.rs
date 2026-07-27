//! Q3 regression: a deterministically-failing async message must converge on
//! `queue.dlq_max_retries` instead of cycling through the DLQ forever.
//!
//! Wires the real components (engine, worker pool, DLQ retry loop, SQLite
//! repositories) rather than mocks, because the bug lived in the seam between
//! them: the retry loop deleted the DLQ row on resubmit and the worker
//! re-enqueued the failure at `retry_count = 0`, so `max_retries` was never
//! reached and every cycle inserted a fresh `traces` row.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use tokio::sync::RwLock;

use orion::channel::ChannelRegistry;
use orion::config::{QueueConfig, StorageConfig, TracingStorageConfig};
use orion::queue::{DlqRetryOptions, QueueMessage};
use orion::storage::DbPool;
use orion::storage::repositories::trace_dlq::{SqlTraceDlqRepository, TraceDlqRepository};
use orion::storage::repositories::traces::{SqlTraceRepository, TraceRepository};

const MAX_RETRIES: i64 = 2;

/// A task function that always fails, so every processing attempt routes the
/// message to the DLQ.
struct AlwaysFail;

#[async_trait]
impl AsyncFunctionHandler for AlwaysFail {
    type Input = serde_json::Value;

    async fn execute(
        &self,
        _ctx: &mut TaskContext<'_>,
        _input: &Self::Input,
    ) -> dataflow_rs::Result<TaskOutcome> {
        Err(DataflowError::FunctionExecution {
            context: "poison message".to_string(),
            source: None,
        })
    }
}

fn poison_engine() -> Arc<RwLock<Arc<dataflow_rs::Engine>>> {
    let workflow = dataflow_rs::Workflow::from_json(
        r#"{
            "id": "poison-wf",
            "name": "Poison",
            "priority": 0,
            "channel": "poison",
            "tasks": [
                {"id": "t1", "name": "Boom", "function": {"name": "always_fail", "input": {}}}
            ]
        }"#,
    )
    .expect("poison workflow parses");

    let engine = dataflow_rs::Engine::builder()
        .with_workflow(workflow)
        .register("always_fail", AlwaysFail)
        .build()
        .expect("engine builds");

    Arc::new(RwLock::new(Arc::new(engine)))
}

fn sqlite(pool: &DbPool) -> &sqlx::SqlitePool {
    match pool {
        DbPool::Sqlite(p) => p,
        _ => panic!("sqlite expected"),
    }
}

/// `(dlq rows, highest retry_count, rows still claimable, traces rows)`.
async fn snapshot(pool: &DbPool) -> (i64, i64, i64, i64) {
    let p = sqlite(pool);
    let (dlq, max_retry, claimable): (i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(MAX(retry_count), -1), \
         COALESCE(SUM(retry_count < max_retries), 0) FROM trace_dlq",
    )
    .fetch_one(p)
    .await
    .expect("dlq snapshot");
    let (traces,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM traces")
        .fetch_one(p)
        .await
        .expect("traces snapshot");
    (dlq, max_retry, claimable, traces)
}

#[tokio::test]
async fn poison_message_converges_on_dlq_max_retries() {
    let pool = orion::storage::init_pool(&StorageConfig {
        url: "sqlite::memory:".to_string(),
        max_connections: 5,
        ..Default::default()
    })
    .await
    .expect("pool");

    let trace_repo: Arc<dyn TraceRepository> = Arc::new(SqlTraceRepository::new(pool.clone()));
    let dlq_repo: Arc<dyn TraceDlqRepository> = Arc::new(SqlTraceDlqRepository::new(pool.clone()));
    let channel_registry = Arc::new(ChannelRegistry::new());
    let trace_storage = TracingStorageConfig::default();

    let (persistence_queue, _persistence_handle) =
        orion::queue::trace_persistence::start(&trace_storage, trace_repo.clone());

    let (trace_queue, _worker_handle) = orion::queue::start_workers(
        &QueueConfig {
            workers: 1,
            buffer_size: 16,
            dlq_max_retries: MAX_RETRIES,
            dlq_poll_interval_secs: 1,
            ..Default::default()
        },
        poison_engine(),
        trace_repo.clone(),
        Some(dlq_repo.clone()),
        channel_registry.clone(),
        persistence_queue,
        trace_storage,
        "x-orion-identity".to_string(),
    );

    let _dlq_retry = orion::queue::start_dlq_retry(
        DlqRetryOptions {
            poll_interval_secs: 1,
            batch_size: 10,
            lease_secs: 30,
            claimant: "test-node".to_string(),
            lease_gate: None,
        },
        dlq_repo.clone(),
        trace_queue.clone(),
        trace_repo.clone(),
        channel_registry,
    );

    let trace = trace_repo
        .create_pending("poison", None, "async", Some("{}"), None)
        .await
        .expect("pending trace");
    trace_queue
        .submit(QueueMessage {
            trace_id: trace.id.clone(),
            channel: "poison".to_string(),
            payload: serde_json::json!({"n": 1}),
            metadata: serde_json::json!({}),
            trace_headers: std::collections::HashMap::new(),
            profile_requested: false,
            backpressure_permit: None,
        })
        .await
        .expect("submit");

    // Phase 1: the lineage must climb to max_retries. Before the carry, every
    // re-enqueue landed at 0 and this never happened.
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut reached = false;
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if snapshot(&pool).await.1 >= MAX_RETRIES {
            reached = true;
            break;
        }
    }
    let (_, max_retry, claimable, traces_before) = snapshot(&pool).await;
    assert!(
        reached,
        "poison message never reached dlq_max_retries (highest retry_count seen: {max_retry})"
    );
    assert_eq!(claimable, 0, "an exhausted entry must not stay claimable");

    // Phase 2: and then stop. Several poll intervals of quiet must produce no
    // further retries — no new trace rows, no claimable DLQ entries.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let (dlq_rows, max_retry, claimable, traces_after) = snapshot(&pool).await;
    assert_eq!(
        max_retry, MAX_RETRIES,
        "retry_count must not grow past the cap"
    );
    assert_eq!(claimable, 0, "exhausted entries must stay unclaimable");
    assert_eq!(
        traces_after, traces_before,
        "an exhausted message must stop creating retry traces"
    );
    assert_eq!(
        dlq_rows, 1,
        "exactly one DLQ row should survive the lineage"
    );
}
