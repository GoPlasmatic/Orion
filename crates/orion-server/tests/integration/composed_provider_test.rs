//! A Direct Rust provider an edition compiles into its binary (an Enterprise
//! `com.orion.enterprise.*` one), end to end, with no package uploaded:
//! `/runtime-capabilities` reports the digest its connections name, a
//! connection revision over it is validated by the provider itself at upload,
//! and a workflow reaches it through `connector_call`. When a new revision
//! replaces one, the provider is told to dispose of the old; at shutdown, to
//! flush.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::http::StatusCode;
use orion_extension_api::{
    AttemptContext, ConnectorError, ConnectorOutput, EffectiveConnectionIdentity,
    ImplementationKind, ProviderBackend,
};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common::{self, body_json, deploy, json_request};

const DRIVER: &str = "com.example.echo";

/// What the node told the provider about its lifecycle.
#[derive(Default)]
struct Lifecycle {
    disposed: Mutex<Vec<(String, u64)>>,
    shut_down: AtomicBool,
}

/// Answers `echo` with its input and the connection's `greeting`.
struct EchoProvider(Arc<Lifecycle>);

#[async_trait]
impl ProviderBackend for EchoProvider {
    fn kind(&self) -> ImplementationKind {
        ImplementationKind::Rust
    }

    fn driver_id(&self) -> &str {
        DRIVER
    }

    fn validate_config(&self, config: &Value) -> Result<(), ConnectorError> {
        match config.get("greeting").and_then(Value::as_str) {
            Some(greeting) if !greeting.is_empty() => Ok(()),
            _ => Err(ConnectorError::invalid_input(
                "a greeting is required",
                Some(json!({ "pointer": "/greeting" })),
            )),
        }
    }

    async fn invoke(
        &self,
        ctx: &AttemptContext,
        operation: &str,
        input: Value,
    ) -> Result<ConnectorOutput, ConnectorError> {
        assert_eq!(operation, "echo");
        let greeting = ctx
            .public_config()
            .and_then(|c| c["greeting"].as_str())
            .unwrap_or_default()
            .to_string();
        Ok(ConnectorOutput::new(
            json!({ "greeting": greeting, "echo": input }),
        ))
    }

    async fn dispose(&self, identity: &EffectiveConnectionIdentity) {
        self.0.disposed.lock().expect("lock").push((
            identity.connection_id.as_str().to_string(),
            identity.revision.get(),
        ));
    }

    async fn shutdown(&self, _deadline: Instant) {
        self.0.shut_down.store(true, Ordering::SeqCst);
    }
}

async fn node() -> (
    orion::runtime::state::AppState,
    axum::Router,
    Arc<Lifecycle>,
) {
    let lifecycle = Arc::new(Lifecycle::default());
    let mut config = orion::config::AppConfig::default();
    config.trace_storage.mode = orion::config::TraceStorageMode::Sync;
    let composition = orion::extensions::CompositionRoot::community()
        .with_provider_backend(Arc::new(EchoProvider(lifecycle.clone())));
    let state = common::test_state_with_composition(config, composition).await;
    let app = orion::server::build_router(state.clone());
    (state, app, lifecycle)
}

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(json_request(method, uri, body))
        .await
        .expect("the node answers");
    let status = response.status();
    (status, body_json(response).await)
}

#[tokio::test]
async fn an_edition_provider_serves_a_connection_without_a_package() {
    let (state, app, lifecycle) = node().await;

    // The digest a connection over the compiled-in driver names.
    let (status, caps) = send(&app, "GET", "/api/v2/admin/runtime-capabilities", None).await;
    assert_eq!(status, StatusCode::OK, "{caps}");
    let digest = caps["provider_backends"]
        .as_array()
        .and_then(|b| b.iter().find(|b| b["driver_id"] == DRIVER))
        .and_then(|b| b["package_digest"].as_str())
        .unwrap_or_else(|| panic!("the composed driver reports its digest: {caps}"))
        .to_string();

    // The provider judges the configuration at upload, field by field.
    let (status, refused) = send(
        &app,
        "POST",
        "/api/v2/admin/connections/greeter/revisions",
        Some(json!({
            "revision": 1,
            "provider_digest": digest,
            "public_config": { "greeting": "" },
            "secret_locators": {}
        })),
    )
    .await;
    assert!(status.is_client_error(), "{status}: {refused}");
    assert!(
        refused["error"]["details"]
            .as_array()
            .is_some_and(|d| d.iter().any(|e| e["path"] == "public_config.greeting")),
        "the refusal names the field the provider pointed at: {refused}"
    );

    let (status, created) = send(
        &app,
        "POST",
        "/api/v2/admin/connections/greeter/revisions",
        Some(json!({
            "revision": 1,
            "provider_digest": digest,
            "public_config": { "greeting": "hello" },
            "secret_locators": {}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let configuration_digest = created["connection"]["configuration_digest"]
        .as_str()
        .expect("a digest")
        .to_string();

    // A workflow that calls it, behind an HTTP channel, deployed as one set.
    let workflow = deploy::upload(
        &app,
        json!({ "kind": "workflow", "definition": {
            "name": "greet",
            "tasks": [
                { "id": "parse", "function": { "name": "parse_json",
                    "input": { "source": "payload", "target": "input" } } },
                { "id": "call", "function": { "name": "connector_call", "input": {
                    "connection": "greeter",
                    "target_id": DRIVER,
                    "operation": "echo",
                    "input": { "name": { "var": "data.input.name" } },
                    "output": "data.reply"
                } } }
            ]
        } }),
    )
    .await;
    let channel = deploy::upload(
        &app,
        deploy::channel_definition(&common::sync_http_channel("greet", "greet")).0,
    )
    .await;
    deploy::deploy(
        &app,
        vec![
            deploy::Upsert {
                kind: "connection",
                id: "greeter".into(),
                digest: configuration_digest,
                revision: Some(1),
                requires: Vec::new(),
            },
            deploy::Upsert {
                kind: "workflow",
                id: "greet".into(),
                digest: workflow,
                revision: None,
                requires: vec![("connection", "greeter".into())],
            },
            deploy::Upsert {
                kind: "channel",
                id: "greet".into(),
                digest: channel,
                revision: None,
                requires: vec![("workflow", "greet".into())],
            },
        ],
    )
    .await;

    let (status, body) = send(
        &app,
        "POST",
        "/api/v2/data/greet",
        Some(json!({ "data": { "name": "orion" } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["reply"]["greeting"], "hello", "{body}");
    assert!(
        lifecycle.disposed.lock().expect("lock").is_empty(),
        "nothing is retired while revision 1 is served"
    );

    // A new revision replaces revision 1: the provider is told to release it.
    let (status, created) = send(
        &app,
        "POST",
        "/api/v2/admin/connections/greeter/revisions",
        Some(json!({
            "revision": 2,
            "provider_digest": digest,
            "public_config": { "greeting": "hi" },
            "secret_locators": {}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    deploy::deploy(
        &app,
        vec![deploy::Upsert {
            kind: "connection",
            id: "greeter".into(),
            digest: created["connection"]["configuration_digest"]
                .as_str()
                .expect("a digest")
                .to_string(),
            revision: Some(2),
            requires: Vec::new(),
        }],
    )
    .await;
    let (_, body) = send(
        &app,
        "POST",
        "/api/v2/data/greet",
        Some(json!({ "data": { "name": "orion" } })),
    )
    .await;
    assert_eq!(body["data"]["reply"]["greeting"], "hi", "{body}");
    assert_eq!(
        *lifecycle.disposed.lock().expect("lock"),
        [("greeter".to_string(), 1)]
    );

    // Shutdown reaches every provider.
    state
        .composition
        .providers()
        .shutdown(Instant::now() + Duration::from_secs(5))
        .await;
    assert!(lifecycle.shut_down.load(Ordering::SeqCst));
}
