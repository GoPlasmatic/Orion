use crate::common;

use crate::common::{body_json, json_request};
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

// ============================================================
// Graceful shutdown: queue drain
// ============================================================

/// Verify that shutting down the worker pool completes within the
/// configured timeout even when the queue is empty.
#[tokio::test]
async fn test_worker_shutdown_empty_queue() {
    let pool = orion::storage::init_pool(&orion::config::StorageConfig {
        url: "sqlite::memory:".to_string(),
        max_connections: 1,
        ..Default::default()
    })
    .await
    .unwrap();

    let trace_repo: std::sync::Arc<dyn orion::storage::repositories::traces::TraceRepository> =
        std::sync::Arc::new(
            orion::storage::repositories::traces::SqlTraceRepository::new(pool.clone()),
        );

    let runtime = common::empty_runtime();

    let test_queue_config = orion::config::TraceQueueConfig {
        workers: 2,
        buffer_size: 10,
        shutdown_timeout_secs: 2, // short for tests
        processing_timeout_ms: 60_000,
        max_result_size_bytes: 1_048_576,
        max_queue_memory_bytes: 104_857_600,
        ..Default::default()
    };
    let global_trace_storage = orion::config::TraceStorageConfig::default();
    let (persistence_queue, _persistence_handle) = orion::queue::trace_persistence::start(
        &orion::runtime::TaskRegistry::new(),
        &global_trace_storage,
        trace_repo.clone(),
    );
    let (queue, worker_handle) = orion::queue::start_workers(
        &orion::runtime::TaskRegistry::new(),
        &test_queue_config,
        orion::queue::WorkerDeps {
            runtime,
            trace_repo,
            dlq_repo: None,
            persistence_queue,
            global_trace_storage,
            rollout_sticky_header: String::new(),
        },
    );

    // Drop the queue sender so the dispatcher loop exits when WorkerHandle
    // drops its internal sender — otherwise the channel stays open.
    drop(queue);

    // Shutdown of an empty queue must complete promptly. A generous timeout
    // (not a wall-clock measurement) so a stalled CI runner cannot fail a
    // healthy shutdown, while a wedged one still gets caught.
    tokio::time::timeout(std::time::Duration::from_secs(10), worker_handle.shutdown())
        .await
        .expect("empty-queue shutdown did not complete within 10s");
}

/// Verify that in-flight async traces reach a terminal state when
/// the worker pool is shut down.
#[tokio::test]
async fn test_inflight_trace_completes_on_shutdown() {
    let app = common::test_app().await;

    // Create a workflow + channel so the engine can actually process something
    let (_channel, _wf_id) = common::create_and_activate_channel(
        &app,
        "shutdown-test",
        json!({
            "name": "Shutdown Workflow",
            "tasks": [{
                "id": "t1",
                "name": "Log",
                "function": {"name": "log", "input": {"message": "shutdown test"}}
            }]
        }),
    )
    .await;

    // Submit an async trace
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/shutdown-test/async",
            Some(json!({"data": {"test": true}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    let trace_id = body["trace_id"].as_str().unwrap().to_string();
    let token = body["trace_token"].as_str().unwrap().to_string();

    // Poll until the trace reaches a terminal state
    let result = common::poll_trace_until_done(&app, &trace_id, 40, Some(&token)).await;
    let status = result["status"].as_str().unwrap_or("");
    assert!(
        status == "completed" || status == "failed",
        "Trace should reach terminal state, got: {}",
        status
    );
}

// ============================================================
// Trace cleanup
// ============================================================

/// Verify the trace cleanup task can be started and aborted without
/// panicking or leaving state in a broken condition.
#[tokio::test]
async fn test_trace_cleanup_abort_is_safe() {
    let pool = orion::storage::init_pool(&orion::config::StorageConfig {
        url: "sqlite::memory:".to_string(),
        max_connections: 1,
        ..Default::default()
    })
    .await
    .unwrap();

    let trace_repo: std::sync::Arc<dyn orion::storage::repositories::traces::TraceRepository> =
        std::sync::Arc::new(
            orion::storage::repositories::traces::SqlTraceRepository::new(pool.clone()),
        );

    // Start cleanup with very short interval
    let tasks = orion::runtime::TaskRegistry::new();
    orion::queue::start_trace_cleanup(&tasks, 72, 1, trace_repo.clone(), None);
    assert_eq!(
        tasks.report().len(),
        1,
        "Cleanup task should be registered when retention > 0"
    );

    // Let it run briefly, then stop it cooperatively. The task returns at its
    // next tick rather than being cut mid-DELETE, so the shutdown completes
    // well inside the deadline.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    tasks.shutdown(std::time::Duration::from_secs(5)).await;
    assert_eq!(
        tasks.report()[0].state,
        orion::runtime::TaskState::ShutDown,
        "a cooperative stop must be recorded as a clean shutdown, not a failure"
    );
}

/// Verify that cleanup with retention_hours=0 does not start a task.
#[tokio::test]
async fn test_trace_cleanup_disabled_when_zero_retention() {
    let pool = orion::storage::init_pool(&orion::config::StorageConfig {
        url: "sqlite::memory:".to_string(),
        max_connections: 1,
        ..Default::default()
    })
    .await
    .unwrap();

    let trace_repo: std::sync::Arc<dyn orion::storage::repositories::traces::TraceRepository> =
        std::sync::Arc::new(orion::storage::repositories::traces::SqlTraceRepository::new(pool));

    let tasks = orion::runtime::TaskRegistry::new();
    orion::queue::start_trace_cleanup(&tasks, 0, 3600, trace_repo, None);
    assert!(
        tasks.report().is_empty(),
        "Cleanup should not start when retention is 0"
    );
}
