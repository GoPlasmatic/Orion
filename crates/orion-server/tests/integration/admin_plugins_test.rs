//! Plugin definitions through the deployment surface, and a plugin function
//! serving a request through the data plane.
//!
//! Uses the fixture component in `tests/fixtures/plugins/` — eleven functions
//! under the plugin id `test.fixture`, one of which (`wrap`) returns its
//! input inside an object, which is what the data-plane round trip asserts.
//!
//! `POST /definitions` reads a plugin's manifest, compiles and self-tests its
//! component, and checks `[plugins.trust]` before it stores anything; a
//! deployment then publishes the plugin's functions with the generation.

use axum::http::StatusCode;
use base64::Engine as _;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common::{self, body_json, deploy, json_request, test_state_with_config};
use orion::config::AppConfig;
use orion::runtime::state::AppState;

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

/// The fixture as a plugin definition, with `manifest` as its `plugin.toml`
/// text and an optional detached signature.
fn plugin_definition(manifest: &str, signature: Option<&str>) -> Value {
    let mut definition = json!({
        "manifest": manifest,
        "component": component_b64(),
        "tags": ["fixture"],
    });
    if let Some(signature) = signature {
        definition["signature"] = json!(signature);
    }
    json!({"kind": "plugin", "definition": definition})
}

/// A definition whose component is `component`, base64-encoded as given.
fn with_component(component: String) -> Value {
    json!({"kind": "plugin", "definition": {"manifest": MANIFEST, "component": component}})
}

/// The fixture as a stored definition, as `POST /definitions` would store it
/// (its manifest parsed), written straight into `state`'s deployment store —
/// what a definition uploaded elsewhere looks like to the node that loads it.
async fn store_fixture(state: &AppState, signature: Option<&str>) -> String {
    let manifest =
        serde_json::to_value(orion::plugin::Manifest::parse(MANIFEST).expect("manifest"))
            .expect("json");
    let mut definition = json!({"manifest": manifest, "component": component_b64()});
    if let Some(signature) = signature {
        definition["signature"] = json!(signature);
    }
    common::store_definition(state, json!({"kind": "plugin", "definition": definition})).await
}

/// Apply the head plus the plugin member `digest`, answering the apply body.
async fn apply_plugin(app: &axum::Router, digest: String) -> (StatusCode, Value) {
    let (base, members) = deploy::merged(
        app,
        vec![deploy::Upsert {
            kind: "plugin",
            id: "test.fixture".to_string(),
            digest,
            revision: None,
            requires: Vec::new(),
        }],
    )
    .await;
    deploy::apply(app, base, members).await
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

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(json_request("GET", path, None))
        .await
        .expect("request");
    let status = resp.status();
    (status, body_json(resp).await)
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
    let (_, body) = get(app, "/api/v2/admin/functions").await;
    body["data"]
        .as_array()
        .expect("array")
        .iter()
        .find(|e| e["name"] == function)
        .cloned()
}

#[tokio::test]
async fn an_upload_is_validated_compiled_and_stored() {
    let (_state, app) = app(true).await;
    let (status, body) = deploy::try_upload(&app, plugin_definition(MANIFEST, None)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["kind"], "plugin", "{body}");
    let digest = body["digest"].as_str().expect("digest").to_string();
    assert!(
        digest.starts_with("sha256:") && digest.len() == 71,
        "{body}"
    );

    // Stored with its manifest read: the parsed form the loader reads.
    let (status, stored) = get(&app, &format!("/api/v2/admin/definitions/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    let def = &stored["definition"];
    assert_eq!(def["manifest"]["name"], "test.fixture", "{stored}");
    assert_eq!(def["manifest"]["abi"], "orion:plugin@1.0.0");
    assert_eq!(def["manifest"]["version"], "0.0.0");
    assert_eq!(
        def["manifest"]["functions"].as_array().map(Vec::len),
        Some(8)
    );
    assert_eq!(def["component"], component_b64());
    assert_eq!(def["tags"], json!(["fixture"]));

    // An upload adds nothing to the vocabulary until a deployment names it.
    assert!(catalogued(&app, "test.fixture.identity").await.is_none());

    // One content, one digest.
    let again = deploy::upload(&app, plugin_definition(MANIFEST, None)).await;
    assert_eq!(again, digest);
}

#[tokio::test]
async fn bad_uploads_are_refused_with_a_path() {
    let (_state, app) = app(true).await;

    let (status, body) = deploy::try_upload(
        &app,
        plugin_definition(
            &MANIFEST.replace("orion:plugin@1.0.0", "orion:plugin@9.0.0"),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["error"]["details"][0]["path"], "manifest.abi",
        "{body}"
    );

    let (status, body) = deploy::try_upload(&app, with_component("not base64!".into())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["details"][0]["path"], "component", "{body}");

    let (status, body) = deploy::try_upload(
        &app,
        with_component(base64::engine::general_purpose::STANDARD.encode(b"not a component")),
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
}

/// A deployed plugin publishes its functions with the generation — catalogue,
/// health, registry — and a workflow serves through one. A deployment that
/// drops the plugin while a deployed workflow still calls it does not build:
/// it is committed but not converged, and the previous generation keeps
/// serving. Once the dependants go too, its functions leave the vocabulary.
#[tokio::test]
async fn a_deployed_plugin_publishes_its_functions_and_its_dependants_hold_it() {
    let (state, app) = app(true).await;
    common::deploy_plugin(&app, MANIFEST, COMPONENT).await;
    let digest = orion::plugin::WasmRuntime::digest(COMPONENT);

    let entry = catalogued(&app, "test.fixture.identity")
        .await
        .expect("a deployed plugin's functions are catalogued");
    assert_eq!(entry["source"], "plugin");
    assert_eq!(entry["plugin"]["id"], "test.fixture");
    assert_eq!(entry["plugin"]["digest"], digest);
    assert_eq!(entry["retry_safety"]["kind"], "pure");
    let generation = state.runtime.load();
    assert!(generation.functions.contains("test.fixture.wrap"));
    assert_eq!(generation.plugins.plugins.len(), 1);
    assert!(generation.plugins.issues.is_empty());
    let (_, health) = get(&app, "/health").await;
    assert_eq!(health["components"]["plugins"], "ok", "{health}");
    assert_eq!(
        health["plugins"]["loaded"][0]["plugin"], "test.fixture",
        "{health}"
    );
    assert!(health["plugins"]["loaded"][0]["compile_ms"].is_number());

    let (channel, workflow_id) =
        common::create_and_activate_channel(&app, "plugin-wrap", wrap_workflow("Wrap via plugin"))
            .await;
    let (status, body) = post(
        &app,
        &format!("/api/v2/data/{channel}"),
        json!({"data": {"msg": "hi"}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["result"]["wrapped"]["message"], "hi", "{body}");
    assert_eq!(body["data"]["result"]["len"], 16, "{body}");

    // Dropping the plugin under its dependant would not build, so it is
    // refused before it commits.
    let (base, mut members) = deploy::head(&app).await;
    members.retain(|m| m["entity"]["kind"] != "plugin");
    let (status, body) = deploy::apply(&app, base, members).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["error"]["code"], "DEPLOYMENT_WOULD_NOT_BUILD",
        "{body}"
    );
    let error = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        error.contains(&workflow_id) && error.contains("test.fixture.wrap"),
        "the refusal names the dependant and the function: {body}"
    );
    assert_eq!(deploy::head(&app).await.0, base, "nothing is committed");
    let (status, body) = post(
        &app,
        &format!("/api/v2/data/{channel}"),
        json!({"data": {"msg": "still"}}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the previous generation keeps serving: {body}"
    );
    assert!(state.runtime.load().functions.contains("test.fixture.wrap"));

    // With the dependants gone, the plugin can go, and its functions leave.
    deploy::remove(
        &app,
        &[
            ("channel", &channel),
            ("workflow", &workflow_id),
            ("plugin", "test.fixture"),
        ],
    )
    .await;
    assert!(catalogued(&app, "test.fixture.identity").await.is_none());
    assert!(!state.runtime.load().functions.contains("test.fixture.wrap"));
}

/// A deployed plugin publishes its functions, and a deployed workflow serves
/// through one — the data-plane half of the test above, deployed through v2.
#[tokio::test]
async fn a_deployed_plugin_serves_a_deployed_workflow() {
    let (state, app) = app(true).await;
    let id = common::deploy_plugin(&app, MANIFEST, COMPONENT).await;
    assert_eq!(id, "test.fixture");
    let generation = state.runtime.load();
    assert!(generation.functions.contains("test.fixture.wrap"));
    assert!(generation.plugins.issues.is_empty());

    let (channel, _) =
        common::create_and_activate_channel(&app, "plugin-wrap", wrap_workflow("Wrap")).await;
    let (status, body) = post(
        &app,
        &format!("/api/v2/data/{channel}"),
        json!({"data": {"msg": "hi"}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["result"]["wrapped"]["message"], "hi", "{body}");
}

/// A plugin and the workflow that calls it arrive in one deployment: the
/// workflow uploads before any generation serves the plugin's functions,
/// and the deployment's build compiles it against the plugin it carries.
#[tokio::test]
async fn a_plugin_and_its_dependant_deploy_together() {
    let (state, app) = app(true).await;
    assert!(!state.runtime.load().functions.contains("test.fixture.wrap"));

    let plugin = deploy::upload(&app, plugin_definition(MANIFEST, None)).await;
    let workflow = deploy::upload(
        &app,
        deploy::workflow_definition(&wrap_workflow("Wrap, deployed together")),
    )
    .await;
    let channel = deploy::upload(
        &app,
        deploy::channel_definition(&common::sync_http_channel("wrap-together", "wrap-wf")).0,
    )
    .await;
    deploy::deploy(
        &app,
        vec![
            deploy::Upsert {
                kind: "plugin",
                id: "test.fixture".to_string(),
                digest: plugin,
                revision: None,
                requires: Vec::new(),
            },
            deploy::Upsert {
                kind: "workflow",
                id: "wrap-wf".to_string(),
                digest: workflow,
                revision: None,
                requires: Vec::new(),
            },
            deploy::Upsert {
                kind: "channel",
                id: "wrap-together".to_string(),
                digest: channel,
                revision: None,
                requires: vec![("workflow", "wrap-wf".to_string())],
            },
        ],
    )
    .await;

    let (status, body) = post(
        &app,
        "/api/v2/data/wrap-together",
        json!({"data": {"msg": "hi"}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["result"]["wrapped"]["message"], "hi", "{body}");
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
/// secret at upload, and — the schema gate — a new revision whose table a
/// deployed dependant no longer satisfies does not build while it is there.
#[tokio::test]
async fn a_template_field_serves_refuses_secrets_and_gates_a_schema_change() {
    let (_state, app) = app(true).await;
    common::deploy_plugin(&app, MANIFEST, COMPONENT).await;

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

    let (channel, workflow_id) = common::create_and_activate_channel(
        &app,
        "plugin-upper",
        upper_workflow(
            "Upper via plugin",
            json!({"cat": ["hello ", {"var": "data.input.who"}]}),
        ),
    )
    .await;
    let (status, body) = post(
        &app,
        &format!("/api/v2/data/{channel}"),
        json!({"data": {"who": "world"}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["up"]["TEXT"], "HELLO WORLD", "{body}");

    // A secret node in the template field is refused at upload, at its
    // path: the engine would evaluate it and record the result, and a plugin
    // never sees key material.
    let body = deploy::assert_upload_refused(
        &app,
        deploy::workflow_definition(&upper_workflow(
            "Upper with a secret",
            json!({"secret": "api_key"}),
        )),
        "definition.tasks[1].function.input.text",
    )
    .await;
    assert!(
        body["error"]["details"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("secret")),
        "{body}"
    );

    // A new revision renames the field. Its upload is fine on its own; the
    // deployment naming it beside the workflow that uses `text` would not
    // build, so it is refused, naming the workflow and the mismatch.
    let renamed = MANIFEST.replace("name = \"text\"", "name = \"payload\"");
    assert_ne!(renamed, MANIFEST);
    let digest = deploy::upload(&app, plugin_definition(&renamed, None)).await;
    let (status, body) = apply_plugin(&app, digest.clone()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["error"]["code"], "DEPLOYMENT_WOULD_NOT_BUILD",
        "{body}"
    );
    let local_error = body["error"]["message"].as_str().unwrap_or("");
    assert!(local_error.contains(&workflow_id), "{body}");
    assert!(
        local_error.contains("unknown key `text`"),
        "the failure names the field the new table dropped: {body}"
    );
    assert!(
        local_error.contains("missing field `payload`"),
        "and the one it now requires: {body}"
    );

    // Revision 1 kept serving throughout.
    let (status, body) = post(
        &app,
        &format!("/api/v2/data/{channel}"),
        json!({"data": {"who": "still"}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["up"]["TEXT"], "HELLO STILL", "{body}");

    // Once the dependant is gone, the new schema deploys and serves.
    deploy::remove(&app, &[("channel", &channel), ("workflow", &workflow_id)]).await;
    let (status, body) = apply_plugin(&app, digest).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
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

/// A changed definition under the same id is the plugin's next revision, and
/// the catalogue serves it.
#[tokio::test]
async fn a_new_revision_supersedes_the_deployed_one() {
    let (_state, app) = app(true).await;
    common::deploy_plugin(&app, MANIFEST, COMPONENT).await;
    let entry = catalogued(&app, "test.fixture.identity")
        .await
        .expect("served");
    assert_eq!(entry["plugin"]["version"], 1, "{entry}");

    // Only the tags differ, so it is new content over the same component.
    let mut changed = plugin_definition(MANIFEST, None);
    changed["definition"]["tags"] = json!(["v2"]);
    let digest = deploy::upload(&app, changed).await;
    let (status, body) = apply_plugin(&app, digest.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["evaluation"]["converged"], true, "{body}");

    let (_, members) = deploy::head(&app).await;
    let member = members
        .iter()
        .find(|m| m["entity"]["kind"] == "plugin")
        .expect("the plugin member");
    assert_eq!(member["entity"]["revision"], 2, "{member}");
    assert_eq!(member["definition_digest"], digest.as_str());
    let entry = catalogued(&app, "test.fixture.identity")
        .await
        .expect("served");
    assert_eq!(entry["plugin"]["version"], 2, "{entry}");
}

#[tokio::test]
async fn a_node_with_plugins_disabled_refuses_uploads_and_reports_it() {
    let (state, app) = app(false).await;
    let (status, body) = deploy::try_upload(&app, plugin_definition(MANIFEST, None)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("plugins.enabled")),
        "{body}"
    );
    let (_, health) = get(&app, "/health").await;
    assert_eq!(health["components"]["plugins"], "disabled", "{health}");

    // A plugin member reaching this node — uploaded on a peer that can
    // compile it, and deployed — is a load issue, not an abort.
    let digest = store_fixture(&state, None).await;
    let (status, body) = apply_plugin(&app, digest).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let (_, health) = get(&app, "/health").await;
    assert_eq!(health["components"]["plugins"], "degraded", "{health}");
    let generation = state.runtime.load();
    assert_eq!(generation.plugins.issues.len(), 1);
    assert_eq!(generation.plugins.issues[0].stage, "disabled");
    assert!(!generation.functions.contains("test.fixture.wrap"));
    assert_eq!(
        health["plugins"]["failed_to_load"][0]["stage"], "disabled",
        "{health}"
    );
}

#[tokio::test]
async fn the_definition_route_takes_a_megabyte_scale_component() {
    // A 1.5 MiB component must reach the handler and be refused as a
    // component, not by the admin body limit.
    let (_state, app) = app(true).await;
    let big = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 1_500_000]);
    let (status, body) = deploy::try_upload(&app, with_component(big)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["details"][0]["path"], "component", "{body}");
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

/// The contract between `orion-cli plugin sign` and the upload check: a
/// `.sig` file the verb wrote is accepted, as-is, by a node that trusts the
/// key `plugin keygen` printed.
#[tokio::test]
async fn a_signature_from_plugin_sign_is_accepted_at_upload() {
    let Some(bin) = common::orion_cli_bin() else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("orion-plugin-sign-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let key = dir.join("signer.pem");
    let component = dir.join("fixture.wasm");
    std::fs::write(&component, COMPONENT).expect("component");
    let run = |args: &[&std::ffi::OsStr]| {
        let out = std::process::Command::new(&bin)
            .args(args)
            .output()
            .expect("invoke orion-cli");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    run(&[
        "plugin".as_ref(),
        "keygen".as_ref(),
        "-o".as_ref(),
        key.as_os_str(),
    ]);
    let public = run(&[
        "plugin".as_ref(),
        "pubkey".as_ref(),
        "--key".as_ref(),
        key.as_os_str(),
    ]);
    run(&[
        "plugin".as_ref(),
        "sign".as_ref(),
        component.as_os_str(),
        "--key".as_ref(),
        key.as_os_str(),
    ]);
    let signature = std::fs::read_to_string(dir.join("fixture.wasm.sig")).expect("sig");
    let _ = std::fs::remove_dir_all(&dir);

    let (_state, app) = trusting_app(vec![public.trim().to_string()]).await;
    let (status, body) =
        deploy::try_upload(&app, plugin_definition(MANIFEST, Some(&signature))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// `[plugins.trust]` end to end: refused without a signature, refused with
/// one by a key the node does not trust, accepted with a good one and stored
/// with it; and the node that *loads* the plugin checks again with its own
/// keys, so a definition that verified where it was uploaded is still refused
/// on a node whose policy differs.
#[tokio::test]
async fn a_trusting_node_requires_a_signature_over_the_digest_at_upload_and_at_load() {
    use orion::plugin::trust::SigningKey;
    let key = SigningKey::generate();
    let stranger = SigningKey::generate();
    let (state, app) = trusting_app(vec![key.public_key_base64()]).await;
    let digest = orion::plugin::WasmRuntime::digest(COMPONENT);

    let (status, body) = deploy::try_upload(&app, plugin_definition(MANIFEST, None)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["details"][0]["path"], "signature", "{body}");
    assert_eq!(body["error"]["details"][0]["code"], "REQUIRED", "{body}");

    let (status, body) = deploy::try_upload(
        &app,
        plugin_definition(MANIFEST, Some(&stranger.sign(&digest))),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["details"][0]["code"], "INVALID", "{body}");
    assert!(
        body["error"]["details"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("does not verify")),
        "{body}"
    );

    let signature = key.sign(&digest);
    let stored = deploy::upload(&app, plugin_definition(MANIFEST, Some(&signature))).await;
    let (_, body) = get(&app, &format!("/api/v2/admin/definitions/{stored}")).await;
    assert_eq!(body["definition"]["signature"], json!(signature), "{body}");
    let (status, body) = apply_plugin(&app, stored).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["evaluation"]["converged"], true, "{body}");
    assert!(state.runtime.load().functions.contains("test.fixture.wrap"));

    // The load-time check: the same signed definition, deployed to a node
    // that trusts only the stranger, is a `signature` load issue — never a
    // served function.
    let (other, other_app) = trusting_app(vec![stranger.public_key_base64()]).await;
    let stored = store_fixture(&other, Some(&signature)).await;
    let (status, body) = apply_plugin(&other_app, stored).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let generation = other.runtime.load();
    assert!(
        generation.plugins.plugins.is_empty(),
        "nothing loads under the other policy"
    );
    assert_eq!(
        generation.plugins.issues.len(),
        1,
        "{:?}",
        generation.plugins.issues
    );
    assert_eq!(generation.plugins.issues[0].stage, "signature");
    assert!(!generation.functions.contains("test.fixture.wrap"));
}
