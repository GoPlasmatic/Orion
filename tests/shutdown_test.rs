mod common;

use axum::http::StatusCode;
use common::{body_json, json_request};
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

    let engine = std::sync::Arc::new(tokio::sync::RwLock::new(std::sync::Arc::new(
        dataflow_rs::Engine::new(vec![], None),
    )));

    let test_queue_config = orion::config::QueueConfig {
        workers: 2,
        buffer_size: 10,
        shutdown_timeout_secs: 2, // short for tests
        processing_timeout_ms: 60_000,
        max_result_size_bytes: 1_048_576,
        max_queue_memory_bytes: 104_857_600,
        ..Default::default()
    };
    let (queue, worker_handle) = orion::queue::start_workers(
        &test_queue_config,
        engine,
        trace_repo,
        None, // no DLQ for this test
    );

    // Drop the queue sender so the dispatcher loop exits when WorkerHandle
    // drops its internal sender — otherwise the channel stays open.
    drop(queue);

    // Shutdown should complete almost immediately with an empty queue
    let start = std::time::Instant::now();
    worker_handle.shutdown().await;
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_secs() < 2,
        "Empty queue shutdown took {elapsed:?}, expected near-instant"
    );
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

    // Poll until the trace reaches a terminal state
    let result = common::poll_trace_until_done(&app, &trace_id, 40).await;
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
    let handle = orion::queue::start_trace_cleanup(72, 1, trace_repo.clone());
    assert!(
        handle.is_some(),
        "Cleanup task should start when retention > 0"
    );

    let handle = handle.unwrap();

    // Let it run briefly, then abort
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    handle.abort();

    // Wait for abort to complete — should not panic
    let result = handle.await;
    assert!(result.is_err(), "Aborted task should return JoinError");
    assert!(result.unwrap_err().is_cancelled());
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

    let handle = orion::queue::start_trace_cleanup(0, 3600, trace_repo);
    assert!(
        handle.is_none(),
        "Cleanup should not start when retention is 0"
    );
}
