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
// Engine reload is serialised by AppState::reload_lock
// ============================================================

/// A reload is a read-modify-write: it reads the active rows, builds the new
/// engine from the current one, republishes the registry and stores the
/// engine. Two of them interleaved would each build from the same pre-mutation
/// read and the loser's `store` would win — so `reload_engine` must take
/// `AppStateInner::reload_lock` for the whole sequence, not just for the
/// registry rebuild (which has a lock of its own that does not cover the
/// engine store).
///
/// Holding the lock from the test is what makes this deterministic: with the
/// acquisition removed from `reload_engine_with_opts` the spawned reload runs
/// to completion while the guard is held and the `is_finished` assertion fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_reload_waits_on_the_reload_lock() {
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;

    let guard = state.reload_lock.lock().await;

    let reload = tokio::spawn({
        let state = state.clone();
        async move { orion::runtime::reload_engine(&state).await }
    });

    // Long enough for the spawned task to be scheduled and reach the lock.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !reload.is_finished(),
        "reload completed while reload_lock was held — the reload is not serialised"
    );

    drop(guard);
    reload
        .await
        .expect("reload task")
        .expect("reload succeeds once the lock is free");
}

/// Concurrent reloads racing a mutation converge on the mutation's outcome.
/// Without the lock the losing build — made from the pre-activation read —
/// can `store` last and leave the new channel unrouted until the next reload.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_reloads_converge_on_the_latest_rows() {
    let app = common::test_app().await;

    common::create_and_activate_channel(
        &app,
        "converge-ch",
        common::simple_log_workflow("Converge Workflow"),
    )
    .await;

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            app.oneshot(common::json_request(
                "POST",
                "/api/v1/admin/engine/reload",
                None,
            ))
            .await
            .unwrap()
            .status()
        }));
    }
    for status in join_all(tasks).await {
        assert_eq!(status, StatusCode::OK, "reload failed with {status}");
    }

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/converge-ch",
            Some(json!({"data": {"after": true}})),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the channel is unrouted after concurrent reloads"
    );
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

// ============================================================
// The engine and the channel estate are one published generation
// ============================================================

/// A reload publishes **once**.
///
/// This is the structural guarantee, stated as a number. The engine and the
/// channel estate used to be two `ArcSwap`s stored one after the other: each
/// store atomic, the pair not. Between them the node served the new estate
/// against the old engine, and no ordering of the two could remove that window
/// — only choose which mismatch it produced.
///
/// So the fix is not "store them closer together", it is "there is one store".
/// Counting it is what keeps that true: re-split publication and this fails,
/// whatever the two halves are called by then.
#[tokio::test]
async fn a_reload_publishes_exactly_one_generation() {
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());

    // A channel worth republishing, so the reload below is doing real work
    // rather than rebuilding nothing.
    common::create_and_activate_channel(&app, "gen-count-ch", common::echo_workflow("Gen Count"))
        .await;

    let before = state.runtime.published_count();
    assert_eq!(
        state.runtime.load().id,
        before,
        "the live generation's id is the publish count"
    );

    orion::runtime::reload_engine(&state)
        .await
        .expect("reload succeeds");

    assert_eq!(
        state.runtime.published_count(),
        before + 1,
        "one reload must publish exactly one generation"
    );

    // And the pair that came out of it is coherent: the channel is in the
    // estate and its workflow is in the engine, read off one load.
    let generation = state.runtime.load();
    assert_eq!(generation.id, before + 1);
    assert!(
        generation.channels.get_by_name("gen-count-ch").is_some(),
        "the published estate must carry the channel"
    );
    assert!(
        generation
            .engine
            .workflows()
            .iter()
            .any(|w| w.channel == "gen-count-ch"),
        "…and the engine published with it must carry its workflow"
    );
}

/// A channel is never routable before its workflow is runnable.
///
/// The failure this pins is not a crash or an error code — it is a `200` that
/// did nothing. With the estate and the engine published separately, a request
/// arriving between the two stores was admitted against the *new* estate (the
/// channel exists, its guards run) and executed by the *old* engine (which has
/// no workflow for it). `process_message_for_channel` matched zero workflows,
/// reported `Ok`, and the caller received its own input back with a `200`.
///
/// Every status code in that story is correct, which is exactly why
/// `test_engine_reload_under_data_load` above cannot see it: the assertion has
/// to be on the body. `404` is a fine answer here — the channel genuinely is
/// not active yet. `200` without the workflow's stamp is the bug.
///
/// Timing-dependent by nature: it samples a window that no longer exists, so
/// it proves nothing on its own. The guarantee is structural — one value, one
/// store (`a_reload_publishes_exactly_one_generation`, and the retention test
/// in `runtime::generation`). This is the net under it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_channel_is_never_routable_before_its_workflow_runs() {
    let app = common::test_app().await;

    for round in 0..8 {
        let channel = format!("gen-race-ch-{round}");

        let activator = {
            let app = app.clone();
            let channel = channel.clone();
            tokio::spawn(async move {
                common::create_and_activate_channel(
                    &app,
                    &channel,
                    common::echo_workflow(&format!("Gen Race {round}")),
                )
                .await;
            })
        };

        // Hammer the channel across the activation. Stop once it answers with
        // the workflow's output — by then the generation carrying both halves
        // is live and there is nothing left to catch.
        let mut served = false;
        while !served {
            let resp = app
                .clone()
                .oneshot(common::json_request(
                    "POST",
                    &format!("/api/v1/data/{channel}"),
                    Some(serde_json::json!({"data": {"round": round}})),
                ))
                .await
                .unwrap();
            let status = resp.status();
            if status == StatusCode::NOT_FOUND {
                // Not activated yet, or activated and not yet published. Both
                // are whole answers from one generation.
                continue;
            }
            assert_eq!(
                status,
                StatusCode::OK,
                "a channel mid-activation answers 404 or 200, nothing else"
            );
            let body = common::body_json(resp).await;
            assert_eq!(
                body["data"]["matched"],
                serde_json::json!(true),
                "a 200 means the channel was admitted, so its workflow must have \
                 run: a routable channel whose engine has no workflow for it \
                 returns the caller's own input with a 200 (body = {body})"
            );
            served = true;
        }

        activator.await.unwrap();
    }
}
