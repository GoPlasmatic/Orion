use crate::common;

use axum::http::StatusCode;
use tower::ServiceExt;

// ============================================================
// Pool exhaustion under concurrent DB operations
// ============================================================

/// With max_connections=1, fire many concurrent requests.
/// All should complete without panics or hangs.
#[tokio::test]
async fn test_pool_exhaustion_no_panics() {
    let config = orion::config::AppConfig {
        storage: orion::config::StorageConfig {
            url: "sqlite::memory:".to_string(),
            max_connections: 1,
            acquire_timeout_secs: 2,
            ..Default::default()
        },
        ..Default::default()
    };
    let app = common::test_app_with_config(config).await;

    // Fire 20 concurrent admin list requests (each hits the DB)
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            let resp = app
                .oneshot(common::json_request("GET", "/api/v1/admin/workflows", None))
                .await
                .unwrap();
            let status = resp.status();
            let body = common::body_json(resp).await;
            (status, body)
        }));
    }

    let mut results = Vec::new();
    for task in tasks {
        results.push(task.await.unwrap());
    }

    // Contract under pool starvation: every request resolves to either a
    // success or a *well-formed* storage-error envelope — a clean HTTP
    // refusal, never a crash, a hang (proven by completion), or a bare body.
    let mut ok_count = 0;
    for (status, body) in &results {
        match *status {
            StatusCode::OK => {
                assert!(body["data"].is_array(), "OK response must carry data");
                ok_count += 1;
            }
            StatusCode::INTERNAL_SERVER_ERROR => {
                assert!(
                    body["error"]["code"].is_string(),
                    "pool-starved refusal must use the error envelope, got {body}"
                );
            }
            other => panic!("unexpected status under pool exhaustion: {other} ({body})"),
        }
    }

    // The pool serves one request at a time, so progress must be made.
    assert!(
        ok_count > 0,
        "expected at least some successes; all 20 starved"
    );
}

/// After a burst that exhausts the pool, verify the system recovers.
#[tokio::test]
async fn test_pool_recovery_after_burst() {
    let config = orion::config::AppConfig {
        storage: orion::config::StorageConfig {
            url: "sqlite::memory:".to_string(),
            max_connections: 1,
            acquire_timeout_secs: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let app = common::test_app_with_config(config).await;

    // Fire a burst of concurrent requests
    let mut tasks = Vec::new();
    for _ in 0..10 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            let resp = app
                .oneshot(common::json_request("GET", "/api/v1/admin/workflows", None))
                .await
                .unwrap();
            resp.status()
        }));
    }

    // Wait for all to complete
    for task in tasks {
        let _ = task.await.unwrap();
    }

    // Now verify recovery: a single request should succeed
    let resp = app
        .clone()
        .oneshot(common::json_request("GET", "/api/v1/admin/workflows", None))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "System should recover after pool exhaustion"
    );
}

/// With a normal pool size, concurrent requests should all succeed.
#[tokio::test]
async fn test_normal_pool_handles_concurrency() {
    let app = common::test_app().await;

    // Fire 20 concurrent admin list requests with default pool (5 connections)
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            let resp = app
                .oneshot(common::json_request("GET", "/api/v1/admin/workflows", None))
                .await
                .unwrap();
            resp.status()
        }));
    }

    let mut results = Vec::new();
    for task in tasks {
        results.push(task.await.unwrap());
    }

    // All should succeed with a properly sized pool
    for status in &results {
        assert_eq!(
            *status,
            StatusCode::OK,
            "All requests should succeed with normal pool size"
        );
    }
}
