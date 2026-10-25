//! `/readyz` component reporting (D15, O10).
//!
//! A cluster's shared store lives in the state database, so the database
//! ping covers it; the `cluster_redis` component a cluster with a Redis
//! used to report is gone, and must stay gone.

use axum::http::StatusCode;
use tower::ServiceExt;

use crate::common::{self, body_json, json_request};

#[tokio::test]
async fn readyz_reports_no_cluster_redis_component() {
    let app = common::test_app().await;
    let resp = app
        .oneshot(json_request("GET", "/readyz", None))
        .await
        .expect("readyz");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ready");
    assert_eq!(body["components"]["database"], "ok");
    assert_eq!(body["components"]["engine"], "ok");
    assert!(
        body["components"].get("cluster_redis").is_none(),
        "the cluster_redis component is gone: {body}"
    );
}

/// The defect the task supervisor was added for: a node whose trace
/// persistence worker has died drops every trace routed to it — counted as an
/// overflow, indistinguishable from load — while `/readyz` keeps answering
/// `ready` and the data plane keeps returning 200s.
///
/// Driven through a task the harness's own registry supervises, because the
/// real workers are healthy in a test app: register one, kill it, and assert
/// the probes move. The wiring under test is the probe reading the registry,
/// which is the same code path a dead persistence worker takes.
#[tokio::test]
async fn a_dead_required_task_makes_the_node_not_ready() {
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/readyz", None))
        .await
        .expect("readyz");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["components"]["background_tasks"], "ok");

    // A required task dies.
    let guard = state
        .tasks
        .guard("test_worker", orion::runtime::Criticality::Required);
    tokio::spawn(guard.run(async { panic!("worker exploded") }))
        .await
        .expect_err("the task panicked");

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/readyz", None))
        .await
        .expect("readyz");
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a dead required background task must take the node out of rotation"
    );
    let body = body_json(resp).await;
    assert_eq!(body["status"], "not_ready");
    assert_eq!(body["components"]["background_tasks"], "error");

    // /health reports it too, and names it for an admin caller.
    let resp = app
        .oneshot(json_request("GET", "/health", None))
        .await
        .expect("health");
    let body = body_json(resp).await;
    assert_eq!(body["status"], "degraded");
    assert_eq!(body["components"]["background_tasks"], "error");
    let named = body["background_tasks"]
        .as_array()
        .expect("the per-task breakdown is admin-visible detail")
        .iter()
        .any(|t| t["name"] == "test_worker" && t["state"] == "failed");
    assert!(named, "the failed task must be named: {body}");
}

/// #310: the throughput trace modes must not report their own workers dead.
///
/// The persistence pool builds one `TaskGuard` and clones it per worker, so a
/// template guard is always left over. While `Drop` lived on `TaskGuard` that
/// leftover recorded the pool as `Failed` the moment `start()` returned —
/// microseconds *before* the workers logged that they had started — and since
/// the task is `Required`, `/health` was pinned to `degraded` and `/readyz` to
/// `not_ready` for the life of the process. Traces persisted correctly
/// throughout, so the only symptom was a node that never became ready.
///
/// Both modes, because both take the pooled path; `sync` spawns no worker and
/// was never affected, which is why it was the only mode that reported honestly.
#[tokio::test]
async fn the_async_trace_modes_report_their_workers_as_running() {
    for mode in [
        orion::config::TraceStorageMode::Async,
        orion::config::TraceStorageMode::Batch,
    ] {
        let mut config = orion::config::AppConfig::default();
        config.trace_storage.mode = mode;
        let state = common::test_state_with_config(config).await;
        let app = orion::server::build_router(state);

        let resp = app
            .clone()
            .oneshot(json_request("GET", "/readyz", None))
            .await
            .expect("readyz");
        assert_eq!(resp.status(), StatusCode::OK, "{mode:?}");
        assert_eq!(body_json(resp).await["status"], "ready", "{mode:?}");

        let resp = app
            .oneshot(json_request("GET", "/health", None))
            .await
            .expect("health");
        let body = body_json(resp).await;
        assert_eq!(body["status"], "ok", "{mode:?}: {body}");
        assert_eq!(body["components"]["background_tasks"], "ok", "{mode:?}");

        let persistence = body["background_tasks"]
            .as_array()
            .expect("the per-task breakdown")
            .iter()
            .find(|t| t["name"] == "trace_persistence")
            .cloned()
            .unwrap_or_else(|| panic!("{mode:?}: trace_persistence must be registered: {body}"));
        assert_eq!(persistence["state"], "running", "{mode:?}: {body}");
    }
}

/// The complement: an optional task's death is visible on `/health` but must
/// not pull the node out of its load balancer.
#[tokio::test]
async fn a_dead_optional_task_degrades_health_but_stays_ready() {
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());

    let guard = state
        .tasks
        .guard("test_retention", orion::runtime::Criticality::Optional);
    tokio::spawn(guard.run(async {})).await.expect("clean exit");

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/readyz", None))
        .await
        .expect("readyz");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["status"], "ready");

    let resp = app
        .oneshot(json_request("GET", "/health", None))
        .await
        .expect("health");
    let body = body_json(resp).await;
    assert_eq!(body["components"]["background_tasks"], "degraded");
}

#[tokio::test]
async fn test_health_endpoint() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/health", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert!(body.get("uptime_seconds").is_some());
    assert!(body.get("version").is_some());
    assert!(body.get("components").is_some());
    assert_eq!(body["components"]["database"], "ok");
    assert_eq!(body["components"]["engine"], "ok");
}

/// A failed engine reload is a `/health` degradation and **not** a `/readyz`
/// failure.
///
/// The pairing is the point. An admin mutation that commits and then fails to
/// reload now answers 2xx — the row is `active` and the next successful reload
/// serves it, so a 5xx would tell the client its change failed when it did
/// not. That makes `/health` the only place the condition is visible, so it has
/// to be visible there. `/readyz` stays ready because this node is serving
/// correctly, just not the newest config: ejecting it would trade a
/// stale-config problem for an availability one, which is the argument
/// `config_propagation` already makes.
///
/// The flag is set directly rather than by breaking a real reload. What is
/// under test is the probes reading it; making `list_active` fail would test
/// sqlx.
#[tokio::test]
async fn a_failed_reload_degrades_health_without_failing_readiness() {
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let degraded = state.reload_degraded.clone();
    let app = orion::server::build_router(state);

    // Baseline: nothing has failed, so the component is present and ok.
    let body = body_json(
        app.clone()
            .oneshot(json_request("GET", "/health", None))
            .await
            .expect("health"),
    )
    .await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["components"]["engine_reload"], "ok");

    degraded.store(true, std::sync::atomic::Ordering::Release);

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/health", None))
        .await
        .expect("health");
    // 200, not 503: the instance still serves every request it served before.
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["components"]["engine_reload"], "degraded");
    assert_eq!(
        body["status"], "degraded",
        "a stale engine must reach the top-level status, or a monitor keying on \
         it sees nothing: {body}"
    );

    let resp = app
        .oneshot(json_request("GET", "/readyz", None))
        .await
        .expect("readyz");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a node serving the previous generation is still serving"
    );
    assert_eq!(body_json(resp).await["status"], "ready");
}

/// The other half of a failed reload, and the half that regressed: the manual
/// route has to tell its caller.
///
/// `POST /engine/reload` has no committed write for an error to misdescribe.
/// That is what separates it from an activate or an archive, where the row is
/// already live and a `5xx` invites a retry that writes a second version or
/// collides with the first — so those degrade `/health` and answer `2xx`. A
/// caller who *asked* for a reload and did not get one must not read
/// `{"reloaded": true}`: a deploy pipeline gating on this route would call a
/// rollout landed while the node still serves the previous generation.
///
/// Unlike the test above, this one breaks a real reload rather than setting the
/// flag, because the flag is not what is under test — the propagation out of
/// `audit_and_reload` is, and only a genuine failure exercises it.
#[tokio::test]
async fn a_failed_manual_reload_is_reported_to_the_caller() {
    let (state, pool) = common::test_state_and_pool(orion::config::AppConfig::default()).await;
    let degraded = state.reload_degraded.clone();
    let app = orion::server::build_router(state);

    // Baseline: the route works, so the assertion below is about the failure
    // and not about the request never having been routed.
    let resp = app
        .clone()
        .oneshot(json_request("POST", "/api/v2/admin/engine/reload", None))
        .await
        .expect("reload");
    assert_eq!(resp.status(), StatusCode::OK);

    // Take the database away. That is the reload's remaining failure mode: an
    // unusable workflow or channel row is quarantined rather than raised, by
    // design, so there is nothing else to break.
    match &pool {
        orion::storage::DbPool::Sqlite(p) => p.close().await,
        _ => unreachable!("the integration harness is SQLite"),
    }

    let resp = app
        .clone()
        .oneshot(json_request("POST", "/api/v2/admin/engine/reload", None))
        .await
        .expect("reload");
    let status = resp.status();
    assert!(
        status.is_server_error(),
        "a reload that did not happen must not answer {status}"
    );
    let body = body_json(resp).await;
    assert!(
        body["data"].is_null(),
        "a failed reload must not carry a success envelope: {body}"
    );

    // And the degradation is still raised. The two reports are not
    // alternatives: the pipeline learns from the response, a dashboard
    // watching a fleet learns from `/health`.
    assert!(
        degraded.load(std::sync::atomic::Ordering::Acquire),
        "the failure must reach /health as well as the caller"
    );
}
