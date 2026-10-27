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
                        &format!("/api/v2/data/conc-ch-{}", i),
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
// Rebuilds under concurrent data requests
// ============================================================

/// Deployments that rebuild the generation, one after another, while data
/// requests to a channel they do not touch are in flight: every request is
/// served.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_rebuild_under_data_load() {
    let app = common::test_app().await;

    // Set up an active channel
    common::create_and_activate_channel(
        &app,
        "reload-ch",
        common::simple_log_workflow("Reload Test Workflow"),
    )
    .await;

    // 5 deployments, each a rebuild of the whole generation
    let deployments = {
        let app = app.clone();
        tokio::spawn(async move {
            for i in 0..5 {
                common::create_and_activate_workflow(
                    &app,
                    common::simple_log_workflow(&format!("Reload Churn {i}")),
                )
                .await;
            }
        })
    };

    // 5 concurrent data requests
    let mut tasks = Vec::new();
    for _ in 0..5 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            let resp = app
                .oneshot(common::json_request(
                    "POST",
                    "/api/v2/data/reload-ch",
                    Some(json!({"data": {"test": true}})),
                ))
                .await
                .unwrap();
            resp.status()
        }));
    }

    let results = join_all(tasks).await;
    deployments.await.expect("every deployment converges");

    // Data requests during a rebuild must succeed outright: readers hold the
    // previous engine Arc while the swap happens, so there is no window in
    // which the channel is unserved.
    for status in &results {
        assert_eq!(
            *status,
            StatusCode::OK,
            "data request failed during rebuild"
        );
    }

    // After the rebuilds, the channel should still work
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v2/data/reload-ch",
            Some(json!({"data": {"after_reload": true}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ============================================================
// Rebuilds are serialised by AppState::reload_lock
// ============================================================

/// A rebuild is a read-modify-write: it reads the committed revision,
/// builds the new engine from the current one, republishes the estate and
/// stores the engine. Two of them interleaved would each build from the same
/// generation and the loser's `store` would win — so a rebuild must take
/// `AppStateInner::reload_lock` for the whole sequence.
///
/// Holding the lock from the test is what makes this deterministic: with the
/// acquisition removed from the rebuild, the deployment below converges
/// while the guard is held and the `is_finished` assertion fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rebuild_waits_on_the_reload_lock() {
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());

    let guard = state.reload_lock.lock().await;

    let deployment = tokio::spawn({
        let app = app.clone();
        async move {
            common::create_and_activate_channel(
                &app,
                "lock-ch",
                common::simple_log_workflow("Lock Workflow"),
            )
            .await
        }
    });

    // Long enough for the deployment to commit and reach the lock.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !deployment.is_finished(),
        "a deployment converged while reload_lock was held — the rebuild is not serialised"
    );
    assert!(
        state
            .runtime
            .load()
            .channels
            .get_by_name("lock-ch")
            .is_none(),
        "nothing is published while the lock is held"
    );

    drop(guard);
    deployment
        .await
        .expect("the deployment converges once the lock is free");
    assert!(
        state
            .runtime
            .load()
            .channels
            .get_by_name("lock-ch")
            .is_some()
    );
}

/// Rebuilds racing a newer deployment converge on the newer one. A rebuild
/// of a revision the node has already moved past is refused rather than
/// published, so a losing build made from an older revision can never
/// `store` last and leave the newer channel unrouted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_reloads_converge_on_the_latest_rows() {
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());

    common::create_and_activate_workflow(&app, common::simple_log_workflow("Older Workflow")).await;
    let (older, _) = state.deployments.current_head();
    common::create_and_activate_channel(
        &app,
        "converge-ch",
        common::simple_log_workflow("Converge Workflow"),
    )
    .await;
    let (latest, _) = state.deployments.current_head();
    assert!(latest > older);
    let published = state.runtime.load().id;

    let mut tasks = Vec::new();
    for i in 0..8 {
        let state = state.clone();
        let revision = if i % 2 == 0 { older } else { latest };
        tasks.push((
            revision,
            tokio::spawn(async move {
                orion::runtime::reconcile_deployment_revision(&state, revision).await
            }),
        ));
    }
    for (revision, task) in tasks {
        let result = task.await.unwrap();
        assert_eq!(result.is_ok(), revision == latest, "{result:?}");
    }
    assert_eq!(
        state.runtime.load().id,
        published,
        "a refused rebuild publishes nothing"
    );

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v2/data/converge-ch",
            Some(json!({"data": {"after": true}})),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the channel is unrouted after concurrent rebuilds"
    );
}

/// Queue the background build ahead of the request's post-commit build, so the
/// response deterministically exercises an already-applied revision.
#[tokio::test]
async fn apply_and_rollback_succeed_when_the_background_reconciler_applies_first() {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    let mut config = orion::config::AppConfig::default();
    config.cluster.epoch_poll_interval_ms = 3_600_000;
    let state = common::test_state_with_config(config).await;
    let app = orion::server::build_router(state.clone());
    let apply = common::deploy::apply_body(
        &app,
        0,
        json!({"schema_version": 2, "providers": [], "members": []}),
        "background-first-apply",
    )
    .await;
    let rollback = json!({
        "base_revision": 1, "rollback_to": 1,
        "idempotency_key": "background-first-rollback", "reason": "test"
    });

    for (revision, body) in [(1, apply), (2, rollback)] {
        let before = state.runtime.published_count();
        let guard = state.reload_lock.lock().await;
        let background = orion::runtime::reconcile_deployment_revision(&state, revision);
        tokio::pin!(background);
        // Tokio's mutex queues in poll order. Register this caller before the
        // request commits and queues its own reconciliation.
        std::future::poll_fn(|cx| {
            assert!(background.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let request = tokio::spawn({
            let app = app.clone();
            async move {
                app.oneshot(common::json_request(
                    "POST",
                    "/api/v2/admin/deployments",
                    Some(body),
                ))
                .await
                .unwrap()
            }
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while state.deployments.current_head().0 != revision {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("request committed while the build is blocked");
        drop(guard);
        let (built, response) = tokio::join!(background, request);
        built.expect("background build succeeds");
        let response = response.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = common::body_json(response).await;
        assert_eq!(body["committed_revision"], revision, "{body}");
        assert_eq!(body["evaluation"]["converged"], true, "{body}");
        assert!(body.get("local_error").is_none(), "{body}");
        assert_eq!(state.runtime.published_count(), before + 1);
        assert!(!state.reload_degraded.load(Ordering::Acquire));
        let local = state.reconciler.local_status();
        assert!(local.last_error.is_none(), "{local:?}");
        assert_eq!(local.backoff_attempt, 0);
        assert_eq!(
            orion::runtime::reconcile_deployment_revision(&state, revision)
                .await
                .unwrap(),
            local
        );
        assert_eq!(state.runtime.published_count(), before + 1);
    }
    state.tasks.shutdown(Duration::from_secs(2)).await;
}

// ============================================================
// Channel deploy/remove race while sending data
// ============================================================

/// Deploying a channel and deploying without it, concurrently and with data
/// requests in flight, never answers a 5xx. Each toggle is an apply over the
/// head it read: it lands (`202`) or loses the compare-and-swap to another
/// toggle (`409`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_channel_status_race_no_panics() {
    let app = common::test_app().await;

    let wf_id =
        common::create_and_activate_workflow(&app, common::simple_log_workflow("Race Workflow"))
            .await;
    common::deploy_channel(
        &app,
        json!({
            "name": "race-ch",
            "channel_type": "sync",
            "protocol": "http",
            "methods": ["POST"],
            "route_pattern": "/race",
            "workflow_id": wf_id,
        }),
    )
    .await;
    let (_, with_channel) = common::deploy::head(&app).await;
    let without_channel: Vec<serde_json::Value> = with_channel
        .iter()
        .filter(|m| m["entity"]["kind"] != "channel")
        .cloned()
        .collect();

    // Now rapidly toggle the channel while sending data
    let mut tasks = Vec::new();

    // Toggle tasks, spawned concurrently with the data requests below.
    for i in 0..10 {
        let app = app.clone();
        let members = if i % 2 == 0 {
            without_channel.clone()
        } else {
            with_channel.clone()
        };
        tasks.push(tokio::spawn(async move {
            let (base, _) = common::deploy::head(&app).await;
            common::deploy::apply(&app, base, members).await.0
        }));
    }

    // Concurrent data requests
    for _ in 0..10 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            let resp = app
                .oneshot(common::json_request(
                    "POST",
                    "/api/v2/data/race-ch",
                    Some(json!({"data": {"test": true}})),
                ))
                .await
                .unwrap();
            resp.status()
        }));
    }

    let results = join_all(tasks).await;

    // Toggles land as 202, or as a 409 when another toggle moved the head
    // first. Data requests see 200 while the channel is deployed and 404 or
    // 503 around the flip. Anything else — above all any 5xx other than a
    // 503 — is a race bug.
    let allowed = [
        StatusCode::OK,
        StatusCode::ACCEPTED,
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
// Concurrent uploads of one definition
// ============================================================

/// A definition is stored by its digest, so five concurrent uploads of the
/// same workflow all succeed and name one digest: none of them is refused as
/// a duplicate, and none stores a second copy under another name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_uploads_of_one_definition() {
    let app = common::test_app().await;

    let mut tasks = Vec::new();
    for _ in 0..5 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            common::deploy::try_upload(
                &app,
                common::deploy::workflow_definition(&common::simple_log_workflow("duplicate-wf")),
            )
            .await
        }));
    }

    let mut digests = std::collections::BTreeSet::new();
    for task in tasks {
        let (status, body) = task.await.unwrap();
        assert!(
            status.is_success(),
            "concurrent upload of one definition must succeed, got {status}: {body}"
        );
        digests.insert(body["digest"].as_str().expect("a digest").to_string());
    }
    assert_eq!(digests.len(), 1, "one definition, one digest: {digests:?}");
}

// ============================================================
// The engine and the channel estate are one published generation
// ============================================================

/// A rebuild publishes **once**.
///
/// This is the structural guarantee, stated as a number. Stored as two
/// `ArcSwap`s one after the other — each store atomic, the pair not — the
/// engine and the channel estate would leave a window in which the node serves
/// the new estate against the old engine, and no ordering of the two could
/// remove it, only choose which mismatch it produced.
///
/// So the guarantee is not "store them closer together", it is "there is one
/// store".
/// Counting it is what keeps that true: re-split publication and this fails,
/// whatever the two halves are called by then.
#[tokio::test]
async fn a_reload_publishes_exactly_one_generation() {
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());

    // A deployment worth publishing: a channel and its workflow, so the
    // rebuild is doing real work rather than rebuilding nothing.
    let before = state.runtime.published_count();
    assert_eq!(
        state.runtime.load().id,
        before,
        "the live generation's id is the publish count"
    );

    common::create_and_activate_channel(&app, "gen-count-ch", common::echo_workflow("Gen Count"))
        .await;

    assert_eq!(
        state.runtime.published_count(),
        before + 1,
        "one deployment must publish exactly one generation"
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
            .workflows()
            .iter()
            .any(|w| w.channel == "gen-count-ch"),
        "…and the engine published with it must carry its workflow"
    );
}

/// The building cutover (I182): a rebuild that keeps the plugin set compiles
/// every row on the running dataflow-rs 4 engine's compiler and swaps with
/// `Engine::with_program`, so the next generation runs on the same
/// `Registry` (Orion's functions, the plugin functions and the profile
/// layer), registered once at boot.
///
/// A workflow links only under the registry it compiled under
/// (`Compiler::link` panics otherwise), so one compiled on the outgoing
/// generation linking under the next is the proof; a registry assembled
/// afresh, as a plugin change assembles one, refuses it. Each generation's
/// `engine4` carries the workflows its `engine` serves.
#[tokio::test]
async fn a_reload_without_a_plugin_change_reuses_the_v4_registry() {
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());
    common::create_and_activate_channel(&app, "reuse-ch", common::echo_workflow("Reuse")).await;

    let probe = |compiler: &dataflow_rs::program::Compiler| {
        let json = serde_json::json!({"id": "probe", "tasks": [
            {"id": "t", "function": {"name": "log", "input": {"message": "x"}}}
        ]});
        let def = dataflow_rs::def::parse(&json, dataflow_rs::def::ParseOptions::default())
            .def
            .expect("the probe parses");
        compiler
            .compile_one(&def)
            .expect("the probe compiles")
            .workflow
    };
    let before = state.runtime.load();
    let compiled = probe(&before.engine.compiler());
    let fresh = probe(&before.engine.compiler());

    // A deployment that changes no plugin: the next generation is built on
    // the running engine's compiler.
    common::create_and_activate_workflow(&app, common::simple_log_workflow("Reuse Other")).await;

    let after = state.runtime.load();
    assert!(after.id > before.id);
    let engine4 = &after.engine;
    let program = engine4.compiler().link([compiled]);
    assert_eq!(
        program.workflows().len(),
        1,
        "one registry across the rebuild"
    );
    let channels: Vec<String> = after
        .workflows()
        .iter()
        .map(|w| w.channel.clone())
        .collect();
    assert!(channels.iter().any(|c| c == "reuse-ch"));

    // The control: a registry assembled afresh from the node's components
    // is another registry, which the outgoing generation's workflow does not
    // link under.
    let rebuilt = orion::runtime::build_registry(&state)
        .compiler(dataflow_rs::program::CompileOptions::default());
    let linked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rebuilt.link([fresh])));
    assert!(linked.is_err(), "a fresh registry is not the generation's");
}

/// A channel is never routable before its workflow is runnable.
///
/// The failure this pins is not a crash or an error code — it is a `200` that
/// did nothing. With the estate and the engine published separately, a request
/// arriving between the two stores would be admitted against the *new* estate
/// (the channel exists, its guards run) and executed by the *old* engine (which
/// has no workflow for it). `process_message_for_channel` would match zero
/// workflows, report `Ok`, and hand the caller its own input back with a `200`.
///
/// Every status code in that story is correct, which is exactly why
/// `test_rebuild_under_data_load` above cannot see it: the assertion has
/// to be on the body. `404` is a fine answer here — the channel genuinely is
/// not active yet. `200` without the workflow's stamp is the bug.
///
/// Timing-dependent by nature: it samples for a window the single store rules
/// out, so it proves nothing on its own. The guarantee is structural — one value, one
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
                    &format!("/api/v2/data/{channel}"),
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
