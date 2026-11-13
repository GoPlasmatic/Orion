//! An edition's channel driver that runs its own listener, end to end: the
//! node starts it at boot, reconciles it on every deployment, runs each
//! delivery it dispatches through the channel's guards and workflow, reports
//! it on `/health` and `/readyz`, and stops it at shutdown.
//!
//! The driver here is a stand-in for an Enterprise one (Kafka): it records
//! what the host hands it, and the tests dispatch through the host it was
//! started with, exactly as its consume loop would.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::http::StatusCode;
use orion_extension_api::{
    ChannelBinding, ChannelDriver, ChannelDriverCapabilities, ChannelProtocolFields, Delivery,
    DispatchOutcome, DriverError, DriverHealth, FieldError, GuardSet, IngressHost, RunningDriver,
};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common::{self, body_json, json_request};

const PROTOCOL: &str = "queue";

/// What the driver has been handed, for the tests to read.
#[derive(Default)]
struct Probe {
    host: Mutex<Option<Arc<dyn IngressHost>>>,
    reconciles: Mutex<Vec<Vec<ChannelBinding>>>,
    shut_down: AtomicBool,
}

impl Probe {
    fn host(&self) -> Arc<dyn IngressHost> {
        self.host
            .lock()
            .expect("probe lock")
            .clone()
            .expect("the driver was started")
    }

    fn latest(&self) -> Option<Vec<ChannelBinding>> {
        self.reconciles.lock().expect("probe lock").last().cloned()
    }

    /// Wait until the latest reconcile satisfies `pred`.
    async fn wait_for(&self, pred: impl Fn(&[ChannelBinding]) -> bool) -> Vec<ChannelBinding> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(latest) = self.latest()
                && pred(&latest)
            {
                return latest;
            }
            assert!(
                Instant::now() < deadline,
                "the driver was never reconciled to the expected set; last: {:?}",
                self.latest()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

struct QueueDriver(Arc<Probe>);

#[async_trait]
impl ChannelDriver for QueueDriver {
    fn protocol_id(&self) -> &str {
        PROTOCOL
    }

    fn capabilities(&self) -> ChannelDriverCapabilities {
        ChannelDriverCapabilities::none()
            .async_submit()
            .ingress(GuardSet::none().validation().deduplication().backpressure())
    }

    fn validate_fields(&self, _fields: &ChannelProtocolFields<'_>) -> Vec<FieldError> {
        Vec::new()
    }

    /// One topic, one channel: the rule `validate_set` exists for.
    fn validate_set(&self, channels: &[ChannelBinding]) -> Vec<FieldError> {
        let mut seen: std::collections::BTreeMap<String, String> = Default::default();
        let mut out = Vec::new();
        for channel in channels {
            let Some(topic) = channel.transport_config()["topic"].as_str() else {
                continue;
            };
            if let Some(first) = seen.insert(topic.to_string(), channel.channel_id().to_string()) {
                out.push(FieldError::new(
                    format!("channel.{}.transport_config.topic", channel.channel_id()),
                    "CONFLICT",
                    format!("topic '{topic}' is already consumed by channel '{first}'"),
                ));
            }
        }
        out
    }

    fn transport_schema(&self) -> Option<Value> {
        Some(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "required": ["topic"],
            "properties": {
                "topic": { "type": "string", "minLength": 1 },
                "connection": { "type": "string" }
            },
            "additionalProperties": false
        }))
    }

    async fn start(
        &self,
        host: Arc<dyn IngressHost>,
    ) -> Result<Option<Box<dyn RunningDriver>>, DriverError> {
        *self.0.host.lock().expect("probe lock") = Some(host);
        Ok(Some(Box::new(Running(self.0.clone()))))
    }
}

struct Running(Arc<Probe>);

#[async_trait]
impl RunningDriver for Running {
    async fn reconcile(&self, channels: Vec<ChannelBinding>) {
        self.0.reconciles.lock().expect("probe lock").push(channels);
    }

    fn health(&self) -> DriverHealth {
        DriverHealth::Ok
    }

    async fn shutdown(&self, _deadline: Instant) {
        self.0.shut_down.store(true, Ordering::SeqCst);
    }
}

async fn node() -> (orion::runtime::state::AppState, axum::Router, Arc<Probe>) {
    let probe = Arc::new(Probe::default());
    let mut config = orion::config::AppConfig::default();
    config.trace_storage.mode = orion::config::TraceStorageMode::Sync;
    let composition = orion::extensions::CompositionRoot::community()
        .with_channel_driver(Arc::new(QueueDriver(probe.clone())));
    let state = common::test_state_with_composition(config, composition).await;
    let app = orion::server::build_router(state.clone());
    (state, app, probe)
}

/// Echoes the payload and what the workflow learns about its delivery.
fn echo_workflow() -> Value {
    common::workflow_with_tasks(
        "Queue Echo",
        json!([
            {
                "id": "parse",
                "name": "Parse the delivered payload",
                "function": {
                    "name": "parse_json",
                    "input": { "source": "payload", "target": "input" }
                }
            },
            {
                "id": "echo",
                "name": "Echo the trigger",
                "function": {
                    "name": "map",
                    "input": {
                        "mappings": [
                            { "path": "data.order", "logic": { "var": "data.input.order_id" } },
                            { "path": "data.saw_type", "logic": { "var": "metadata.trigger.type" } },
                            { "path": "data.saw_topic", "logic": { "var": "metadata.trigger.topic" } },
                            { "path": "data.saw_channel", "logic": { "var": "metadata.channel" } }
                        ]
                    }
                }
            }
        ]),
    )
}

/// Deploy the echo workflow and a `queue` channel bound to it.
async fn deploy_orders(app: &axum::Router, topic: &str) -> String {
    let workflow_id = common::create_and_activate_workflow(app, echo_workflow()).await;
    common::deploy_channel(
        app,
        json!({
            "name": "orders",
            "channel_type": "async",
            "protocol": PROTOCOL,
            "workflow_id": workflow_id,
            "transport_config": { "topic": topic },
            "config": {
                "validation_logic": { "!!": [{ "var": "data.order_id" }] },
                "deduplication": { "header": "x-idempotency-key", "window_secs": 60 }
            }
        }),
    )
    .await
}

fn trigger(topic: &str) -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    map.insert("topic".into(), json!(topic));
    map.insert("offset".into(), json!(41));
    map
}

#[tokio::test]
async fn a_deployed_channel_reaches_its_driver_and_a_delivery_runs_its_workflow() {
    let (_state, app, probe) = node().await;
    let channel_id = deploy_orders(&app, "orders.placed").await;

    let bindings = probe
        .wait_for(|set| set.iter().any(|b| b.channel_id() == channel_id))
        .await;
    assert_eq!(bindings.len(), 1, "{bindings:?}");
    assert_eq!(bindings[0].name(), "orders");
    assert_eq!(
        bindings[0].transport_config(),
        &json!({ "topic": "orders.placed" })
    );
    assert!(bindings[0].fingerprint().starts_with("sha256:"));

    let dispatched = probe
        .host()
        .dispatch(
            Delivery::new(&channel_id, json!({ "order_id": 7 }))
                .with_trigger(trigger("orders.placed")),
        )
        .await;
    let DispatchOutcome::Completed { output } = dispatched.outcome else {
        panic!("a valid delivery completes: {:?}", dispatched.outcome);
    };
    assert_eq!(output["order"], 7, "{output}");
    assert_eq!(output["saw_type"], PROTOCOL, "the host stamps the protocol");
    assert_eq!(
        output["saw_topic"], "orders.placed",
        "the driver's facts arrive"
    );
    assert_eq!(output["saw_channel"], "orders");
    dispatched.settle.committed().await;

    // The run is traced under the protocol, with its input.
    let response = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v2/admin/traces?channel=orders&limit=5",
            None,
        ))
        .await
        .expect("trace listing answers");
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let rows = body["data"].as_array().expect("a trace page");
    assert_eq!(rows.len(), 1, "{body}");
    assert_eq!(rows[0]["mode"], PROTOCOL, "{body}");
}

#[tokio::test]
async fn the_channels_guards_decide_the_outcome() {
    let (_state, app, probe) = node().await;
    let channel_id = deploy_orders(&app, "orders.placed").await;
    probe
        .wait_for(|set| set.iter().any(|b| b.channel_id() == channel_id))
        .await;
    let host = probe.host();

    // `validation_logic` refuses a delivery with no order id: terminal.
    let refused = host
        .dispatch(Delivery::new(&channel_id, json!({ "sku": "x" })))
        .await;
    assert!(
        matches!(refused.outcome, DispatchOutcome::Rejected { .. }),
        "{:?}",
        refused.outcome
    );

    // A settled key is a duplicate for a different physical delivery.
    let first = host
        .dispatch(
            Delivery::new(&channel_id, json!({ "order_id": 1 }))
                .with_dedup_key("order-1")
                .with_dedup_owner("queue:orders/0/1"),
        )
        .await;
    assert!(matches!(first.outcome, DispatchOutcome::Completed { .. }));
    first.settle.committed().await;
    let replay = host
        .dispatch(
            Delivery::new(&channel_id, json!({ "order_id": 1 }))
                .with_dedup_key("order-1")
                .with_dedup_owner("queue:orders/0/2"),
        )
        .await;
    assert_eq!(replay.outcome, DispatchOutcome::Duplicate);

    // An abandoned delivery releases its key: the redelivery runs.
    let attempt = host
        .dispatch(
            Delivery::new(&channel_id, json!({ "order_id": 2 }))
                .with_dedup_key("order-2")
                .with_dedup_owner("queue:orders/0/3"),
        )
        .await;
    assert!(matches!(attempt.outcome, DispatchOutcome::Completed { .. }));
    attempt.settle.abandoned().await;
    let redelivery = host
        .dispatch(
            Delivery::new(&channel_id, json!({ "order_id": 2 }))
                .with_dedup_key("order-2")
                .with_dedup_owner("queue:orders/1/9"),
        )
        .await;
    assert!(
        matches!(redelivery.outcome, DispatchOutcome::Completed { .. }),
        "{:?}",
        redelivery.outcome
    );

    // A key another delivery holds, unsettled: nothing has completed yet, so
    // the second delivery is told to wait, not that it is a duplicate.
    let holder = host
        .dispatch(
            Delivery::new(&channel_id, json!({ "order_id": 3 }))
                .with_dedup_key("order-3")
                .with_dedup_owner("queue:orders/2/1"),
        )
        .await;
    assert!(matches!(holder.outcome, DispatchOutcome::Completed { .. }));
    let waiting = host
        .dispatch(
            Delivery::new(&channel_id, json!({ "order_id": 3 }))
                .with_dedup_key("order-3")
                .with_dedup_owner("queue:orders/3/1"),
        )
        .await;
    assert!(
        matches!(waiting.outcome, DispatchOutcome::Deferred { .. }),
        "{:?}",
        waiting.outcome
    );
    // Once the holder settles, the same key is a duplicate.
    holder.settle.committed().await;
    let after = host
        .dispatch(
            Delivery::new(&channel_id, json!({ "order_id": 3 }))
                .with_dedup_key("order-3")
                .with_dedup_owner("queue:orders/3/1"),
        )
        .await;
    assert_eq!(after.outcome, DispatchOutcome::Duplicate);

    // A channel this node does not serve: nothing runs.
    let nowhere = host
        .dispatch(Delivery::new("no-such-channel", json!({ "order_id": 3 })))
        .await;
    assert!(
        matches!(nowhere.outcome, DispatchOutcome::Unavailable { .. }),
        "{:?}",
        nowhere.outcome
    );
}

#[tokio::test]
async fn a_channel_hands_its_driver_the_connections_it_requires() {
    let (state, app, probe) = node().await;
    let connection_id = common::create_connector(
        &app,
        json!({
            "name": "events",
            "connector_type": "http",
            "config": { "type": "http", "url": "https://broker.example.com" }
        }),
    )
    .await;
    let workflow_id = common::create_and_activate_workflow(&app, echo_workflow()).await;
    let channel_id = common::deploy_channel(
        &app,
        json!({
            "name": "orders",
            "channel_type": "async",
            "protocol": PROTOCOL,
            "workflow_id": workflow_id,
            "transport_config": { "topic": "orders.placed", "connection": connection_id },
            "connections": [connection_id],
        }),
    )
    .await;

    let bindings = probe
        .wait_for(|set| {
            set.iter()
                .any(|b| b.channel_id() == channel_id && b.connection(&connection_id).is_some())
        })
        .await;
    let binding = &bindings[0];
    assert_eq!(
        binding.connection_ids().collect::<Vec<_>>(),
        [connection_id.as_str()]
    );
    let identity = binding
        .connection_identity(&connection_id)
        .expect("the identity travels with the resolver");
    assert_eq!(identity.connection_id.as_str(), connection_id);
    let resolver = binding.connection(&connection_id).expect("a resolver");
    assert_eq!(
        resolver.public_config().and_then(|c| c["url"].as_str()),
        Some("https://broker.example.com"),
        "the driver reads the revision the deployment pins"
    );
    let merged = resolver
        .resolve_merged_config()
        .await
        .expect("resolves")
        .expect("a governed connection has a config");
    assert_eq!(merged["url"], "https://broker.example.com");

    // A revocation reaches a resolver the driver already holds: a listener
    // that outlives it cannot keep reading the connection.
    state.composition.providers().ledger().revoke_connection(
        &orion_extension_api::ConnectionId::new(connection_id.as_str()),
    );
    let refused = resolver
        .resolve_merged_config()
        .await
        .expect_err("a revoked connection does not resolve");
    assert!(refused.message.contains("revoked"), "{refused:?}");
}

#[tokio::test]
async fn a_deployment_whose_channels_conflict_is_refused() {
    let (_state, app, _probe) = node().await;
    let workflow_id = common::create_and_activate_workflow(&app, echo_workflow()).await;
    common::deploy_channel(
        &app,
        json!({
            "name": "orders",
            "channel_type": "async",
            "protocol": PROTOCOL,
            "workflow_id": workflow_id,
            "transport_config": { "topic": "orders.placed" }
        }),
    )
    .await;

    // A second channel on the same topic: each is valid alone.
    let copy = common::deploy::upload(
        &app,
        common::deploy::channel_definition(&json!({
            "name": "orders-copy",
            "channel_type": "async",
            "protocol": PROTOCOL,
            "transport_config": { "topic": "orders.placed" }
        }))
        .0,
    )
    .await;
    let (base, members) = common::deploy::merged(
        &app,
        vec![common::deploy::Upsert {
            kind: "channel",
            id: "orders-copy".into(),
            digest: copy,
            revision: None,
            requires: vec![("workflow", workflow_id)],
        }],
    )
    .await;
    let (status, body) = common::deploy::apply(&app, base, members).await;
    assert!(status.is_client_error(), "{status}: {body}");
    let text = body.to_string();
    assert!(text.contains("DEPLOYMENT_WOULD_NOT_BUILD"), "{body}");
    assert!(text.contains("already consumed by channel"), "{body}");
}

#[tokio::test]
async fn a_changed_channel_reconciles_with_a_new_fingerprint() {
    let (_state, app, probe) = node().await;
    let channel_id = deploy_orders(&app, "orders.placed").await;
    let before = probe
        .wait_for(|set| set.iter().any(|b| b.channel_id() == channel_id))
        .await;

    deploy_orders(&app, "orders.v2").await;
    let after = probe
        .wait_for(|set| {
            set.iter()
                .any(|b| b.transport_config() == &json!({ "topic": "orders.v2" }))
        })
        .await;
    assert_eq!(after.len(), 1, "{after:?}");
    assert_ne!(before[0].fingerprint(), after[0].fingerprint());
}

#[tokio::test]
async fn the_driver_is_reported_on_health_and_readiness_and_stopped_at_shutdown() {
    let (state, app, probe) = node().await;
    // Started at boot, before any deployment.
    probe.wait_for(|_| true).await;

    let health = body_json(
        app.clone()
            .oneshot(json_request("GET", "/health", None))
            .await
            .expect("health answers"),
    )
    .await;
    assert_eq!(health["components"][PROTOCOL], "ok", "{health}");

    let ready = app
        .clone()
        .oneshot(json_request("GET", "/readyz", None))
        .await
        .expect("readyz answers");
    assert_eq!(ready.status(), StatusCode::OK);
    let ready = body_json(ready).await;
    assert_eq!(
        ready["components"]["channel_drivers"][PROTOCOL], "ok",
        "{ready}"
    );

    state.tasks.shutdown(Duration::from_secs(5)).await;
    assert!(probe.shut_down.load(Ordering::SeqCst));
}

#[tokio::test]
async fn runtime_capabilities_describe_the_driver_and_upload_enforces_its_schema() {
    let (_state, app, _probe) = node().await;
    let caps = body_json(
        app.clone()
            .oneshot(json_request(
                "GET",
                "/api/v2/admin/runtime-capabilities",
                None,
            ))
            .await
            .expect("capabilities answer"),
    )
    .await;
    let info = caps["channel_driver_info"]
        .as_array()
        .expect("drivers described")
        .iter()
        .find(|d| d["protocol"] == PROTOCOL)
        .cloned()
        .unwrap_or_else(|| panic!("the composed driver is described: {caps}"));
    assert_eq!(info["supports_async"], true);
    assert_eq!(
        info["ingress_guards"],
        json!({ "rate_limit": false, "validation": true, "deduplication": true, "backpressure": true })
    );
    assert_eq!(info["transport_schema"]["required"], json!(["topic"]));

    // The node holds a channel to the schema it publishes.
    let response = app
        .oneshot(json_request(
            "POST",
            "/api/v2/admin/definitions",
            Some(json!({
                "kind": "channel",
                "definition": {
                    "name": "orders",
                    "channel_type": "async",
                    "protocol": PROTOCOL,
                    "transport_config": { "topics": ["orders"] }
                }
            })),
        ))
        .await
        .expect("definition upload answers");
    assert!(response.status().is_client_error(), "{}", response.status());
    let body = body_json(response).await;
    assert!(
        body["error"]["details"]
            .as_array()
            .is_some_and(|d| d.iter().any(|e| e["path"]
                .as_str()
                .is_some_and(|p| p.contains("transport_config")))),
        "the refusal names transport_config: {body}"
    );
}

#[tokio::test]
async fn a_channel_configuring_a_guard_its_protocol_never_runs_is_refused() {
    let (_state, app, _probe) = node().await;
    let response = app
        .oneshot(json_request(
            "POST",
            "/api/v2/admin/definitions",
            Some(json!({
                "kind": "channel",
                "definition": {
                    "name": "orders",
                    "channel_type": "async",
                    "protocol": PROTOCOL,
                    "config": { "auth": { "type": "api_key", "keys": ["k"] } }
                }
            })),
        ))
        .await
        .expect("definition upload answers");
    assert!(
        response.status().is_client_error(),
        "an auth block on a queue channel is refused, got {}",
        response.status()
    );
    let body = body_json(response).await;
    let details = body["error"]["details"].as_array().expect("field errors");
    assert!(
        details.iter().any(|d| d["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("config.auth"))),
        "the refusal names the field: {body}"
    );
}
