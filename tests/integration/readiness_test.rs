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
    state.kafka_ingest_status.set_degraded(true);

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
    state.kafka_ingest_status.set_degraded(false);
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
    assert!(state.kafka_ingest_status.is_degraded());
    assert!(
        state.kafka_ingest_status.supervisor_active(),
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
        state.kafka_consumer_handle.lock().await.is_some(),
        "boot must start a consumer for the configured topic"
    );

    // Simulate K7's failure mode: a reload shut the consumer down and the
    // restart failed — handle gone, degraded flagged.
    let old_handle = state
        .kafka_consumer_handle
        .lock()
        .await
        .take()
        .expect("handle present");
    old_handle.shutdown().await;
    state.kafka_ingest_status.set_degraded(true);

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/readyz", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

    // The supervisor retries (first attempt after ~1s), restores the
    // consumer, and clears the flag.
    orion::engine::spawn_kafka_restart_supervisor(&state);

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
        state.kafka_consumer_handle.lock().await.is_some(),
        "the supervisor must restore the consumer handle"
    );
    assert!(!state.kafka_ingest_status.is_degraded());
    // The supervisor releases its slot *before* clearing the degraded flag
    // (both while still holding the handle mutex — the TOCTOU fix), so once
    // the flag reads false the slot must read free.
    assert!(
        !state.kafka_ingest_status.supervisor_active(),
        "a stood-down supervisor must have released its slot"
    );

    // Drain the restored consumer so the test leaves nothing running.
    if let Some(handle) = state.kafka_consumer_handle.lock().await.take() {
        handle.shutdown().await;
    }
}
