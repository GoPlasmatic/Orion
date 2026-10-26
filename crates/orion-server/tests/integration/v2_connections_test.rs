//! Connections over HTTP: a revision is judged as the node will load it
//! before it is stored, compile makes the same judgement, a probe connects
//! for real, and `connector_call` holds a package provider's input to the
//! schema the package declares.

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use orion::extensions::context::AttemptContext;
use orion::extensions::provider_backend::ProviderBackend;
use orion::extensions::{ConnectorError, ConnectorOutput, ImplementationKind};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common::{body_json, json_request, test_app, test_state_with_config};

const CONNECTIONS: &str = "/api/v2/admin/connections";
const PACKAGES: &str = "/api/v2/admin/connector-packages";

async fn send(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(json_request(method, uri, body))
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

fn builtin(driver: &str) -> String {
    orion::extensions::foundation::builtin_provider_digest(driver)
        .expect("a built-in driver")
        .into_string()
}

async fn create(
    app: &Router,
    id: &str,
    provider: &str,
    public_config: Value,
    secret_locators: Value,
) -> (StatusCode, Value) {
    send(
        app,
        "POST",
        CONNECTIONS,
        Some(json!({
            "connection_id": id,
            "revision": 1,
            "provider_digest": provider,
            "public_config": public_config,
            "secret_locators": secret_locators,
        })),
    )
    .await
}

/// `(path, code)` of every problem a refusal names.
fn problems(body: &Value) -> Vec<(String, String)> {
    body["error"]["details"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| {
            (
                d["path"].as_str().unwrap_or_default().to_string(),
                d["code"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// A configuration over a built-in driver that the node would not load is
/// refused at create, each problem at its field; one whose references stand
/// where a boolean goes is typed as it will be once they resolve.
#[tokio::test]
async fn a_built_in_connection_that_would_not_load_is_refused_by_field() {
    let app = test_app().await;
    let http = builtin(orion::extensions::foundation::DRIVER_HTTP);
    let db = builtin(orion::extensions::foundation::DRIVER_POSTGRESQL);

    for (config, locators, path, code) in [
        (
            json!({"url": "ftp://files.example.com"}),
            json!({}),
            "public_config.url",
            "INVALID",
        ),
        (
            json!({"url": "https://api.example.com", "retry": {"max_retries": 99}}),
            json!({}),
            "public_config.retry.max_retries",
            "INVALID",
        ),
        (
            json!({"url": "https://api.example.com", "max_response_size": "large"}),
            json!({}),
            "public_config.max_response_size",
            "TYPE_MISMATCH",
        ),
    ] {
        let (status, body) = create(&app, "api", &http, config.clone(), locators).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{config}: {body}");
        assert_eq!(body["error"]["code"], "VALIDATION_ERROR", "{body}");
        assert!(
            problems(&body).contains(&(path.to_string(), code.to_string())),
            "{config} is refused at {path} ({code}): {body}"
        );
    }

    let (status, body) = create(&app, "orders", &db, json!({}), json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        problems(&body).contains(&(
            "public_config.connection_string".to_string(),
            "REQUIRED".to_string()
        )),
        "{body}"
    );

    // The connection string is supplied by a locator, so the refusal is
    // named there.
    let (status, body) = create(
        &app,
        "orders",
        &db,
        json!({}),
        json!({"connection_string": "env://ORION_SECRET_PROBE_DSN"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a reference defers to load: {body}"
    );

    let (status, body) = create(
        &app,
        "api",
        &http,
        json!({"url": "https://api.example.com"}),
        json!({"allow_private_urls": "env://ORION_SECRET_PRIVATE_OK"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a reference where a boolean goes is typed as one: {body}"
    );
}

/// `compile` refuses the connector the API would refuse, for the same reason.
#[test]
fn compile_refuses_what_the_api_refuses() {
    let err = orion::definitions::deployment::connection(
        &json!({
            "name": "api",
            "connector_type": "http",
            "config": {"url": "https://api.example.com", "retry": {"max_retries": 99}}
        }),
        "connectors/api.json",
    )
    .expect_err("refused");
    assert!(err.contains("would not load"), "{err}");
    assert!(err.contains("`config.retry.max_retries`"), "{err}");

    orion::definitions::deployment::connection(
        &json!({
            "name": "api",
            "connector_type": "http",
            "config": {"url": "https://api.example.com", "retry": {"max_retries": 3}}
        }),
        "connectors/api.json",
    )
    .expect("a loadable connector compiles");
}

/// The fixture's helper package, admitted on upload.
async fn helper_package(app: &Router) -> (String, Value) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/v2/package-identity/bundle.json");
    let bundle: Value = serde_json::from_str(&std::fs::read_to_string(dir).unwrap()).unwrap();
    let pkg = &bundle["providers"][1];
    let mut files = pkg["payloads"].as_object().unwrap().clone();
    let inventory =
        orion::extensions::package_identity::canonical_bytes(&pkg["inventory"]).unwrap();
    files.insert(
        "inventory.json".to_string(),
        Value::String(String::from_utf8(inventory).unwrap()),
    );
    let (status, uploaded) = send(
        app,
        "POST",
        PACKAGES,
        Some(json!({"files": files, "admit": true})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{uploaded}");
    (
        uploaded["digest"].as_str().unwrap().to_string(),
        uploaded["package"].clone(),
    )
}

/// A connection over an uploaded package must satisfy the package's declared
/// `configuration_schema` (`base_url`: a required, non-empty string).
#[tokio::test]
async fn a_package_connection_must_satisfy_its_configuration_schema() {
    let app = test_app().await;
    let (digest, _) = helper_package(&app).await;

    for (config, path, code) in [
        (json!({}), "public_config.base_url", "REQUIRED"),
        (
            json!({"base_url": 5}),
            "public_config.base_url",
            "TYPE_MISMATCH",
        ),
        (
            json!({"base_url": "https://crm.example.com", "region": "eu"}),
            "public_config.region",
            "INVALID",
        ),
    ] {
        let (status, body) = create(&app, "crm", &digest, config.clone(), json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{config}: {body}");
        assert_eq!(
            problems(&body),
            vec![(path.to_string(), code.to_string())],
            "{config}: {body}"
        );
    }
    let (status, body) = create(
        &app,
        "crm",
        &digest,
        json!({"base_url": "https://crm.example.com"}),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

async fn probe(app: &Router, id: &str) -> (StatusCode, Value) {
    send(app, "POST", &format!("{CONNECTIONS}/{id}/probe"), None).await
}

/// A built-in-driver connection's probe connects: `SELECT 1` through the
/// node's pool, a read of the memory cache, a TCP connect to an HTTP
/// connection's host — refusing a private address unless the connection
/// allows one — and it never answers a secret.
#[tokio::test]
async fn a_built_in_connection_probe_connects() {
    use orion::extensions::foundation::{
        DRIVER_HTTP, DRIVER_MEMORY_CACHE, DRIVER_POSTGRESQL, DRIVER_SQLITE,
    };
    let app = test_app().await;

    let (status, body) = create(
        &app,
        "numbers",
        &builtin(DRIVER_SQLITE),
        json!({"connection_string": "sqlite::memory:"}),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = probe(&app, "numbers").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["healthy"], true, "{body}");
    assert_eq!(body["driver_id"], DRIVER_SQLITE, "{body}");
    assert_eq!(body["details"]["probe"], "SELECT 1", "{body}");

    // A database file that cannot be opened.
    let (status, body) = create(
        &app,
        "missing",
        &builtin(DRIVER_SQLITE),
        json!({"connection_string": "sqlite:///nonexistent-orion-dir/probe.db"}),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = probe(&app, "missing").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["healthy"], false, "{body}");
    assert_eq!(body["details"]["probe"], "SELECT 1", "{body}");

    let (status, body) = create(
        &app,
        "scratch",
        &builtin(DRIVER_MEMORY_CACHE),
        json!({}),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = probe(&app, "scratch").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["details"]["probe"], "cache read of a probe key",
        "{body}"
    );

    // HTTP: a TCP connect to the host, which is private here.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let url = format!("http://127.0.0.1:{port}/v1");
    let (status, body) = create(
        &app,
        "peer",
        &builtin(DRIVER_HTTP),
        json!({"url": url, "allow_private_urls": true}),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = probe(&app, "peer").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["details"]["probe"], "TCP connect to the URL's host",
        "{body}"
    );

    let (status, body) = create(
        &app,
        "peer-strict",
        &builtin(DRIVER_HTTP),
        json!({"url": url}),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = probe(&app, "peer-strict").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(
        body["details"]["error"]
            .as_str()
            .is_some_and(|e| e.contains("private")),
        "{body}"
    );
    drop(listener);

    // A secret that does not resolve is the answer, named by stage.
    let (status, body) = create(
        &app,
        "orders",
        &builtin(DRIVER_POSTGRESQL),
        json!({}),
        json!({"connection_string": "env://ORION_SECRET_PROBE_UNSET_DSN"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = probe(&app, "orders").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(
        body["details"]["error"]
            .as_str()
            .is_some_and(|e| e.starts_with("secret_resolution")),
        "{body}"
    );

    let (status, body) = send(
        &app,
        "GET",
        "/api/v2/admin/audit-logs?action=v2_connection_probe",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// A provider that answers what it was sent, so a test can tell whether a
/// call reached it.
struct EchoProvider {
    digest: String,
}

#[async_trait::async_trait]
impl ProviderBackend for EchoProvider {
    fn kind(&self) -> ImplementationKind {
        ImplementationKind::Wasm
    }

    fn driver_id(&self) -> &str {
        &self.digest
    }

    fn validate_config(&self, _config: &Value) -> Result<(), ConnectorError> {
        Ok(())
    }

    async fn invoke(
        &self,
        _ctx: &AttemptContext,
        _operation: &str,
        input: Value,
    ) -> Result<ConnectorOutput, ConnectorError> {
        Ok(ConnectorOutput::new(json!({
            "contact_id": input["contact_id"],
            "display_name": "Ada",
        })))
    }
}

/// `connector_call` checks its input against the operation's declared
/// `input_schema` before the provider is called, and refuses with
/// `CONNECTOR_INVALID_INPUT` naming the field.
#[tokio::test]
async fn connector_call_input_must_satisfy_the_declared_input_schema() {
    let state = test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());
    let (digest, package) = helper_package(&app).await;
    state
        .composition
        .providers()
        .admit(Arc::new(EchoProvider {
            digest: digest.clone(),
        }))
        .expect("the package's backend is admitted");

    let (status, body) = create(
        &app,
        "crm",
        &digest,
        json!({"base_url": "https://crm.example.com"}),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let connection = body["connection"]["configuration_digest"]
        .as_str()
        .unwrap()
        .to_string();

    let workflow = crate::common::deploy::upload(
        &app,
        json!({"kind": "workflow", "definition": {
            "name": "Lookup",
            "tasks": [{"id": "get", "function": {"name": "connector_call", "input": {
                "connection": "crm",
                "kind": "wasm",
                "target_id": digest,
                "operation": "contacts.get",
                "input": {"contact_id": {"var": "data.id"}},
                "output": "data.contact"
            }}}]
        }}),
    )
    .await;
    let channel = crate::common::deploy::upload(
        &app,
        json!({"kind": "channel", "definition": {
            "name": "lookup",
            "protocol": "rest",
            "methods": ["POST"],
            "route_pattern": "/lookup",
        }}),
    )
    .await;
    let (status, applied) = send(
        &app,
        "POST",
        "/api/v2/admin/deployments",
        Some(json!({
            "idempotency_key": "lookup-1",
            "base_revision": 0,
            "manifest": {
                "schema_version": 2,
                "providers": [{
                    "publisher": package["publisher"],
                    "provider_id": package["provider_id"],
                    "version": package["version"],
                    "digest": digest,
                }],
                "members": [
                    {"entity": {"kind": "channel", "id": "lookup", "revision": 1},
                     "definition_digest": channel,
                     "requires": [{"kind": "workflow", "id": "lookup-wf", "revision": 1}]},
                    {"entity": {"kind": "workflow", "id": "lookup-wf", "revision": 1},
                     "definition_digest": workflow,
                     "requires": [{"kind": "connection", "id": "crm", "revision": 1}]},
                    {"entity": {"kind": "connection", "id": "crm", "revision": 1},
                     "definition_digest": connection},
                ]
            },
            "reason": "deploy lookup",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{applied}");
    assert_eq!(applied["evaluation"]["converged"], true, "{applied}");

    let (status, body) = send(
        &app,
        "POST",
        "/api/v2/data/lookup",
        Some(json!({"data": {"id": "c-1"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["contact"]["display_name"], "Ada", "{body}");

    // `contact_id` must be a non-empty string: a number never reaches the
    // provider.
    let (status, body) = send(
        &app,
        "POST",
        "/api/v2/data/lookup",
        Some(json!({"data": {"id": 7}})),
    )
    .await;
    assert_ne!(status, StatusCode::OK, "{body}");
    let text = body.to_string();
    assert!(text.contains("CONNECTOR_INVALID_INPUT"), "{body}");
    assert!(text.contains("input.contact_id"), "{body}");
}
