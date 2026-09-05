//! The plugin entity through the admin API, and a plugin function serving a
//! request through the data plane.
//!
//! Uses the fixture component in `tests/fixtures/plugins/` — eleven functions
//! under the plugin id `test.fixture`, one of which (`wrap`) returns its
//! input inside an object, which is what the data-plane round trip asserts.

use axum::http::StatusCode;
use base64::Engine as _;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common::{
    body_json, create_and_activate_channel_with_config, json_request, test_state_with_config,
};
use orion::config::AppConfig;
use orion::server::state::AppState;

const COMPONENT: &[u8] = include_bytes!("../fixtures/plugins/fixture.wasm");
// The upload manifest: the functions that answer a probe. `fixture.toml`
// also declares the three that misbehave on purpose, which the self-test
// every upload runs would — correctly — refuse.
const MANIFEST: &str = include_str!("../fixtures/plugins/fixture-upload.toml");

fn config(enabled: bool) -> AppConfig {
    let mut config = AppConfig::default();
    config.trace_storage.mode = orion::config::TraceStorageMode::Sync;
    config.plugins.enabled = enabled;
    config.plugins.max_timeout_ms = 2_000;
    config
}

async fn app(enabled: bool) -> (AppState, axum::Router) {
    let state = test_state_with_config(config(enabled)).await;
    let router = orion::server::build_router(state.clone());
    (state, router)
}

fn component_b64() -> String {
    base64::engine::general_purpose::STANDARD.encode(COMPONENT)
}

fn upload() -> Value {
    json!({"manifest": MANIFEST, "component": component_b64(), "tags": ["fixture"]})
}

async fn post(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(json_request("POST", path, Some(body)))
        .await
        .expect("request");
    let status = resp.status();
    (status, body_json(resp).await)
}

async fn patch(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(json_request("PATCH", path, Some(body)))
        .await
        .expect("request");
    let status = resp.status();
    (status, body_json(resp).await)
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(json_request("GET", path, None))
        .await
        .expect("request");
    let status = resp.status();
    (status, body_json(resp).await)
}

async fn create_fixture(app: &axum::Router) -> Value {
    let (status, body) = post(app, "/api/v1/admin/plugins", upload()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["data"].clone()
}

async fn set_status(app: &axum::Router, id: &str, status: &str) -> (StatusCode, Value) {
    patch(
        app,
        &format!("/api/v1/admin/plugins/{id}/status"),
        json!({"status": status}),
    )
    .await
}

/// A workflow calling the fixture's `wrap` on the request's `msg`.
fn wrap_workflow(name: &str) -> Value {
    json!({
        "name": name,
        "condition": true,
        "tasks": [
            {"id": "parse", "name": "parse", "function": {"name": "parse_json",
                "input": {"source": "payload", "target": "input"}}},
            {"id": "wrap", "name": "wrap", "function": {"name": "test.fixture.wrap",
                "input": {"message": {"var": "data.input.msg"}, "output": "data.result"}}}
        ]
    })
}

/// The catalogue entry for a function, if served.
async fn catalogued(app: &axum::Router, function: &str) -> Option<Value> {
    let (_, body) = get(app, "/api/v1/admin/functions").await;
    body["data"]
        .as_array()
        .expect("array")
        .iter()
        .find(|e| e["name"] == function)
        .cloned()
}

#[tokio::test]
async fn an_upload_is_validated_compiled_and_stored_as_a_draft() {
    let (_state, app) = app(true).await;
    let plugin = create_fixture(&app).await;
    assert_eq!(plugin["plugin_id"], "test.fixture");
    assert_eq!(plugin["version"], 1);
    assert_eq!(plugin["status"], "draft");
    assert_eq!(plugin["abi"], "orion:plugin@1.0.0");
    assert_eq!(plugin["plugin_version"], "0.0.0");
    assert_eq!(plugin["functions"].as_array().map(Vec::len), Some(8));
    assert!(
        plugin["digest"]
            .as_str()
            .is_some_and(|d| d.starts_with("sha256:") && d.len() == 71),
        "{plugin}"
    );
    assert!(
        plugin["content_hash"]
            .as_str()
            .is_some_and(|h| h.starts_with("sha256:"))
    );
    assert_eq!(plugin["tags"], json!(["fixture"]));
    assert!(
        plugin.get("health").is_none(),
        "the list/create shape carries no health"
    );

    // The single read says what this node thinks of the version.
    let (status, body) = get(&app, "/api/v1/admin/plugins/test.fixture").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["health"]["state"], "inactive", "{body}");

    // A draft adds nothing to the vocabulary yet.
    assert!(catalogued(&app, "test.fixture.identity").await.is_none());

    // The id is the manifest's, and one plugin has one id.
    let (status, body) = post(&app, "/api/v1/admin/plugins", upload()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

#[tokio::test]
async fn bad_uploads_are_refused_with_a_path() {
    let (_state, app) = app(true).await;

    let (status, body) = post(
        &app,
        "/api/v1/admin/plugins",
        json!({"manifest": MANIFEST.replace("orion:plugin@1.0.0", "orion:plugin@9.0.0"), "component": component_b64()}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["error"]["details"][0]["path"], "manifest.abi",
        "{body}"
    );

    let (status, body) = post(
        &app,
        "/api/v1/admin/plugins",
        json!({"manifest": MANIFEST, "component": "not base64!"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["details"][0]["path"], "component", "{body}");

    let (status, body) = post(
        &app,
        "/api/v1/admin/plugins",
        json!({"manifest": MANIFEST, "component": base64::engine::general_purpose::STANDARD.encode(b"not a component")}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["details"][0]["path"], "component", "{body}");
    assert!(
        body["error"]["details"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("compile")),
        "{body}"
    );

    let (status, body) = post(
        &app,
        "/api/v1/admin/plugins",
        json!({"plugin_id": "someone.else", "manifest": MANIFEST, "component": component_b64()}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["details"][0]["path"], "plugin_id", "{body}");

    let (status, body) = post(
        &app,
        "/api/v1/admin/plugins",
        json!({"manifest": MANIFEST, "digest": "sha256:0000000000000000000000000000000000000000000000000000000000000000"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["details"][0]["path"], "digest", "{body}");
}

#[tokio::test]
async fn activation_publishes_the_functions_and_a_workflow_serves_through_one() {
    let (state, app) = app(true).await;
    let plugin = create_fixture(&app).await;
    let digest = plugin["digest"].as_str().expect("digest").to_string();

    let (status, body) = set_status(&app, "test.fixture", "active").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["status"], "active");

    // The generation now carries the plugin: catalogue, health, registry.
    let entry = catalogued(&app, "test.fixture.identity")
        .await
        .expect("an active plugin's functions are catalogued");
    assert_eq!(entry["source"], "plugin");
    assert_eq!(entry["plugin"]["id"], "test.fixture");
    assert_eq!(entry["plugin"]["digest"], digest);
    assert_eq!(entry["retry_safety"]["kind"], "pure");
    let (_, body) = get(&app, "/api/v1/admin/plugins/test.fixture").await;
    assert_eq!(body["data"]["health"]["state"], "loaded", "{body}");
    assert!(body["data"]["health"]["compile_ms"].is_number());
    let generation = state.runtime.load();
    assert!(generation.functions.contains("test.fixture.wrap"));
    assert_eq!(generation.plugins.plugins.len(), 1);
    assert!(generation.plugins.issues.is_empty());
    let (_, health) = get(&app, "/health").await;
    assert_eq!(health["components"]["plugins"], "ok", "{health}");

    // A workflow may name the function now, and a channel serves it.
    let (channel, workflow_id) = create_and_activate_channel_with_config(
        &app,
        "plugin-wrap",
        wrap_workflow("Wrap via plugin"),
        json!({}),
    )
    .await;
    let (status, body) = post(
        &app,
        &format!("/api/v1/data/{channel}"),
        json!({"data": {"msg": "hi"}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["result"]["wrapped"]["message"], "hi", "{body}");
    assert_eq!(body["data"]["result"]["len"], 16, "{body}");

    // Dependants are reported, and they gate archive and delete.
    let (_, deps) = get(&app, "/api/v1/admin/plugins/test.fixture/dependencies").await;
    assert_eq!(deps["data"]["workflows"], json!([workflow_id]), "{deps}");
    let (status, body) = set_status(&app, "test.fixture", "archived").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains(&workflow_id))
    );
    let resp = app
        .clone()
        .oneshot(json_request(
            "DELETE",
            "/api/v1/admin/plugins/test.fixture",
            None,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // Archive the workflow, then the plugin; its functions leave the vocabulary.
    let (status, body) = patch(
        &app,
        &format!("/api/v1/admin/workflows/{workflow_id}/status"),
        json!({"status": "archived"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = set_status(&app, "test.fixture", "archived").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(catalogued(&app, "test.fixture.identity").await.is_none());
    assert!(!state.runtime.load().functions.contains("test.fixture.wrap"));

    // The workflow cannot come back while its function is gone.
    let (status, body) = post(
        &app,
        &format!("/api/v1/admin/workflows/{workflow_id}/versions"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = patch(
        &app,
        &format!("/api/v1/admin/workflows/{workflow_id}/status"),
        json!({"status": "active"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("test.fixture.wrap") && m.contains("not available")),
        "{body}"
    );

    // And now the plugin can go, artifact and all.
    let resp = app
        .clone()
        .oneshot(json_request(
            "DELETE",
            "/api/v1/admin/plugins/test.fixture",
            None,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(
        !state
            .repos
            .plugins
            .artifact_exists(&digest)
            .await
            .expect("query")
    );
}

/// A workflow calling the fixture's `upper` with an expression in its
/// template field.
fn upper_workflow(name: &str, text: Value) -> Value {
    json!({
        "name": name,
        "condition": true,
        "tasks": [
            {"id": "parse", "name": "parse", "function": {"name": "parse_json",
                "input": {"source": "payload", "target": "input"}}},
            {"id": "up", "name": "up", "function": {"name": "test.fixture.upper",
                "input": {"text": text, "output": "data.up"}}}
        ]
    })
}

/// The manifest's `template_at` field end to end: catalogued as the
/// registry spelling, evaluated by the engine on the data plane, refused a
/// secret at create time, and — the schema gate — a new version whose table
/// an active dependant no longer satisfies cannot activate under it.
#[tokio::test]
async fn a_template_field_serves_refuses_secrets_and_gates_a_schema_change() {
    let (_state, app) = app(true).await;
    create_fixture(&app).await;
    let (status, body) = set_status(&app, "test.fixture", "active").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let entry = catalogued(&app, "test.fixture.upper")
        .await
        .expect("served");
    let text = entry["input_fields"]
        .as_array()
        .expect("fields")
        .iter()
        .find(|f| f["name"] == "text")
        .expect("text field")
        .clone();
    assert_eq!(text["template_at"], json!([""]), "{entry}");
    assert_eq!(text["resolvable"], json!(false));

    let (channel, workflow_id) = create_and_activate_channel_with_config(
        &app,
        "plugin-upper",
        upper_workflow(
            "Upper via plugin",
            json!({"cat": ["hello ", {"var": "data.input.who"}]}),
        ),
        json!({}),
    )
    .await;
    let (status, body) = post(
        &app,
        &format!("/api/v1/data/{channel}"),
        json!({"data": {"who": "world"}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["up"]["TEXT"], "HELLO WORLD", "{body}");

    // A secret node in the template field is refused at create time: the
    // engine would evaluate it, and a plugin never sees key material.
    let (status, body) = post(
        &app,
        "/api/v1/admin/workflows",
        upper_workflow("Upper with a secret", json!({"secret": "api_key"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.to_string().contains("never sees key material"),
        "{body}"
    );

    // A new version renames the field. Activating it would quarantine the
    // active workflow, so the gate refuses with the workflow and the
    // mismatch named — and `?dry_run=true` reports the same finding.
    let (status, body) = post(
        &app,
        "/api/v1/admin/plugins/test.fixture/versions",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let renamed = MANIFEST.replace("name = \"text\"", "name = \"payload\"");
    assert_ne!(renamed, MANIFEST);
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/plugins/test.fixture",
            Some(json!({"manifest": renamed})),
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK, "{}", body_json(resp).await);
    let (status, body) = patch(
        &app,
        "/api/v1/admin/plugins/test.fixture/status?dry_run=true",
        json!({"status": "active"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["valid"], false, "{body}");
    assert!(body.to_string().contains(&workflow_id), "{body}");
    let (status, body) = set_status(&app, "test.fixture", "active").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let message = body["error"]["message"].as_str().expect("message");
    assert!(message.contains(&workflow_id), "{message}");
    assert!(
        message.contains("'text'") && message.contains("UNKNOWN_FIELD"),
        "{message}"
    );
    assert!(
        message.contains("'payload'") && message.contains("REQUIRED"),
        "{message}"
    );

    // Version 1 kept serving throughout.
    let (status, body) = post(
        &app,
        &format!("/api/v1/data/{channel}"),
        json!({"data": {"who": "still"}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["up"]["TEXT"], "HELLO STILL", "{body}");

    // Once the dependant is gone, the new schema activates.
    let (status, body) = patch(
        &app,
        &format!("/api/v1/admin/workflows/{workflow_id}/status"),
        json!({"status": "archived"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = set_status(&app, "test.fixture", "active").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["version"], 2);
    let entry = catalogued(&app, "test.fixture.upper")
        .await
        .expect("served");
    assert!(
        entry["input_fields"]
            .as_array()
            .expect("fields")
            .iter()
            .any(|f| f["name"] == "payload"),
        "{entry}"
    );
}

#[tokio::test]
async fn a_new_version_supersedes_the_active_one() {
    let (_state, app) = app(true).await;
    create_fixture(&app).await;
    let (status, _) = set_status(&app, "test.fixture", "active").await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = post(
        &app,
        "/api/v1/admin/plugins/test.fixture/versions",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["data"]["version"], 2);
    assert_eq!(body["data"]["status"], "draft");

    // The draft can be edited — tags here, the component stays.
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/plugins/test.fixture",
            Some(json!({"tags": ["v2"]})),
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["tags"], json!(["v2"]), "{body}");

    let (status, body) = set_status(&app, "test.fixture", "active").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["version"], 2);

    let (_, versions) = get(&app, "/api/v1/admin/plugins/test.fixture/versions").await;
    let by_version: Vec<(i64, String)> = versions["data"]
        .as_array()
        .expect("array")
        .iter()
        .map(|v| {
            (
                v["version"].as_i64().unwrap(),
                v["status"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        by_version,
        vec![(2, "active".to_string()), (1, "archived".to_string())],
        "{versions}"
    );
    let entry = catalogued(&app, "test.fixture.identity")
        .await
        .expect("served");
    assert_eq!(entry["plugin"]["version"], 2);
}

#[tokio::test]
async fn export_and_import_round_trip_with_and_without_artifacts() {
    let (_state, app) = app(true).await;
    create_fixture(&app).await;
    let (status, _) = set_status(&app, "test.fixture", "active").await;
    assert_eq!(status, StatusCode::OK);

    // Inlined: the artifact travels with the item.
    let (status, exported) = get(&app, "/api/v1/admin/plugins/export?include_artifacts=true").await;
    assert_eq!(status, StatusCode::OK);
    let items = exported["data"].as_array().expect("array").clone();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["component"], component_b64());
    assert!(items[0].get("manifest").is_some());

    // Re-importing the same content over the active version is a no-op.
    let (status, body) = post(
        &app,
        "/api/v1/admin/plugins/import?on_conflict=new_version",
        Value::Array(items.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["unchanged"], 1, "{body}");
    assert_eq!(body["data"]["results"][0]["action"], "unchanged", "{body}");

    // Without artifacts the item names the digest; a target holding the
    // bytes accepts it — here as a second plugin sharing the component.
    let (_, exported) = get(&app, "/api/v1/admin/plugins/export").await;
    let mut item = exported["data"][0].clone();
    assert!(item.get("component").is_none());
    let mut manifest = item["manifest"].clone();
    manifest["name"] = json!("test.other");
    for f in manifest["functions"].as_array_mut().expect("functions") {
        let name = f["name"]
            .as_str()
            .expect("name")
            .replace("test.fixture.", "test.other.");
        f["name"] = json!(name);
    }
    item["manifest"] = manifest;
    item["plugin_id"] = json!("test.other");
    let (status, body) = post(&app, "/api/v1/admin/plugins/import", json!([item])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["imported"], 1, "{body}");
    let (status, body) = get(&app, "/api/v1/admin/plugins/test.other").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["digest"], items[0]["digest"]);
    let (status, body) = set_status(&app, "test.other", "active").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(catalogued(&app, "test.other.wrap").await.is_some());
}

#[tokio::test]
async fn dry_run_reports_every_gate_without_writing() {
    let (_state, app) = app(true).await;
    create_fixture(&app).await;
    let (status, body) = patch(
        &app,
        "/api/v1/admin/plugins/test.fixture/status?dry_run=true",
        json!({"status": "active"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["valid"], true, "{body}");
    let (_, body) = get(&app, "/api/v1/admin/plugins/test.fixture").await;
    assert_eq!(body["data"]["status"], "draft", "a dry run writes nothing");

    let (status, _) = set_status(&app, "test.fixture", "active").await;
    assert_eq!(status, StatusCode::OK);
    let (_channel, workflow_id) = create_and_activate_channel_with_config(
        &app,
        "plugin-dry",
        wrap_workflow("Wrap dry"),
        json!({}),
    )
    .await;
    let (status, body) = patch(
        &app,
        "/api/v1/admin/plugins/test.fixture/status?dry_run=true",
        json!({"status": "archived"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["valid"], false, "{body}");
    assert!(
        body["data"]["errors"].to_string().contains(&workflow_id),
        "{body}"
    );
}

#[tokio::test]
async fn a_node_with_plugins_disabled_refuses_uploads_and_reports_it() {
    let (state, app) = app(false).await;
    let (status, body) = post(&app, "/api/v1/admin/plugins", upload()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("plugins.enabled")),
        "{body}"
    );
    let (_, health) = get(&app, "/health").await;
    assert_eq!(health["components"]["plugins"], "disabled", "{health}");

    // An active row reaching this node — through the database, as a cluster
    // peer's activation would — is a load issue, not an abort.
    let draft = orion::storage::repositories::plugins::PluginDraft {
        plugin_id: "test.fixture".to_string(),
        manifest_json: serde_json::to_string(
            &serde_json::to_value(orion::plugin::Manifest::parse(MANIFEST).expect("manifest"))
                .expect("json"),
        )
        .expect("json"),
        digest: orion::plugin::WasmRuntime::digest(COMPONENT),
        tags_json: "[]".to_string(),
        signature: None,
    };
    state
        .repos
        .plugins
        .create(&draft, Some(COMPONENT))
        .await
        .expect("row");
    state
        .repos
        .plugins
        .activate("test.fixture")
        .await
        .expect("activate");
    orion::runtime::reload_engine(&state).await.expect("reload");
    let (_, health) = get(&app, "/health").await;
    assert_eq!(health["components"]["plugins"], "degraded", "{health}");
    let generation = state.runtime.load();
    assert_eq!(generation.plugins.issues.len(), 1);
    assert_eq!(generation.plugins.issues[0].stage, "disabled");
    assert!(!generation.functions.contains("test.fixture.wrap"));
    let (_, body) = get(&app, "/api/v1/admin/plugins/test.fixture").await;
    assert_eq!(body["data"]["health"]["state"], "disabled", "{body}");
}

#[tokio::test]
async fn the_plugin_routes_take_a_component_sized_body() {
    // The admin plane's default body limit is 1 MiB; a component may be 16.
    // A 1.5 MiB body must reach the handler and be refused as a component,
    // not by the limit.
    let (_state, app) = app(true).await;
    let big = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 1_500_000]);
    let (status, body) = post(
        &app,
        "/api/v1/admin/plugins",
        json!({"manifest": MANIFEST, "component": big}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["details"][0]["path"], "component", "{body}");
}

#[tokio::test]
async fn validate_answers_without_storing() {
    let (_state, app) = app(true).await;
    let (status, body) = post(&app, "/api/v1/admin/plugins/validate", upload()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["valid"], true, "{body}");
    let (status, _) = get(&app, "/api/v1/admin/plugins/test.fixture").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = post(
        &app,
        "/api/v1/admin/plugins/validate",
        json!({"manifest": MANIFEST.replace("[[functions]]\nname = \"test.fixture.upper\"", "[[functions]]\nname = \"parse\""), "component": component_b64()}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["valid"], false, "{body}");
}

/// An app whose node names trust keys: an upload must carry a signature over
/// the digest by one of them.
async fn trusting_app(keys: Vec<String>) -> (AppState, axum::Router) {
    let mut config = config(true);
    config.plugins.trust.public_keys = keys;
    let state = test_state_with_config(config).await;
    let router = orion::server::build_router(state.clone());
    (state, router)
}

/// `[plugins.trust]` end to end: refused without a signature, refused with
/// one by a key the node does not trust, accepted with a good one and echoed
/// back; and the node that *loads* the version checks again with its own
/// keys, so a row that verified where it was uploaded is still refused on a
/// node whose policy differs.
#[tokio::test]
async fn a_trusting_node_requires_a_signature_over_the_digest_at_upload_and_at_load() {
    use orion::plugin::trust::SigningKey;
    let key = SigningKey::generate();
    let stranger = SigningKey::generate();
    let (state, app) = trusting_app(vec![key.public_key_base64()]).await;
    let digest = orion::plugin::WasmRuntime::digest(COMPONENT);

    let (status, body) = post(&app, "/api/v1/admin/plugins", upload()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["details"][0]["path"], "signature", "{body}");
    assert_eq!(body["error"]["details"][0]["code"], "REQUIRED", "{body}");

    let mut signed_by_stranger = upload();
    signed_by_stranger["signature"] = json!(stranger.sign(&digest));
    let (status, body) = post(&app, "/api/v1/admin/plugins", signed_by_stranger).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["details"][0]["code"], "INVALID", "{body}");
    assert!(
        body["error"]["details"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("does not verify")),
        "{body}"
    );
    // The same refusal from the pre-flight, so a pipeline learns before it uploads.
    let (status, body) = post(&app, "/api/v1/admin/plugins/validate", upload()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["valid"], false, "{body}");

    let mut signed = upload();
    signed["signature"] = json!(key.sign(&digest));
    let (status, body) = post(&app, "/api/v1/admin/plugins", signed).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        body["data"]["signature"],
        json!(key.sign(&digest)),
        "{body}"
    );
    let (status, body) = set_status(&app, "test.fixture", "active").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, body) = get(&app, "/api/v1/admin/plugins/test.fixture").await;
    assert_eq!(body["data"]["health"]["state"], "loaded", "{body}");

    // A new version keeps the digest, so it keeps the signature and activates.
    let (status, body) = post(
        &app,
        "/api/v1/admin/plugins/test.fixture/versions",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        body["data"]["signature"],
        json!(key.sign(&digest)),
        "{body}"
    );

    // The load-time check: the same rows, loaded under a node that trusts
    // only the stranger, are a `signature` load issue — never a served function.
    let rows = state.repos.plugins.list_active().await.expect("rows");
    let mut other_policy = state.config.plugins.clone();
    other_policy.trust.public_keys = vec![stranger.public_key_base64()];
    let set = orion::plugin::load_active(
        rows,
        state.repos.plugins.as_ref(),
        state.plugins.as_ref(),
        &other_policy,
    )
    .await;
    assert!(
        set.plugins.is_empty(),
        "nothing loads under the other policy"
    );
    assert_eq!(set.issues.len(), 1, "{:?}", set.issues);
    assert_eq!(set.issues[0].stage, "signature");
    assert!(
        set.unavailable
            .iter()
            .any(|(f, why)| f == "test.fixture.wrap" && why.contains("signature")),
        "{:?}",
        set.unavailable
    );
}

/// `multipart/form-data` is the JSON upload in another shape: the same
/// fields as parts, the component as raw bytes, folded into the same request
/// — so the two forms cannot accept different things.
#[tokio::test]
async fn a_multipart_upload_is_the_json_upload_in_another_shape() {
    let (_state, app) = app(true).await;
    let boundary = "orion-test-boundary";
    let mut body: Vec<u8> = Vec::new();
    let part = |body: &mut Vec<u8>, name: &str, extra: &str, bytes: &[u8]| {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"{extra}\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    };
    part(&mut body, "manifest", "", MANIFEST.as_bytes());
    part(
        &mut body,
        "component",
        "; filename=\"fixture.wasm\"\r\nContent-Type: application/wasm",
        COMPONENT,
    );
    part(&mut body, "tags", "", b"[\"multipart\"]");
    part(&mut body, "tags", "", b"second");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v1/admin/plugins")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(axum::body::Body::from(body))
        .expect("request");
    let resp = app.clone().oneshot(request).await.expect("request");
    let status = resp.status();
    let created = body_json(resp).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(
        created["data"]["digest"],
        json!(orion::plugin::WasmRuntime::digest(COMPONENT)),
        "the bytes arrived intact: {created}"
    );
    assert_eq!(created["data"]["tags"], json!(["multipart", "second"]));

    // A part that is not an upload field is refused, not ignored.
    let mut stray: Vec<u8> = Vec::new();
    part(&mut stray, "manifest", "", MANIFEST.as_bytes());
    part(&mut stray, "components", "", COMPONENT);
    stray.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v1/admin/plugins/validate")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(axum::body::Body::from(stray))
        .expect("request");
    let resp = app.clone().oneshot(request).await.expect("request");
    let status = resp.status();
    let body = body_json(resp).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("'components'"), "{body}");
}

/// The dependency route names the plugin closure the package exporter
/// records: which plugin, at which version and digest, for which functions
/// — and which names this generation cannot resolve at all.
#[tokio::test]
async fn workflow_dependencies_name_the_plugin_version_and_digest() {
    let (_state, app) = app(true).await;
    let plugin = create_fixture(&app).await;
    let digest = plugin["digest"].as_str().expect("digest").to_string();
    let (status, body) = set_status(&app, "test.fixture", "active").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = post(
        &app,
        "/api/v1/admin/workflows",
        wrap_workflow("Wrap, for the dependency route"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let workflow_id = body["data"]["workflow_id"]
        .as_str()
        .expect("id")
        .to_string();
    let (status, deps) = get(
        &app,
        &format!("/api/v1/admin/workflows/{workflow_id}/dependencies"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{deps}");
    assert_eq!(
        deps["data"]["plugins"],
        json!([{"id": "test.fixture", "version": 1, "digest": digest, "functions": ["test.fixture.wrap"]}]),
        "{deps}"
    );
    assert!(deps["data"].get("unresolved_functions").is_none(), "{deps}");

    // Once the plugin is gone from the generation, the same workflow's
    // function is unresolved — what a package export must not silently omit.
    let (status, body) = set_status(&app, "test.fixture", "archived").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, deps) = get(
        &app,
        &format!("/api/v1/admin/workflows/{workflow_id}/dependencies"),
    )
    .await;
    assert_eq!(deps["data"]["plugins"], json!([]), "{deps}");
    assert_eq!(
        deps["data"]["unresolved_functions"],
        json!(["test.fixture.wrap"]),
        "{deps}"
    );
}
