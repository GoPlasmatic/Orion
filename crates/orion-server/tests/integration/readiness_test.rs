//! `/readyz` component reporting (D15, O10).
//!
//! The cluster-Redis probe only appears when cluster mode is on — a
//! single-node deployment has no shared Redis to be unready about. The
//! negative case (Redis unreachable → 503) needs a real container and lives
//! in `tests/cluster`. Likewise the `kafka` component appears only when
//! `kafka.enabled` is true; its degraded path (K7) is exercised here
//! without a broker via the shared `KafkaIngestStatus` signal.

use axum::http::StatusCode;
use tower::ServiceExt;

use crate::common::{self, body_json, json_request};

#[tokio::test]
async fn readyz_omits_cluster_redis_on_a_single_node() {
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
        "cluster_redis must be absent outside cluster mode: {body}"
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

/// O10: with Kafka disabled — the default — neither probe grows a `kafka`
/// component, so non-Kafka deployments are unaffected.
#[tokio::test]
async fn probes_omit_kafka_when_disabled() {
    let app = common::test_app().await;

    for path in ["/readyz", "/health"] {
        let resp = app
            .clone()
            .oneshot(json_request("GET", path, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert!(
            body["components"].get("kafka").is_none(),
            "{path} must not report kafka when it is disabled: {body}"
        );
    }
}

/// O10/K7: with Kafka enabled, `/readyz` reports the ingest consumer and
/// flips to 503 while the degraded flag is up; `/health` reports the same
/// component but stays 200 (HTTP still serves) with `status: degraded`.
#[tokio::test]
async fn degraded_kafka_ingest_flips_readyz_and_health() {
    // Enabled with no topics: the consumer is intentionally not started,
    // which must read as healthy — idle, not broken.
    let state =
        common::test_state_with_kafka(orion::config::AppConfig::default(), "127.0.0.1:1").await;
    let app = orion::server::build_router(state.clone());

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/readyz", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["components"]["kafka"], "ok", "{body}");

    // The K7 signal: a failed consumer restart flags ingestion as degraded.
    state.kafka.ingest_status.set_degraded(true);

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/readyz", None))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a node that consumes nothing must leave rotation"
    );
    let body = body_json(resp).await;
    assert_eq!(body["status"], "not_ready");
    assert_eq!(body["components"]["kafka"], "error", "{body}");

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/health", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "HTTP itself still serves");
    let body = body_json(resp).await;
    assert_eq!(body["status"], "degraded");
    assert_eq!(body["components"]["kafka"], "error", "{body}");

    // Recovery clears the signal.
    state.kafka.ingest_status.set_degraded(false);
    let resp = app
        .oneshot(json_request("GET", "/readyz", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// K7 defect path: the reload error arm itself
/// (`restart_kafka_consumer_if_needed` in src/engine/reload.rs) — a
/// consumer restart that fails during an engine reload must flag ingestion
/// degraded (503 `/readyz`, `kafka: error`) and claim the supervisor slot,
/// not just log. The failure is injected locally, no broker involved:
/// `session.timeout.ms` is set only on the consumer client (K9 — the
/// producer never sets it), and `0` is outside librdkafka's allowed range,
/// so the boot-time producer is created fine while consumer client
/// creation fails the moment the reload-driven restart tries to start one.
#[tokio::test]
async fn failed_consumer_restart_on_reload_degrades_readiness() {
    let mut config = orion::config::AppConfig::default();
    // No config-file topics: boot's merged topic list is empty, so no
    // consumer client is created at boot. The invalid value is first hit
    // by the reload below.
    config.kafka.session_timeout_ms = 0;
    let state = common::test_state_with_kafka(config, "127.0.0.1:1").await;
    let app = orion::server::build_router(state.clone());

    // Healthy before the reload: Kafka enabled with nothing to consume is
    // idle, not broken.
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/readyz", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Activating an async Kafka channel changes the merged topic set, so
    // the reload takes the full-restart path and consumer creation fails.
    let workflow_id = common::create_and_activate_workflow(
        &app,
        common::simple_log_workflow("K7 Defect Workflow"),
    )
    .await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(serde_json::json!({
                "name": "k7-defect-channel",
                "channel_type": "async",
                "protocol": "kafka",
                "topic": "k7-defect-topic",
                "workflow_id": workflow_id,
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let channel_id = body_json(resp).await["data"]["channel_id"]
        .as_str()
        .expect("channel_id")
        .to_string();
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{channel_id}/status"),
            Some(serde_json::json!({"status": "active"})),
        ))
        .await
        .unwrap();
    // The activation succeeds: a consumer start failure degrades ingestion,
    // it must not fail the reload (and with it the activation).
    assert_eq!(resp.status(), StatusCode::OK);

    // The error arm ran: degraded flagged and the supervisor slot claimed —
    // both happen synchronously before the PATCH returns.
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/readyz", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "not_ready");
    assert_eq!(body["components"]["kafka"], "error", "{body}");
    assert!(state.kafka.ingest_status.is_degraded());
    assert!(
        state.kafka.ingest_status.supervisor_active(),
        "the failed restart must hand recovery to the supervisor"
    );

    // Drain, so the supervisor — whose retries can never succeed against
    // the invalid value — stands down instead of outliving the test.
    state
        .ready
        .store(false, std::sync::atomic::Ordering::Release);
}

/// K7: the restart supervisor brings a downed consumer back and clears the
/// degraded flag. No broker needed — rdkafka client creation and subscribe
/// are local, so a consumer against an unreachable address "starts"
/// successfully (parity with production, where brokers may come online
/// later; the boot-time connectivity probe is non-fatal for that reason).
#[tokio::test]
async fn restart_supervisor_recovers_a_downed_consumer() {
    let mut config = orion::config::AppConfig::default();
    // A config-file topic mapping so the merged topic list is non-empty and
    // a consumer is actually wanted.
    config.kafka.topics = vec![orion::config::TopicMapping {
        topic: "supervisor-topic".to_string(),
        channel: "supervisor-channel".to_string(),
    }];
    let state = common::test_state_with_kafka(config, "127.0.0.1:1").await;
    let app = orion::server::build_router(state.clone());

    // Boot started a consumer for the configured topic.
    assert!(
        state.kafka.consumer_handle.lock().await.is_some(),
        "boot must start a consumer for the configured topic"
    );

    // Simulate K7's failure mode: a reload shut the consumer down and the
    // restart failed — handle gone, degraded flagged.
    let old_handle = state
        .kafka
        .consumer_handle
        .lock()
        .await
        .take()
        .expect("handle present");
    old_handle.shutdown().await;
    state.kafka.ingest_status.set_degraded(true);

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/readyz", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

    // The supervisor retries (first attempt after ~1s), restores the
    // consumer, and clears the flag.
    orion::runtime::spawn_kafka_restart_supervisor(&state);

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let resp = app
            .clone()
            .oneshot(json_request("GET", "/readyz", None))
            .await
            .unwrap();
        if resp.status() == StatusCode::OK {
            let body = body_json(resp).await;
            assert_eq!(body["components"]["kafka"], "ok", "{body}");
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "supervisor did not restore the consumer within 30s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert!(
        state.kafka.consumer_handle.lock().await.is_some(),
        "the supervisor must restore the consumer handle"
    );
    assert!(!state.kafka.ingest_status.is_degraded());
    // The supervisor releases its slot *before* clearing the degraded flag
    // (both while still holding the handle mutex — the TOCTOU fix), so once
    // the flag reads false the slot must read free.
    assert!(
        !state.kafka.ingest_status.supervisor_active(),
        "a stood-down supervisor must have released its slot"
    );

    // Drain the restored consumer so the test leaves nothing running.
    if let Some(handle) = state.kafka.consumer_handle.lock().await.take() {
        handle.shutdown().await;
    }
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
        .oneshot(json_request("POST", "/api/v1/admin/engine/reload", None))
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
        .oneshot(json_request("POST", "/api/v1/admin/engine/reload", None))
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
