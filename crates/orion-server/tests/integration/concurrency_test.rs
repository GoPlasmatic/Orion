use crate::common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

/// Collect results from a vec of JoinHandles.
async fn join_all(tasks: Vec<tokio::task::JoinHandle<StatusCode>>) -> Vec<StatusCode> {
    let mut results = Vec::with_capacity(tasks.len());
    for task in tasks {
        results.push(task.await.unwrap());
    }
    results
}

// ============================================================
// Concurrent data requests to multiple channels
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_data_requests_multiple_channels() {
    let app = common::test_app().await;

    // Set up multiple active channels
    let n = 5;
    for i in 0..n {
        common::create_and_activate_channel(
            &app,
            &format!("conc-ch-{}", i),
            common::simple_log_workflow(&format!("Conc Workflow {}", i)),
        )
        .await;
    }

    // Fire concurrent data requests to all channels
    let mut tasks = Vec::new();
    for round in 0..3 {
        for i in 0..n {
            let app = app.clone();
            tasks.push(tokio::spawn(async move {
                let resp = app
                    .oneshot(common::json_request(
                        "POST",
                        &format!("/api/v1/data/conc-ch-{}", i),
                        Some(json!({"data": {"round": round, "channel": i}})),
                    ))
                    .await
                    .unwrap();
                resp.status()
            }));
        }
    }

    let results = join_all(tasks).await;

    // All data requests should succeed
    for (i, status) in results.iter().enumerate() {
        assert_eq!(
            *status,
            StatusCode::OK,
            "Data request {} returned unexpected status: {}",
            i,
            status,
        );
    }
}

// ============================================================
// Engine reload under concurrent data requests
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_engine_reload_under_data_load() {
    let app = common::test_app().await;

    // Set up an active channel
    common::create_and_activate_channel(
        &app,
        "reload-ch",
        common::simple_log_workflow("Reload Test Workflow"),
    )
    .await;

    // Spawn concurrent reloads and data requests
    let mut tasks = Vec::new();

    // 5 concurrent engine reloads
    for _ in 0..5 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            let resp = app
                .oneshot(common::json_request(
                    "POST",
                    "/api/v1/admin/engine/reload",
                    None,
                ))
                .await
                .unwrap();
            resp.status()
        }));
    }

    // 5 concurrent data requests
    for _ in 0..5 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            let resp = app
                .oneshot(common::json_request(
                    "POST",
                    "/api/v1/data/reload-ch",
                    Some(json!({"data": {"test": true}})),
                ))
                .await
                .unwrap();
            resp.status()
        }));
    }

    let results = join_all(tasks).await;

    // Reloads should all succeed (200)
    for status in &results[..5] {
        assert_eq!(
            *status,
            StatusCode::OK,
            "Engine reload failed with {}",
            status
        );
    }

    // Data requests during a reload must succeed outright: readers hold the
    // previous engine Arc while the swap happens, so there is no window in
    // which the channel is unserved.
    for status in &results[5..] {
        assert_eq!(*status, StatusCode::OK, "data request failed during reload");
    }

    // After reloads, the channel should still work
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/reload-ch",
            Some(json!({"data": {"after_reload": true}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ============================================================
// Channel status race: activate/deactivate while sending data
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_channel_status_race_no_panics() {
    let app = common::test_app().await;

    // Create workflow and channel
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(common::simple_log_workflow("Race Workflow")),
        ))
        .await
        .unwrap();
    let body = common::body_json(resp).await;
    let wf_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{}/status", wf_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "race-ch",
                "channel_type": "sync",
                "protocol": "http",
                "methods": ["POST"],
                "route_pattern": "/race",
                "workflow_id": wf_id,
            })),
        ))
        .await
        .unwrap();
    let body = common::body_json(resp).await;
    let ch_id = body["data"]["channel_id"].as_str().unwrap().to_string();

    // Activate first
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{}/status", ch_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Now rapidly toggle status while sending data
    let mut tasks = Vec::new();

    // Status toggle tasks, spawned concurrently with the data requests below.
    for i in 0..10 {
        let app = app.clone();
        let ch_id = ch_id.clone();
        tasks.push(tokio::spawn(async move {
            let status = if i % 2 == 0 { "archived" } else { "active" };
            let resp = app
                .oneshot(common::json_request(
                    "PATCH",
                    &format!("/api/v1/admin/channels/{}/status", ch_id),
                    Some(json!({"status": status})),
                ))
                .await
                .unwrap();
            resp.status()
        }));
    }

    // Concurrent data requests
    for _ in 0..10 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            let resp = app
                .oneshot(common::json_request(
                    "POST",
                    "/api/v1/data/race-ch",
                    Some(json!({"data": {"test": true}})),
                ))
                .await
                .unwrap();
            resp.status()
        }));
    }

    let results = join_all(tasks).await;

    // Toggles land as 200, or as a client-error refusal when the transition
    // is invalid at that instant. Data requests see 200 (active, or the
    // engine fallback while archived) or 404/503 around the flip. Anything
    // else — above all any 5xx — is a race bug.
    let allowed = [
        StatusCode::OK,
        StatusCode::BAD_REQUEST,
        StatusCode::CONFLICT,
        StatusCode::NOT_FOUND,
        StatusCode::SERVICE_UNAVAILABLE,
    ];
    for (i, status) in results.iter().enumerate() {
        assert!(
            allowed.contains(status),
            "task {i} returned unexpected status {status} under the status race"
        );
    }
}

// ============================================================
// Concurrent workflow creation with same name
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_workflow_creation_same_name() {
    let app = common::test_app().await;

    // Try to create 5 workflows with the same name concurrently
    let mut tasks = Vec::new();
    for _ in 0..5 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            let resp = app
                .oneshot(common::json_request(
                    "POST",
                    "/api/v1/admin/workflows",
                    Some(common::simple_log_workflow("duplicate-wf")),
                ))
                .await
                .unwrap();
            resp.status()
        }));
    }

    let results = join_all(tasks).await;

    // Names are not unique keys — every creation gets its own workflow_id,
    // so all five must succeed outright.
    for status in &results {
        assert_eq!(
            *status,
            StatusCode::CREATED,
            "concurrent same-name creation must succeed"
        );
    }
}
