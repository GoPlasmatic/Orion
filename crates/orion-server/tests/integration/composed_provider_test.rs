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
    ImplementationKind, OperationDescription, ProviderBackend, ProviderDescription,
};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common::{self, body_json, deploy, json_request};

const DRIVER: &str = "com.example.echo";
const FETCHER: &str = "com.example.fetcher";

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
        if operation == "fail" {
            return Err(failure(input["mode"].as_str().unwrap_or_default()));
        }
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

    fn describe(&self) -> Option<ProviderDescription> {
        Some(
            ProviderDescription::new()
                .with_operation(
                    "echo",
                    OperationDescription::new()
                        .with_alias("say")
                        .with_input_schema(json!({
                            "$schema": "https://json-schema.org/draft/2020-12/schema",
                            "type": "object",
                            "required": ["name"],
                            "properties": { "name": { "type": "string" } }
                        }))
                        .with_output_schema(json!({
                            "$schema": "https://json-schema.org/draft/2020-12/schema",
                            "type": "object",
                            "required": ["greeting"]
                        })),
                )
                .with_operation("fail", OperationDescription::new()),
        )
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

/// The failure `fail` answers for `mode`. Each message names a host, which
/// must never reach a caller.
fn failure(mode: &str) -> ConnectorError {
    use orion_extension_api::{ConnectorErrorClass, EffectStatus, RetryAdvice};
    match mode {
        "unavailable" => ConnectorError::new(
            "ECHO_UNAVAILABLE",
            ConnectorErrorClass::Unavailable,
            "echo backend at db-7 is draining",
            EffectStatus::NotStarted,
        )
        .with_retry_advice(RetryAdvice::Transient)
        .with_retry_after_ms(2_000),
        _ => ConnectorError::new(
            "ECHO_REFUSED",
            ConnectorErrorClass::ProviderFailure,
            "echo refused by host db-7",
            EffectStatus::NotStarted,
        ),
    }
}

/// Declares `store`, a dependency on an echo connection granting `echo`, and
/// answers `fetch` by calling `input.op` (default `echo`) on it.
struct FetcherProvider;

#[async_trait]
impl ProviderBackend for FetcherProvider {
    fn kind(&self) -> ImplementationKind {
        ImplementationKind::Rust
    }

    fn driver_id(&self) -> &str {
        FETCHER
    }

    fn validate_config(&self, _config: &Value) -> Result<(), ConnectorError> {
        Ok(())
    }

    async fn invoke(
        &self,
        ctx: &AttemptContext,
        _operation: &str,
        input: Value,
    ) -> Result<ConnectorOutput, ConnectorError> {
        let op = input["op"].as_str().unwrap_or("echo").to_string();
        let bound = ctx
            .bound_connection("store")
            .map(|c| json!({ "id": c.connection_id.as_str(), "revision": c.revision.get() }));
        let reply = ctx
            .call_dependency("store", &op, json!({ "name": input["name"] }))
            .await?;
        Ok(ConnectorOutput::new(
            json!({ "via": reply.data, "bound": bound }),
        ))
    }

    /// The probe reaches the dependency as a call would.
    async fn health_check(&self, ctx: &AttemptContext) -> Result<(), ConnectorError> {
        ctx.call_dependency("store", "echo", json!({ "name": "probe" }))
            .await
            .map(|_| ())
    }

    fn describe(&self) -> Option<ProviderDescription> {
        Some(
            ProviderDescription::new()
                .with_operation("fetch", OperationDescription::new())
                .with_dependency(
                    "store",
                    orion_extension_api::DependencyDescription::new()
                        .on_provider(DRIVER)
                        .with_operation("echo"),
                ),
        )
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
        .with_provider_backend(Arc::new(EchoProvider(lifecycle.clone())))
        .with_provider_backend(Arc::new(FetcherProvider));
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
    let backend = caps["provider_backends"]
        .as_array()
        .and_then(|b| b.iter().find(|b| b["driver_id"] == DRIVER))
        .expect("listed");
    assert_eq!(
        backend["operations"]["echo"]["aliases"],
        json!(["say"]),
        "{backend}"
    );
    assert_eq!(
        backend["operations"]["echo"]["input_schema"]["required"],
        json!(["name"])
    );

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

    // The declared input schema holds on every call: no name, no dispatch.
    let (_, refused) = send(
        &app,
        "POST",
        "/api/v2/data/greet",
        Some(json!({ "data": {} })),
    )
    .await;
    assert!(
        refused.to_string().contains("input_schema"),
        "the call is refused against the declared schema: {refused}"
    );

    // An operation the provider does not declare is refused at upload, at
    // the field that names it.
    let (status, refused) = deploy::try_upload(
        &app,
        json!({ "kind": "workflow", "definition": {
            "name": "greet-typo",
            "tasks": [
                { "id": "call", "function": { "name": "connector_call", "input": {
                    "connection": "greeter",
                    "target_id": DRIVER,
                    "operation": "ecoh",
                    "input": { "name": "x" },
                    "output": "data.reply"
                } } }
            ]
        } }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        refused["error"]["details"]
            .as_array()
            .is_some_and(|d| d.iter().any(|e| {
                e["path"]
                    .as_str()
                    .is_some_and(|p| p.ends_with("input.operation"))
                    && e["message"]
                        .as_str()
                        .is_some_and(|m| m.contains("no operation 'ecoh'"))
            })),
        "{refused}"
    );
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

/// Deploy revision 1 of the `greeter` connection, a workflow `fails` that
/// calls the provider's `fail` operation with `data.mode`, and a sync HTTP
/// channel `fails` bound to it.
async fn deploy_failing(app: &axum::Router) {
    let (_, caps) = send(app, "GET", "/api/v2/admin/runtime-capabilities", None).await;
    let digest = caps["provider_backends"]
        .as_array()
        .and_then(|b| b.iter().find(|b| b["driver_id"] == DRIVER))
        .and_then(|b| b["package_digest"].as_str())
        .expect("the composed driver reports its digest")
        .to_string();
    let (status, created) = send(
        app,
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
    let workflow = deploy::upload(
        app,
        json!({ "kind": "workflow", "definition": {
            "name": "fails",
            "tasks": [
                { "id": "call", "function": { "name": "connector_call", "input": {
                    "connection": "greeter",
                    "target_id": DRIVER,
                    "operation": "fail",
                    "input": { "mode": { "var": "data.mode" } },
                    "output": "data.reply"
                } } }
            ]
        } }),
    )
    .await;
    let channel = deploy::upload(
        app,
        deploy::channel_definition(&common::sync_http_channel("fails", "fails")).0,
    )
    .await;
    deploy::deploy(
        app,
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
                id: "fails".into(),
                digest: workflow,
                revision: None,
                requires: vec![("connection", "greeter".into())],
            },
            deploy::Upsert {
                kind: "channel",
                id: "fails".into(),
                digest: channel,
                revision: None,
                requires: vec![("workflow", "fails".into())],
            },
        ],
    )
    .await;
}

/// A provider's own failure reaches the caller as what it is: a 502 naming
/// the provider's code, not an opaque engine error, and never the provider's
/// message. An unavailable provider is a 503 the caller can retry, with the
/// provider's hint. The trace keeps the raw text for the operator.
#[tokio::test]
async fn a_provider_failure_answers_with_its_class_and_code() {
    let (_state, app, _lifecycle) = node().await;
    deploy_failing(&app).await;

    let (status, body) = send(
        &app,
        "POST",
        "/api/v2/data/fails",
        Some(json!({ "data": { "mode": "provider" } })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(
        body["error"]["code"], "CONNECTOR_PROVIDER_FAILURE",
        "{body}"
    );
    assert_eq!(body["error"]["connector"]["provider_code"], "ECHO_REFUSED");
    assert_eq!(body["error"]["connector"]["class"], "provider_failure");
    assert!(!body.to_string().contains("db-7"), "{body}");

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/fails",
            Some(json!({ "data": { "mode": "unavailable" } })),
        ))
        .await
        .expect("the node answers");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.headers()[axum::http::header::RETRY_AFTER],
        "2",
        "the provider's hint"
    );
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "CONNECTOR_PROVIDER_UNAVAILABLE");
    assert_eq!(
        body["error"]["connector"]["provider_code"],
        "ECHO_UNAVAILABLE"
    );
    assert_eq!(body["error"]["connector"]["retryable"], true);
    assert!(!body.to_string().contains("db-7"), "{body}");

    // The operator's trace keeps what the caller was not told.
    let (_, traces) = send(
        &app,
        "GET",
        "/api/v2/admin/traces?channel=fails&limit=5",
        None,
    )
    .await;
    let messages: Vec<&str> = traces["data"]
        .as_array()
        .expect("a trace page")
        .iter()
        .filter_map(|t| t["error_message"].as_str())
        .collect();
    assert!(
        messages.iter().any(
            |m| m.starts_with("CONNECTOR_PROVIDER_FAILURE (ECHO_REFUSED): ") && m.contains("db-7")
        ),
        "{traces}"
    );
}

/// A provider failure inside a channel the caller reached through
/// `channel_call` answers the outer caller the same way.
#[tokio::test]
async fn a_nested_provider_failure_reaches_the_outer_caller() {
    let (_state, app, _lifecycle) = node().await;
    deploy_failing(&app).await;
    common::create_and_activate_channel(
        &app,
        "outer",
        json!({
            "name": "Outer",
            "condition": true,
            "tasks": [{
                "id": "call",
                "name": "Call the failing channel",
                "function": {
                    "name": "channel_call",
                    "input": { "channel": "fails", "output": "data.inner" }
                }
            }]
        }),
    )
    .await;

    let (status, body) = send(
        &app,
        "POST",
        "/api/v2/data/outer",
        Some(json!({ "data": { "mode": "provider" } })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["error"]["connector"]["provider_code"], "ECHO_REFUSED");
    assert!(!body.to_string().contains("db-7"), "{body}");
}

/// The digest connections over the composed `driver` name.
async fn digest_of(app: &axum::Router, driver: &str) -> String {
    let (_, caps) = send(app, "GET", "/api/v2/admin/runtime-capabilities", None).await;
    caps["provider_backends"]
        .as_array()
        .and_then(|b| b.iter().find(|b| b["driver_id"] == driver))
        .and_then(|b| b["package_digest"].as_str())
        .unwrap_or_else(|| unreachable!("{driver} is composed: {caps}"))
        .to_string()
}

/// Store `revision` of `connection` over `driver` with `bindings`.
async fn store_revision(
    app: &axum::Router,
    connection: &str,
    revision: u64,
    driver: &str,
    public_config: Value,
    bindings: Value,
) -> (StatusCode, Value) {
    let digest = digest_of(app, driver).await;
    send(
        app,
        "POST",
        &format!("/api/v2/admin/connections/{connection}/revisions"),
        Some(json!({
            "revision": revision,
            "provider_digest": digest,
            "public_config": public_config,
            "secret_locators": {},
            "bindings": bindings
        })),
    )
    .await
}

fn refused_at(body: &Value, path: &str, code: &str) -> bool {
    body["error"]["details"]
        .as_array()
        .is_some_and(|d| d.iter().any(|e| e["path"] == path && e["code"] == code))
}

/// A Rust provider's declared dependencies are what a connection over it
/// binds, judged at upload as a workflow package's are: every required alias
/// bound, no other alias, never itself, and a target on the provider the
/// dependency names that declares no dependencies of its own.
#[tokio::test]
async fn a_rust_binding_must_match_the_declared_aliases_and_provider() {
    let (_state, app, _lifecycle) = node().await;
    let (status, body) = store_revision(
        &app,
        "greeter",
        1,
        DRIVER,
        json!({ "greeting": "hi" }),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body) = store_revision(&app, "fetch", 1, FETCHER, json!({}), json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(refused_at(&body, "bindings.store", "REQUIRED"), "{body}");

    let (_, body) = store_revision(
        &app,
        "fetch",
        1,
        FETCHER,
        json!({}),
        json!({ "store": "greeter", "extra": "greeter" }),
    )
    .await;
    assert!(
        refused_at(&body, "bindings.extra", "UNKNOWN_FIELD"),
        "{body}"
    );

    let (_, body) = store_revision(
        &app,
        "fetch",
        1,
        FETCHER,
        json!({}),
        json!({ "store": "fetch" }),
    )
    .await;
    assert!(refused_at(&body, "bindings.store", "INVALID"), "{body}");

    // A connection that is itself a fetcher: the wrong provider, and one
    // that would chain.
    let (status, body) = store_revision(
        &app,
        "fetch-a",
        1,
        FETCHER,
        json!({}),
        json!({ "store": "greeter" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (_, body) = store_revision(
        &app,
        "fetch",
        1,
        FETCHER,
        json!({}),
        json!({ "store": "fetch-a" }),
    )
    .await;
    assert!(refused_at(&body, "bindings.store", "INVALID"), "{body}");

    // A provider that declares no dependencies binds none.
    let (_, body) = store_revision(
        &app,
        "greeter",
        2,
        DRIVER,
        json!({ "greeting": "hi" }),
        json!({ "store": "greeter" }),
    )
    .await;
    assert!(refused_at(&body, "bindings", "INVALID"), "{body}");

    let (status, body) = store_revision(
        &app,
        "fetch",
        1,
        FETCHER,
        json!({}),
        json!({ "store": "greeter" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["connection"]["bindings"]["store"], "greeter", "{body}");
}

/// The binding's target is pinned by the deployment: a member that binds a
/// connection it does not require is refused when the deployment is built.
#[tokio::test]
async fn a_bound_target_must_be_required_by_the_deployment() {
    let (_state, app, _lifecycle) = node().await;
    let (_, greeter) = store_revision(
        &app,
        "greeter",
        1,
        DRIVER,
        json!({ "greeting": "hi" }),
        json!({}),
    )
    .await;
    let (_, fetch) = store_revision(
        &app,
        "fetch",
        1,
        FETCHER,
        json!({}),
        json!({ "store": "greeter" }),
    )
    .await;
    let digest = |b: &Value| {
        b["connection"]["configuration_digest"]
            .as_str()
            .expect("a digest")
            .to_string()
    };
    let (base, members) = deploy::merged(
        &app,
        vec![
            deploy::Upsert {
                kind: "connection",
                id: "greeter".into(),
                digest: digest(&greeter),
                revision: Some(1),
                requires: Vec::new(),
            },
            deploy::Upsert {
                kind: "connection",
                id: "fetch".into(),
                digest: digest(&fetch),
                revision: Some(1),
                requires: Vec::new(),
            },
        ],
    )
    .await;
    let (status, body) = deploy::apply(&app, base, members).await;
    assert!(status.is_client_error(), "{status}: {body}");
    assert!(
        body.to_string().contains("does not require"),
        "the refusal names the missing requirement: {body}"
    );
}

fn configuration_digest(body: &Value) -> String {
    body["connection"]["configuration_digest"]
        .as_str()
        .unwrap_or_else(|| unreachable!("a stored revision: {body}"))
        .to_string()
}

/// Deploy `greeter` (an echo connection at `greeter_rev`), `fetch` bound to
/// it, and a workflow + channel `fetches` that calls `fetch` with
/// `data.name` and `data.op`.
async fn deploy_fetcher(app: &axum::Router, greeter: &Value, fetch: &Value) {
    let workflow = deploy::upload(
        app,
        json!({ "kind": "workflow", "definition": {
            "name": "fetches",
            "tasks": [
                { "id": "call", "function": { "name": "connector_call", "input": {
                    "connection": "fetch",
                    "target_id": FETCHER,
                    "operation": "fetch",
                    "input": { "name": { "var": "data.name" }, "op": { "var": "data.op" } },
                    "output": "data.reply"
                } } }
            ]
        } }),
    )
    .await;
    let channel = deploy::upload(
        app,
        deploy::channel_definition(&common::sync_http_channel("fetches", "fetches")).0,
    )
    .await;
    deploy::deploy(
        app,
        vec![
            deploy::Upsert {
                kind: "connection",
                id: "greeter".into(),
                digest: configuration_digest(greeter),
                revision: greeter["connection"]["revision"].as_u64(),
                requires: Vec::new(),
            },
            deploy::Upsert {
                kind: "connection",
                id: "fetch".into(),
                digest: configuration_digest(fetch),
                revision: Some(1),
                requires: vec![("connection", "greeter".into())],
            },
            deploy::Upsert {
                kind: "workflow",
                id: "fetches".into(),
                digest: workflow,
                revision: None,
                requires: vec![("connection", "fetch".into())],
            },
            deploy::Upsert {
                kind: "channel",
                id: "fetches".into(),
                digest: channel,
                revision: None,
                requires: vec![("workflow", "fetches".into())],
            },
        ],
    )
    .await;
}

/// A Rust provider calls the connection bound to its declared alias, at the
/// revision the deployment pins: a newer stored revision is not reached until
/// a deployment requires it, and then the old one is released.
#[tokio::test]
async fn a_rust_provider_reaches_the_pinned_target_revision() {
    let (_state, app, lifecycle) = node().await;
    let (_, greeter) = store_revision(
        &app,
        "greeter",
        1,
        DRIVER,
        json!({ "greeting": "hello" }),
        json!({}),
    )
    .await;
    let (_, fetch) = store_revision(
        &app,
        "fetch",
        1,
        FETCHER,
        json!({}),
        json!({ "store": "greeter" }),
    )
    .await;
    deploy_fetcher(&app, &greeter, &fetch).await;

    let (status, body) = send(
        &app,
        "POST",
        "/api/v2/data/fetches",
        Some(json!({ "data": { "name": "orion" } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["reply"]["via"]["greeting"], "hello", "{body}");
    assert_eq!(
        body["data"]["reply"]["via"]["echo"]["name"], "orion",
        "{body}"
    );
    assert_eq!(
        body["data"]["reply"]["bound"],
        json!({ "id": "greeter", "revision": 1 })
    );

    // Stored, not deployed: still revision 1.
    let (status, greeter2) = store_revision(
        &app,
        "greeter",
        2,
        DRIVER,
        json!({ "greeting": "hey" }),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{greeter2}");
    let (_, body) = send(
        &app,
        "POST",
        "/api/v2/data/fetches",
        Some(json!({ "data": { "name": "orion" } })),
    )
    .await;
    assert_eq!(body["data"]["reply"]["via"]["greeting"], "hello", "{body}");

    // Deployed: the binding follows the requirement to revision 2.
    deploy::deploy(
        &app,
        vec![deploy::Upsert {
            kind: "connection",
            id: "greeter".into(),
            digest: configuration_digest(&greeter2),
            revision: Some(2),
            requires: Vec::new(),
        }],
    )
    .await;
    let (_, body) = send(
        &app,
        "POST",
        "/api/v2/data/fetches",
        Some(json!({ "data": { "name": "orion" } })),
    )
    .await;
    assert_eq!(body["data"]["reply"]["via"]["greeting"], "hey", "{body}");
    assert_eq!(body["data"]["reply"]["bound"]["revision"], 2, "{body}");
    assert!(
        lifecycle
            .disposed
            .lock()
            .expect("lock")
            .contains(&("greeter".to_string(), 1)),
        "the released revision is disposed"
    );
}

/// The dependency grants `echo` alone: any other operation on the bound
/// connection is refused before it is dispatched.
#[tokio::test]
async fn an_ungranted_dependency_operation_is_forbidden() {
    let (_state, app, _lifecycle) = node().await;
    let (_, greeter) = store_revision(
        &app,
        "greeter",
        1,
        DRIVER,
        json!({ "greeting": "hello" }),
        json!({}),
    )
    .await;
    let (_, fetch) = store_revision(
        &app,
        "fetch",
        1,
        FETCHER,
        json!({}),
        json!({ "store": "greeter" }),
    )
    .await;
    deploy_fetcher(&app, &greeter, &fetch).await;

    let (status, body) = send(
        &app,
        "POST",
        "/api/v2/data/fetches",
        Some(json!({ "data": { "name": "orion", "op": "fail" } })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["error"]["connector"]["provider_code"], "CONNECTOR_OPERATION_FORBIDDEN",
        "{body}"
    );
}

/// The probe of a connection over a Rust provider binds its dependencies, so
/// a health check that calls one reaches the bound connection.
#[tokio::test]
async fn the_probe_reaches_a_rust_providers_bound_connection() {
    let (_state, app, _lifecycle) = node().await;
    let (_, greeter) = store_revision(
        &app,
        "greeter",
        1,
        DRIVER,
        json!({ "greeting": "hello" }),
        json!({}),
    )
    .await;
    let (_, fetch) = store_revision(
        &app,
        "fetch",
        1,
        FETCHER,
        json!({}),
        json!({ "store": "greeter" }),
    )
    .await;
    deploy_fetcher(&app, &greeter, &fetch).await;

    let (status, body) = send(
        &app,
        "POST",
        "/api/v2/admin/connections/fetch/revisions/1/probes",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["healthy"], true, "{body}");
}

/// `/runtime-capabilities` lists what a provider depends on, so a deployer
/// knows which aliases a connection over it binds.
#[tokio::test]
async fn runtime_capabilities_list_a_providers_dependencies() {
    let (_state, app, _lifecycle) = node().await;
    let (_, caps) = send(&app, "GET", "/api/v2/admin/runtime-capabilities", None).await;
    let backends = caps["provider_backends"].as_array().expect("backends");
    let fetcher = backends
        .iter()
        .find(|b| b["driver_id"] == FETCHER)
        .expect("listed");
    assert_eq!(
        fetcher["dependencies"]["store"],
        json!({ "provider": DRIVER, "operations": ["echo"], "optional": false }),
        "{fetcher}"
    );
    let echo = backends
        .iter()
        .find(|b| b["driver_id"] == DRIVER)
        .expect("listed");
    assert!(echo.get("dependencies").is_none(), "{echo}");
}
