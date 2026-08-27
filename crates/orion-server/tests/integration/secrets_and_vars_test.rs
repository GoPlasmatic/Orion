//! `[vars]` and `[secrets]`: the two ways an operator declares a value that
//! workflow expressions may read, and the one difference between them.
//!
//! A var is stamped into every message's `metadata.vars`, so it is part of the
//! message and part of every trace — which is the point: an operator debugging
//! "which topic did this run publish to?" needs to see it. A secret is held by
//! the engine (dataflow-rs 3.8), reached through `{"secret": "name"}`, and is
//! never part of a message at all, so there is nothing to strip from a trace.
//!
//! What this file pins is the pair of properties that make the split worth
//! having, and it pins them from both ends:
//!
//! * a var is **unforgeable** — stamped last, over whatever the caller sent,
//!   on every ingress — and **recorded**, which is the deliberate half;
//! * a secret is **unrecordable** — absent from the response, the metadata
//!   echo, the served trace, the per-step task trace and the row on disk.
//!
//! The leak sweep is deliberately structural rather than a spot check. One
//! run exercises every way a workflow can touch a secret (a condition, an HMAC
//! key, a JWT signing key), captures per-step task details, and then every
//! surface that could carry a value out is asserted against — including the
//! database row itself, which the read path masks and the API therefore cannot
//! prove clean.

use axum::http::StatusCode;
use orion::config::{AppConfig, SecretsConfig, VarsConfig};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common::{self, body_json, json_request};

/// The variables the `[secrets]` fixture below resolves, and their values.
///
/// Three, because the leak sweep needs one secret per shape a workflow can
/// consume: a raw HMAC key, an HS256 JWT key, and a plain value read from a
/// condition. Sharing one would let a single surface's cleanliness stand in
/// for all of them.
const SECRET_ENV: &str = "ORION_SECRET_VARS_FIXTURE_KEY";
const SECRET_VALUE: &str = "hmac-fixture-value-9f8e";
const JWT_ENV: &str = "ORION_SECRET_VARS_FIXTURE_JWT";
const JWT_VALUE: &str = "jwt-fixture-signing-key-4c3d2e1f";
const GATE_ENV: &str = "ORION_SECRET_VARS_FIXTURE_GATE";
const GATE_VALUE: &str = "gate-fixture-value-7b6a";

/// Every declared secret's value, for the sweep that asserts none of them
/// reaches a recording surface.
const ALL_SECRET_VALUES: &[&str] = &[SECRET_VALUE, JWT_VALUE, GATE_VALUE];

/// Set pre-main, on the process's sole thread — the only context where
/// `set_var` cannot race a concurrent `getenv` from another test's runtime
/// threads, which is why it is `unsafe` in Rust 2024. Same reasoning, and same
/// reserved `ORION_SECRET_*` namespace, as `secret_references_test`: every
/// other `ORION_*` name is refused at startup as a misspelled override.
#[ctor::ctor(unsafe)]
fn install_secret_fixture() {
    // SAFETY: runs pre-main on the sole thread of the process; no concurrent
    // reader of the environment can exist yet.
    unsafe {
        std::env::set_var(SECRET_ENV, SECRET_VALUE);
        std::env::set_var(JWT_ENV, JWT_VALUE);
        std::env::set_var(GATE_ENV, GATE_VALUE);
    }
}

/// A config declaring vars and secrets — the secrets as references, the vars
/// as literals, which is the only shape each section accepts.
fn config_with_vars_and_secrets() -> AppConfig {
    let mut config = AppConfig::default();
    config.trace_storage.mode = orion::config::TraceStorageMode::Sync;
    config.vars = VarsConfig(
        [
            (
                "topic_prefix".to_string(),
                toml::Value::String("eu-west".to_string()),
            ),
            ("max_retries".to_string(), toml::Value::Integer(3)),
            ("debug_mode".to_string(), toml::Value::Boolean(false)),
            (
                "regions".to_string(),
                toml::Value::Array(vec![
                    toml::Value::String("eu".to_string()),
                    toml::Value::String("us".to_string()),
                ]),
            ),
        ]
        .into_iter()
        .collect(),
    );
    config.secrets = SecretsConfig(
        [
            ("partner_hmac".to_string(), format!("env://{SECRET_ENV}")),
            ("jwt_key".to_string(), format!("env://{JWT_ENV}")),
            ("gate_value".to_string(), format!("env://{GATE_ENV}")),
        ]
        .into_iter()
        .collect(),
    );
    config
}

/// A workflow that copies the whole metadata object into `data`, so a test can
/// see exactly what the engine received.
fn metadata_echo_workflow(name: &str) -> Value {
    common::workflow_with_tasks(
        name,
        json!([{
            "id": "capture", "name": "Capture metadata",
            "function": { "name": "map", "input": { "mappings": [
                { "path": "data.meta", "logic": { "var": "metadata" } }
            ] } }
        }]),
    )
}

async fn post(app: &axum::Router, channel: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/data/{channel}"),
            Some(body),
        ))
        .await
        .expect("request");
    let status = resp.status();
    (status, body_json(resp).await)
}

/// Create a workflow through the admin API and return `(status, body)` — the
/// authoring-time checks are asserted on the refusal, so they need the raw
/// response rather than a helper that unwraps a 201.
async fn create_workflow(app: &axum::Router, workflow: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(workflow),
        ))
        .await
        .expect("create workflow");
    let status = resp.status();
    (status, body_json(resp).await)
}

// ---------------------------------------------------------------------------
// vars — declaration and shape
// ---------------------------------------------------------------------------

/// The declared values reach the workflow under `metadata.vars`, with the
/// types they were written as — `max_retries` compares against `3`, not `"3"`,
/// and a TOML array arrives as a JSON array rather than its rendered text.
#[tokio::test]
async fn declared_vars_reach_a_workflow_with_their_types() {
    let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
    common::create_and_activate_channel(&app, "vars-ch", metadata_echo_workflow("vars wf")).await;

    let (status, body) = post(&app, "vars-ch", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let vars = &body["data"]["meta"]["vars"];
    assert_eq!(vars["topic_prefix"], json!("eu-west"), "{body}");
    assert_eq!(
        vars["max_retries"],
        json!(3),
        "a var must keep the type it was written as: {body}"
    );
    assert_eq!(vars["debug_mode"], json!(false), "{body}");
    assert_eq!(
        vars["regions"],
        json!(["eu", "us"]),
        "a structured var must survive as structure, not as its rendered text: {body}"
    );
}

/// A var is readable the way the documentation says to read it — through
/// `{"var": "metadata.vars.<name>"}` inside an ordinary expression, not only
/// as part of a whole-metadata echo.
#[tokio::test]
async fn a_var_is_readable_through_an_expression() {
    let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
    common::create_and_activate_channel(
        &app,
        "vars-expr-ch",
        common::workflow_with_tasks(
            "vars expr wf",
            json!([{
                "id": "compose", "name": "Compose",
                "function": { "name": "map", "input": { "mappings": [
                    { "path": "data.topic", "logic": { "cat": [
                        { "var": "metadata.vars.topic_prefix" }, ".order.placed"
                    ] } }
                ] } }
            }]),
        ),
    )
    .await;

    let (status, body) = post(&app, "vars-expr-ch", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["data"]["topic"],
        json!("eu-west.order.placed"),
        "{body}"
    );
}

// ---------------------------------------------------------------------------
// vars — unforgeable at every ingress
// ---------------------------------------------------------------------------

/// **The forgery test.** Envelope mode merges the caller's own `metadata`
/// wholesale, so `vars` has to be stamped *over* whatever arrived — otherwise a
/// request could name the topic prefix its own run publishes to.
#[tokio::test]
async fn a_caller_cannot_forge_a_var() {
    let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
    common::create_and_activate_channel(&app, "forge-ch", metadata_echo_workflow("forge wf")).await;

    let (status, body) = post(
        &app,
        "forge-ch",
        json!({
            "data": {},
            "metadata": { "vars": { "topic_prefix": "attacker-controlled" } }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["data"]["meta"]["vars"]["topic_prefix"],
        json!("eu-west"),
        "a caller-supplied var must be overwritten by the declared one: {body}"
    );
}

/// The stamp replaces the whole object rather than merging into it. A merge
/// would let a caller *add* a name the instance never declared, which a
/// workflow reading `metadata.vars.anything` would then trust.
#[tokio::test]
async fn a_caller_cannot_add_a_var_the_instance_never_declared() {
    let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
    common::create_and_activate_channel(&app, "add-ch", metadata_echo_workflow("add wf")).await;

    let (status, body) = post(
        &app,
        "add-ch",
        json!({
            "data": {},
            "metadata": { "vars": { "smuggled": "attacker-controlled" } }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let vars = &body["data"]["meta"]["vars"];
    assert!(
        vars.get("smuggled").is_none(),
        "the stamp must replace the object, not merge into it: {body}"
    );
    assert_eq!(vars["topic_prefix"], json!("eu-west"), "{body}");
}

/// A non-object `vars` is replaced too. A caller sending a string would
/// otherwise leave `metadata.vars` unreadable for every expression in the
/// workflow, which is a denial of service on the instance's own configuration.
#[tokio::test]
async fn a_non_object_var_payload_is_replaced_rather_than_left_in_place() {
    let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
    common::create_and_activate_channel(&app, "shape-ch", metadata_echo_workflow("shape wf")).await;

    let (status, body) = post(
        &app,
        "shape-ch",
        json!({ "data": {}, "metadata": { "vars": "not-an-object" } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["data"]["meta"]["vars"]["topic_prefix"],
        json!("eu-west"),
        "{body}"
    );
}

/// An instance that declares no vars stamps *nothing* — not an empty object,
/// and not whatever the caller sent. The removal is what makes the key
/// unforgeable when there is nothing to overwrite it with.
#[tokio::test]
async fn an_instance_with_no_vars_strips_the_key() {
    let app = common::test_app().await;
    common::create_and_activate_channel(&app, "novars-ch", metadata_echo_workflow("novars wf"))
        .await;

    let (status, body) = post(
        &app,
        "novars-ch",
        json!({ "data": {}, "metadata": { "vars": { "topic_prefix": "attacker" } } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["data"]["meta"].get("vars").is_none(),
        "no [vars] section must mean no `vars` key at all: {body}"
    );
}

/// The async ingress is the same `build_request_metadata` call, and a workflow
/// must not read different deployment values depending on whether the caller
/// waited for the answer.
#[tokio::test]
async fn the_async_ingress_stamps_the_same_vars() {
    let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
    common::create_and_activate_channel(&app, "async-ch", metadata_echo_workflow("async wf")).await;

    let (trace_id, token) = common::submit_async(
        &app,
        "/api/v1/data/async-ch/async",
        json!({ "data": {}, "metadata": { "vars": { "topic_prefix": "attacker" } } }),
    )
    .await;
    let trace = common::poll_trace_until_done(&app, &trace_id, 60, Some(&token)).await;
    let rendered = trace["message"].to_string();
    assert!(
        rendered.contains("eu-west"),
        "the async run must see the declared var: {trace}"
    );
    assert!(
        !rendered.contains("attacker"),
        "the async ingress must overwrite a caller-supplied var: {trace}"
    );
}

/// `POST /admin/workflows/{id}/test` builds its own message, so it needs its
/// own stamp — otherwise "test this workflow" answers differently from the
/// request it is meant to stand in for.
#[tokio::test]
async fn the_admin_test_endpoint_stamps_the_same_vars() {
    let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
    let wf_id = common::create_and_activate_workflow(&app, metadata_echo_workflow("test-ep wf"))
        .await
        .to_string();

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/admin/workflows/{wf_id}/test"),
            Some(json!({
                "data": {},
                "metadata": { "vars": { "topic_prefix": "attacker" } }
            })),
        ))
        .await
        .expect("test endpoint");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["data"]["output"]["meta"]["vars"]["topic_prefix"],
        json!("eu-west"),
        "the test endpoint must stamp what a real request would: {body}"
    );
}

/// The stamping helper itself, in isolation. The Kafka ingress reaches vars
/// through this same function, and its own end-to-end path is container-gated
/// — so the contract both transports share is pinned where it always runs.
#[test]
fn the_stamp_replaces_or_removes_but_never_merges() {
    let declared = json!({ "topic_prefix": "eu-west" });

    let mut metadata = json!({ "channel": "c", "vars": { "topic_prefix": "forged", "extra": 1 } });
    orion::engine::stamp_vars(&mut metadata, Some(&declared));
    assert_eq!(metadata["vars"], declared, "declared vars must replace");
    assert_eq!(metadata["channel"], json!("c"), "nothing else is touched");

    let mut metadata = json!({ "channel": "c", "vars": { "topic_prefix": "forged" } });
    orion::engine::stamp_vars(&mut metadata, None);
    assert!(
        metadata.get("vars").is_none(),
        "no declared vars must strip the key: {metadata}"
    );

    // A non-object metadata becomes one rather than being skipped. Skipping is
    // what let `POST /workflows/{id}/test` serve a caller-omitted `metadata`
    // (which deserialises to `null`) with no vars at all, while the data route
    // — which normalises first — stamped them.
    let mut metadata = json!("not-an-object");
    orion::engine::stamp_vars(&mut metadata, Some(&declared));
    assert_eq!(metadata["vars"], declared, "{metadata}");
}

// ---------------------------------------------------------------------------
// vars — recorded, deliberately
// ---------------------------------------------------------------------------

/// The other half of the contract, and the reason vars are not secrets: a var
/// **does** reach the persisted per-step trace, so an operator asking "which
/// topic did this run publish to?" can answer it from the record.
#[tokio::test]
async fn a_var_is_visible_in_the_persisted_task_trace() {
    let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
    common::create_and_activate_channel_with_config(
        &app,
        "vars-trace-ch",
        metadata_echo_workflow("vars trace wf"),
        json!({ "tracing": { "task_details": true } }),
    )
    .await;

    let (status, body) = post(&app, "vars-trace-ch", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let list = common::wait_for_body(&app, "/api/v1/admin/traces", |b| {
        b["data"]
            .as_array()
            .is_some_and(|a| a.iter().any(|r| r["channel"] == "vars-trace-ch"))
    })
    .await;
    let trace_id = list["data"]
        .as_array()
        .and_then(|a| a.iter().find(|r| r["channel"] == "vars-trace-ch"))
        .and_then(|r| r["id"].as_str())
        .expect("a trace row for the run")
        .to_string();

    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/traces/{trace_id}"),
            None,
        ))
        .await
        .expect("trace detail");
    let detail = body_json(resp).await;
    assert!(
        detail.to_string().contains("eu-west"),
        "a var must be recorded — that is the whole reason it is not a secret: {detail}"
    );
}

// ---------------------------------------------------------------------------
// vars — the config section's own rules
// ---------------------------------------------------------------------------

/// Each section refuses the other's value shape, and it refuses it *at the
/// gate an operator actually hits* — `load_config`, not a hand-called
/// predicate. The unit tests in `config::vars` cover the rules themselves;
/// what these pin is that both checks are wired into the boot path, which is
/// what decides whether a misdeclared instance starts.
#[test]
fn a_misdeclared_section_stops_the_boot() {
    let scratch = common::ScratchDir::new("vars-config");
    for (name, body, expected) in [
        (
            "vars-reference.toml",
            "[vars]\ntoken = \"env://SOMETHING\"\n",
            "[secrets]",
        ),
        (
            "secrets-literal.toml",
            "[secrets]\ntoken = \"sk-live-abcdef\"\n",
            "env://",
        ),
        (
            "bad-name.toml",
            "[vars]\n\"not.an.identifier\" = \"x\"\n",
            "identifier",
        ),
    ] {
        let path = write_scratch(scratch.path(), name, body);
        let err = orion::config::load_config(Some(&path))
            .expect_err("a misdeclared section must not boot");
        assert!(
            err.to_string().contains(expected),
            "'{name}' must explain itself: {err}"
        );
    }
}

/// The accepted shapes load, so the refusals above are not simply refusing
/// everything.
#[test]
fn a_well_formed_pair_of_sections_loads() {
    let scratch = common::ScratchDir::new("vars-config-ok");
    let path = write_scratch(
        scratch.path(),
        "ok.toml",
        &format!(
            "[vars]\ntopic_prefix = \"eu-west\"\nmax_retries = 3\n\n\
             [secrets]\npartner_hmac = \"env://{SECRET_ENV}\"\n"
        ),
    );
    let config = orion::config::load_config(Some(&path)).expect("a well-formed config loads");
    assert_eq!(
        config.vars.to_json().expect("non-empty")["max_retries"],
        json!(3)
    );
    assert_eq!(config.secrets.iter().count(), 1);
}

// ---------------------------------------------------------------------------
// secrets — the value resolves, in every field that takes one
// ---------------------------------------------------------------------------

fn hmac_workflow(name: &str, key: Value) -> Value {
    common::workflow_with_tasks(
        name,
        json!([{
            "id": "sign", "name": "Sign",
            "function": { "name": "crypto", "input": {
                "op": "hmac", "algorithm": "sha256",
                "key": key,
                "data": "order-4711",
                "output": "data.mac"
            } }
        }]),
    )
}

/// A workflow signing with `{"secret": "partner_hmac"}` produces the same MAC
/// as one signing with the literal value — so the store resolved, and resolved
/// to the right thing.
#[tokio::test]
async fn a_declared_secret_signs_the_same_as_the_literal() {
    let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
    common::create_and_activate_channel(
        &app,
        "sign-store",
        hmac_workflow("store wf", json!({"secret": "partner_hmac"})),
    )
    .await;
    common::create_and_activate_channel(
        &app,
        "sign-literal",
        hmac_workflow("literal wf", json!(SECRET_VALUE)),
    )
    .await;

    let payload = json!({"data": {"payload": "order-4711"}});
    let (store_status, store_body) = post(&app, "sign-store", payload.clone()).await;
    let (literal_status, literal_body) = post(&app, "sign-literal", payload).await;
    assert_eq!(store_status, StatusCode::OK, "{store_body}");
    assert_eq!(literal_status, StatusCode::OK, "{literal_body}");

    let mac = store_body["data"]["mac"].as_str().unwrap_or_default();
    assert!(!mac.is_empty(), "no MAC was produced: {store_body}");
    assert_eq!(
        store_body["data"]["mac"], literal_body["data"]["mac"],
        "the store must resolve to exactly the declared value"
    );
}

/// `jwt_sign.key` is the second of the five secret-bearing fields. The proof
/// that it resolved to the *right* value is that `jwt_verify` accepts the token
/// against the literal — a wrong key would still mint a well-formed JWS.
#[tokio::test]
async fn jwt_sign_and_verify_both_read_the_store() {
    let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
    common::create_and_activate_channel(
        &app,
        "jwt-ch",
        common::workflow_with_tasks(
            "jwt wf",
            json!([
                {
                    "id": "sign", "name": "Sign",
                    "function": { "name": "jwt_sign", "input": {
                        "algorithm": "HS256",
                        "key": { "secret": "jwt_key" },
                        "claims": { "sub": "user-1" },
                        "issuer": "orion-test",
                        "expires_in": "5m",
                        "output": "data.token"
                    } }
                },
                {
                    "id": "verify", "name": "Verify",
                    "function": { "name": "jwt_verify", "input": {
                        "token": { "var": "data.token" },
                        "algorithms": ["HS256"],
                        "keys": [{ "algorithm": "HS256", "key": { "secret": "jwt_key" } }],
                        "issuer": "orion-test",
                        "output": "data.claims"
                    } }
                }
            ]),
        ),
    )
    .await;

    let (status, body) = post(&app, "jwt-ch", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["data"]["token"]
            .as_str()
            .is_some_and(|t| !t.is_empty()),
        "jwt_sign must mint a token from the store: {body}"
    );
    assert_eq!(
        body["data"]["claims"]["sub"],
        json!("user-1"),
        "jwt_verify must accept a token signed with the same stored key: {body}"
    );
}

/// `jwt_verify`'s `issuer` and `audience` take the store too — the OAuth
/// client-id case, where the accepted value is itself a credential. `issuer`
/// is a single expression rather than an array, so this also pins that the
/// `{"secret": …}` node survives `string_or_vec`'s array/scalar split.
#[tokio::test]
async fn jwt_verify_reads_the_store_for_issuer_and_audience() {
    let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
    common::create_and_activate_channel(
        &app,
        "jwt-aud-ch",
        common::workflow_with_tasks(
            "jwt aud wf",
            json!([
                {
                    "id": "sign", "name": "Sign",
                    "function": { "name": "jwt_sign", "input": {
                        "algorithm": "HS256",
                        "key": { "secret": "jwt_key" },
                        "claims": { "sub": "user-1" },
                        // The signed values are the secret's *value*, which is
                        // what `issuer`/`audience` must resolve to below.
                        "issuer": GATE_VALUE,
                        "audience": GATE_VALUE,
                        "expires_in": "5m",
                        "output": "data.token"
                    } }
                },
                {
                    "id": "verify", "name": "Verify",
                    "function": { "name": "jwt_verify", "input": {
                        "token": { "var": "data.token" },
                        "algorithms": ["HS256"],
                        "keys": [{ "algorithm": "HS256", "key": { "secret": "jwt_key" } }],
                        "issuer": { "secret": "gate_value" },
                        "audience": { "secret": "gate_value" },
                        "output": "data.claims"
                    } }
                }
            ]),
        ),
    )
    .await;

    let (status, body) = post(&app, "jwt-aud-ch", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["data"]["claims"]["sub"],
        json!("user-1"),
        "issuer/audience read from the store must match the signed claims: {body}"
    );
}

/// A secret in a **task condition** is the one place an expression may read one
/// — nothing of the value survives a bool. That the guarded task ran is the
/// only observable, and it is only possible if the operator resolved.
#[tokio::test]
async fn a_secret_gates_a_task_condition() {
    let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
    common::create_and_activate_channel(
        &app,
        "gate-ch",
        common::workflow_with_tasks(
            "gate wf",
            json!([
                {
                    "id": "matched", "name": "Matched",
                    "condition": { "==": [{ "secret": "gate_value" }, GATE_VALUE] },
                    "function": { "name": "map", "input": { "mappings": [
                        { "path": "data.matched", "logic": true }
                    ] } }
                },
                {
                    "id": "mismatched", "name": "Mismatched",
                    "condition": { "==": [{ "secret": "gate_value" }, "wrong"] },
                    "function": { "name": "map", "input": { "mappings": [
                        { "path": "data.mismatched", "logic": true }
                    ] } }
                }
            ]),
        ),
    )
    .await;

    let (status, body) = post(&app, "gate-ch", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["data"]["matched"],
        json!(true),
        "the condition comparing against the declared value must hold: {body}"
    );
    assert!(
        body["data"].get("mismatched").is_none(),
        "and the one comparing against the wrong value must not: {body}"
    );
}

/// The store is per-engine, and a reload builds a new one. `with_new_workflows`
/// carries the secrets forward upstream, but Orion rebuilds from its own
/// `ResolvedSecrets` — either way a reload must not silently empty the store
/// and quarantine every channel that names a secret.
#[tokio::test]
async fn an_engine_reload_keeps_the_store() {
    let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
    common::create_and_activate_channel(
        &app,
        "reload-ch",
        hmac_workflow("reload wf", json!({"secret": "partner_hmac"})),
    )
    .await;

    let (before_status, before) = post(&app, "reload-ch", json!({})).await;
    assert_eq!(before_status, StatusCode::OK, "{before}");

    let resp = app
        .clone()
        .oneshot(json_request("POST", "/api/v1/admin/engine/reload", None))
        .await
        .expect("reload");
    assert_eq!(resp.status(), StatusCode::OK, "{:?}", body_json(resp).await);

    let (after_status, after) = post(&app, "reload-ch", json!({})).await;
    assert_eq!(
        after_status,
        StatusCode::OK,
        "a reload must not empty the secret store: {after}"
    );
    assert_eq!(
        after["data"]["mac"], before["data"]["mac"],
        "and it must not resolve to a different value: {after}"
    );
}

// ---------------------------------------------------------------------------
// secrets — the engine refuses what it cannot keep out of a record
// ---------------------------------------------------------------------------

/// A workflow naming a secret the instance does not declare does not silently
/// read `null`: the engine refuses it, and the channel is quarantined with the
/// reason named rather than served with a missing key.
#[tokio::test]
async fn an_undeclared_secret_quarantines_the_channel() {
    let app = common::test_app().await; // no [secrets] at all
    common::create_and_activate_channel(
        &app,
        "unknown-ch",
        common::workflow_with_tasks(
            "unknown wf",
            json!([{
                "id": "guard", "name": "Guard",
                "condition": { "==": [{ "secret": "nope" }, "x"] },
                "function": { "name": "map", "input": { "mappings": [
                    { "path": "data.ran", "logic": true }
                ] } }
            }]),
        ),
    )
    .await;

    let (status, body) = post(&app, "unknown-ch", json!({})).await;
    assert_ne!(
        status,
        StatusCode::OK,
        "a workflow reading an undeclared secret must not serve: {body}"
    );
}

/// The same refusal from inside a function input rather than a condition. The
/// two travel different paths in dataflow-rs's check — a condition is one
/// expression, a custom task's input is a whole document walked for references
/// — so a misspelled key in the field authors actually type needs its own pin.
#[tokio::test]
async fn an_undeclared_secret_in_a_function_field_quarantines_the_channel() {
    let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
    common::create_and_activate_channel(
        &app,
        "typo-ch",
        // `partner_hmacc` — one letter off a declared name.
        hmac_workflow("typo wf", json!({"secret": "partner_hmacc"})),
    )
    .await;

    let (status, body) = post(&app, "typo-ch", json!({})).await;
    assert_ne!(
        status,
        StatusCode::OK,
        "a misspelled secret must fail at build, not at the remote system: {body}"
    );
}

/// A `map` mapping may not read a secret, because the engine records a
/// mapping's result by construction. dataflow-rs refuses it at build, so the
/// channel never serves rather than writing the key into `data`.
#[tokio::test]
async fn a_mapping_may_not_read_a_secret() {
    let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
    common::create_and_activate_channel(
        &app,
        "copy-ch",
        common::workflow_with_tasks(
            "copy wf",
            json!([{
                "id": "copy", "name": "Copy",
                "function": { "name": "map", "input": { "mappings": [
                    { "path": "data.leaked", "logic": { "secret": "partner_hmac" } }
                ] } }
            }]),
        ),
    )
    .await;

    let (status, body) = post(&app, "copy-ch", json!({})).await;
    assert_ne!(
        status,
        StatusCode::OK,
        "copying a secret into `data` must not be servable: {body}"
    );
    assert!(
        !body.to_string().contains(SECRET_VALUE),
        "the refusal must not quote the value: {body}"
    );
}

/// The refusal is about *recording*, not about `map` — so it covers a value
/// merely derived from a secret, and it covers `log`, which the engine emits
/// rather than stores. Both are documented as refused; both are pinned here.
#[tokio::test]
async fn a_derived_secret_and_a_log_field_are_refused_too() {
    for (channel, tasks) in [
        (
            "derive-ch",
            json!([{
                "id": "derive", "name": "Derive",
                "function": { "name": "map", "input": { "mappings": [
                    // Not a verbatim copy — a prefix of it. There is no static
                    // line between a copy and a derived value, so both go.
                    { "path": "data.hint", "logic": { "cat": [
                        "prefix-", { "secret": "partner_hmac" }
                    ] } }
                ] } }
            }]),
        ),
        (
            "log-ch",
            json!([{
                "id": "shout", "name": "Shout",
                "function": { "name": "log", "input": {
                    "message": "run",
                    "fields": { "key": { "secret": "partner_hmac" } }
                } }
            }]),
        ),
    ] {
        let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
        common::create_and_activate_channel(
            &app,
            channel,
            common::workflow_with_tasks(&format!("{channel} wf"), tasks),
        )
        .await;

        let (status, body) = post(&app, channel, json!({})).await;
        assert_ne!(
            status,
            StatusCode::OK,
            "'{channel}' reads a secret into a recorded expression and must not serve: {body}"
        );
        assert!(
            !body.to_string().contains(SECRET_VALUE),
            "'{channel}': the refusal must not quote the value: {body}"
        );
    }
}

/// The documented limit: `{"secret": …}` is registered by dataflow-rs on the
/// engines *it* builds, so a channel's `validation_logic` — compiled on Orion's
/// own datalogic engine — cannot resolve one. It must fail to compile and
/// quarantine the channel, not pass the reference through as a data object that
/// silently evaluates truthy.
#[tokio::test]
async fn a_secret_in_validation_logic_quarantines_the_channel() {
    let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
    common::create_and_activate_channel_with_config(
        &app,
        "guard-ch",
        common::simple_log_workflow("guard wf"),
        json!({ "validation_logic": { "==": [{ "secret": "gate_value" }, GATE_VALUE] } }),
    )
    .await;

    let (status, body) = post(&app, "guard-ch", json!({ "data": {} })).await;
    assert_ne!(
        status,
        StatusCode::OK,
        "a channel guard that cannot compile must be quarantined, not served: {body}"
    );
    assert!(
        !body.to_string().contains(GATE_VALUE),
        "the refusal must not quote the value: {body}"
    );
}

// ---------------------------------------------------------------------------
// secrets — the leak sweep
// ---------------------------------------------------------------------------

/// Every workflow shape that touches a secret, in one run, so a single sweep
/// covers a condition, an HMAC key and a JWT signing key at once.
fn every_secret_shape_workflow(name: &str) -> Value {
    common::workflow_with_tasks(
        name,
        json!([
            {
                "id": "gate", "name": "Gate",
                "condition": { "==": [{ "secret": "gate_value" }, GATE_VALUE] },
                "function": { "name": "map", "input": { "mappings": [
                    { "path": "data.gated", "logic": true }
                ] } }
            },
            {
                "id": "sign", "name": "Sign",
                "function": { "name": "crypto", "input": {
                    "op": "hmac", "algorithm": "sha256",
                    "key": { "secret": "partner_hmac" },
                    "data": "order-4711",
                    "output": "data.mac"
                } }
            },
            {
                "id": "token", "name": "Token",
                "function": { "name": "jwt_sign", "input": {
                    "algorithm": "HS256",
                    "key": { "secret": "jwt_key" },
                    "claims": { "sub": "user-1" },
                    "expires_in": "5m",
                    "output": "data.token"
                } }
            },
            {
                "id": "capture", "name": "Capture metadata",
                "function": { "name": "map", "input": { "mappings": [
                    { "path": "data.meta", "logic": { "var": "metadata" } }
                ] } }
            }
        ]),
    )
}

/// Assert no declared secret's value appears anywhere in `rendered`, naming
/// which one did and where.
fn assert_no_secret_in(surface: &str, rendered: &str) {
    for value in ALL_SECRET_VALUES {
        assert!(
            !rendered.contains(value),
            "a declared secret reached {surface}"
        );
    }
}

/// **The guarantee, swept.** One run reads three secrets three different ways,
/// with per-step task capture on, and then every surface that could carry a
/// value out is checked — including the trace row *as stored*, which the read
/// path masks and the API therefore cannot prove clean.
#[tokio::test]
async fn a_secret_appears_in_no_recording_surface() {
    let state = common::test_state_with_config(config_with_vars_and_secrets()).await;
    let app = orion::server::build_router(state.clone());

    common::create_and_activate_channel_with_config(
        &app,
        "leak-ch",
        every_secret_shape_workflow("leak wf"),
        json!({ "tracing": { "task_details": true } }),
    )
    .await;

    let (status, body) = post(
        &app,
        "leak-ch",
        json!({ "data": { "payload": "order-4711" } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The run must actually have done the work, or the sweep below is vacuous.
    assert_eq!(body["data"]["gated"], json!(true), "{body}");
    assert!(body["data"]["mac"].as_str().is_some(), "{body}");
    assert!(body["data"]["token"].as_str().is_some(), "{body}");

    assert_no_secret_in("the response body", &body.to_string());

    // The served trace list and detail, including `task_trace_json` — a
    // per-step `Message` clone under default trace options.
    let list = common::wait_for_body(&app, "/api/v1/admin/traces", |b| {
        b["data"]
            .as_array()
            .is_some_and(|a| a.iter().any(|r| r["channel"] == "leak-ch"))
    })
    .await;
    assert_no_secret_in("a served trace listing", &list.to_string());
    let trace_id = list["data"]
        .as_array()
        .and_then(|a| a.iter().find(|r| r["channel"] == "leak-ch"))
        .and_then(|r| r["id"].as_str())
        .expect("a trace row for the run")
        .to_string();

    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/traces/{trace_id}"),
            None,
        ))
        .await
        .expect("trace detail");
    let detail = body_json(resp).await;
    assert!(
        detail["data"]["task_trace_json"]["steps"]
            .as_array()
            .is_some_and(|s| !s.is_empty()),
        "per-step capture must be on, or this proves nothing: {detail}"
    );
    assert_no_secret_in("a served trace detail", &detail.to_string());

    // The row as stored. The read path strips `context.metadata`, so a value
    // sitting on disk would not surface through the API above — this is the
    // only assertion that can see it.
    let row = state
        .repos
        .traces
        .get_by_id(&trace_id)
        .await
        .expect("the stored trace row");
    for (surface, held) in [
        ("the stored input_json", &row.input_json),
        ("the stored result_json", &row.result_json),
        ("the stored task_trace_json", &row.task_trace_json),
        ("the stored error_message", &row.error_message),
    ] {
        assert_no_secret_in(surface, held.as_deref().unwrap_or(""));
    }

    // The audit trail, which records the admin writes that created all this.
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/audit-logs", None))
        .await
        .expect("audit logs");
    assert_no_secret_in("an audit log entry", &body_json(resp).await.to_string());
}

/// **The control for the sweep above.** A sweep that finds nothing proves
/// nothing unless it can find something, so this puts a known value onto the
/// message and asserts every message-derived surface the sweep checks does
/// carry it: the response, the served trace detail with per-step capture, and
/// the row on disk.
///
/// It is written as a `map` mapping on purpose — the shape dataflow-rs refuses
/// for a secret. What the refusal buys is exactly this: a value that reaches a
/// mapping reaches all three of these places, so a secret must never reach one.
#[tokio::test]
async fn the_sweep_finds_a_value_that_does_reach_the_message() {
    const PLANTED: &str = "planted-control-value-3e2f";

    let state = common::test_state_with_config(config_with_vars_and_secrets()).await;
    let app = orion::server::build_router(state.clone());
    common::create_and_activate_channel_with_config(
        &app,
        "control-ch",
        common::workflow_with_tasks(
            "control wf",
            json!([{
                "id": "plant", "name": "Plant",
                "function": { "name": "map", "input": { "mappings": [
                    { "path": "data.planted", "logic": PLANTED }
                ] } }
            }]),
        ),
        json!({ "tracing": { "task_details": true } }),
    )
    .await;

    let (status, body) = post(&app, "control-ch", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.to_string().contains(PLANTED), "the response: {body}");

    let list = common::wait_for_body(&app, "/api/v1/admin/traces", |b| {
        b["data"]
            .as_array()
            .is_some_and(|a| a.iter().any(|r| r["channel"] == "control-ch"))
    })
    .await;
    let trace_id = list["data"]
        .as_array()
        .and_then(|a| a.iter().find(|r| r["channel"] == "control-ch"))
        .and_then(|r| r["id"].as_str())
        .expect("a trace row")
        .to_string();

    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/traces/{trace_id}"),
            None,
        ))
        .await
        .expect("trace detail");
    let detail = body_json(resp).await;
    assert!(
        detail.to_string().contains(PLANTED),
        "the served trace detail — with per-step capture on: {detail}"
    );

    let row = state
        .repos
        .traces
        .get_by_id(&trace_id)
        .await
        .expect("the stored row");
    assert!(
        row.result_json
            .as_deref()
            .is_some_and(|r| r.contains(PLANTED)),
        "the stored result_json"
    );
    assert!(
        row.task_trace_json
            .as_deref()
            .is_some_and(|t| t.contains(PLANTED)),
        "the stored task_trace_json"
    );
}

/// The other control, for the other assertion. A key written as a **literal**
/// never reaches the message — the handler resolves it into a MAC and keeps the
/// bytes to itself — so a trace is *not* where a literal leaks. It leaks into
/// the stored definition, which is exactly what the docs say and exactly what
/// `[secrets]` fixes: connector configs can be encrypted at rest, workflow
/// documents cannot, and every version of the workflow keeps the key.
#[tokio::test]
async fn a_literal_key_leaks_through_the_stored_definition_not_the_trace() {
    const LITERAL: &str = "literal-key-value-3e2f";

    let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
    let (_, wf_id) = common::create_and_activate_channel(
        &app,
        "literal-ch",
        hmac_workflow("literal wf", json!(LITERAL)),
    )
    .await;

    let (status, body) = post(&app, "literal-ch", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        !body.to_string().contains(LITERAL),
        "a key never reaches the message, whichever spelling it was written in: {body}"
    );

    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/workflows/{wf_id}"),
            None,
        ))
        .await
        .expect("admin read");
    let stored = body_json(resp).await;
    assert!(
        stored.to_string().contains(LITERAL),
        "a literal key is stored in clear — the leak `[secrets]` closes: {stored}"
    );
}

/// The stored definition holds the *name*, not the value — which is what makes
/// a package promotable. If the admin read echoed a resolved secret, exporting
/// a workflow would copy production key material into the artifact.
#[tokio::test]
async fn the_stored_definition_holds_the_name_and_not_the_value() {
    let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
    let (_, wf_id) = common::create_and_activate_channel(
        &app,
        "export-ch",
        hmac_workflow("export wf", json!({"secret": "partner_hmac"})),
    )
    .await;

    for uri in [
        format!("/api/v1/admin/workflows/{wf_id}"),
        "/api/v1/admin/workflows/export".to_string(),
    ] {
        let resp = app
            .clone()
            .oneshot(json_request("GET", &uri, None))
            .await
            .expect("admin read");
        let body = body_json(resp).await;
        let rendered = body.to_string();
        assert!(
            rendered.contains("partner_hmac"),
            "{uri} must carry the name: {body}"
        );
        assert_no_secret_in(&uri, &rendered);
    }
}

/// `POST /admin/workflows/{id}/test` returns the **whole** `ExecutionTrace`
/// under default trace options — a full `Message` clone per step, and the one
/// surface that hands a caller more of the run than a persisted trace does.
#[tokio::test]
async fn the_admin_test_endpoint_leaks_no_secret() {
    let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
    let wf_id =
        common::create_and_activate_workflow(&app, every_secret_shape_workflow("test-leak wf"))
            .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/admin/workflows/{wf_id}/test"),
            Some(json!({ "data": { "payload": "order-4711" } })),
        ))
        .await
        .expect("test endpoint");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(
        body["data"]["output"]["mac"].as_str().is_some(),
        "the run must have used the store, or this proves nothing: {body}"
    );
    assert_no_secret_in("the workflow test endpoint's trace", &body.to_string());
}

/// A *failing* run is the surface a leak is likeliest to reach: an error
/// message is assembled from whatever the handler had in hand. `hmac_verify`
/// with a bad signature fails while holding the resolved key.
#[tokio::test]
async fn a_failing_task_names_the_field_and_never_the_value() {
    let state = common::test_state_with_config(config_with_vars_and_secrets()).await;
    let app = orion::server::build_router(state.clone());
    common::create_and_activate_channel(
        &app,
        "fail-ch",
        common::workflow_with_tasks(
            "fail wf",
            json!([{
                "id": "check", "name": "Check",
                "function": { "name": "crypto", "input": {
                    "op": "hmac_verify", "algorithm": "sha256",
                    "key": { "secret": "partner_hmac" },
                    "data": "order-4711",
                    "signature": "0000000000000000000000000000000000000000000000000000000000000000",
                    "output": "data.ok"
                } }
            }]),
        ),
    )
    .await;

    let (_, body) = post(&app, "fail-ch", json!({ "data": {} })).await;
    assert_no_secret_in("a failing run's response", &body.to_string());

    let list = common::wait_for_body(&app, "/api/v1/admin/traces", |b| {
        b["data"]
            .as_array()
            .is_some_and(|a| a.iter().any(|r| r["channel"] == "fail-ch"))
    })
    .await;
    let trace_id = list["data"]
        .as_array()
        .and_then(|a| a.iter().find(|r| r["channel"] == "fail-ch"))
        .and_then(|r| r["id"].as_str())
        .expect("a trace row")
        .to_string();
    let row = state
        .repos
        .traces
        .get_by_id(&trace_id)
        .await
        .expect("the stored row");
    assert_no_secret_in(
        "a failing run's stored error_message",
        row.error_message.as_deref().unwrap_or(""),
    );
    assert_no_secret_in(
        "a failing run's stored result_json",
        row.result_json.as_deref().unwrap_or(""),
    );
}

/// A store *miss* at execution — reachable only for a dynamic name, which the
/// build-time check cannot resolve — names the key and never a value, and the
/// message must not disclose which other names exist.
#[test]
fn the_store_never_yields_a_value_to_debug_or_display() {
    let mut map = serde_json::Map::new();
    map.insert("partner_hmac".into(), json!(SECRET_VALUE));
    let store = orion::engine::ResolvedSecrets::from_values(map);

    let rendered = format!("{store:?}");
    assert!(rendered.contains("partner_hmac"), "{rendered}");
    assert_no_secret_in("the store's Debug output", &rendered);
    assert_eq!(store.names().collect::<Vec<_>>(), vec!["partner_hmac"]);
}

// ---------------------------------------------------------------------------
// authoring-time: a reference in a field that resolves none
// ---------------------------------------------------------------------------

/// A reference in a field nothing resolves is refused at create, not accepted
/// and then requested as a URL spelled `env://API_BASE`.
#[tokio::test]
async fn a_stray_reference_is_refused_at_create() {
    let app = common::test_app().await;
    let (status, body) = create_workflow(
        &app,
        common::workflow_with_tasks(
            "stray wf",
            json!([{
                "id": "call", "name": "Call",
                "function": { "name": "http_call", "input": {
                    "connector": "crm", "path": "env://API_BASE"
                } }
            }]),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let details = body["error"]["details"].to_string();
    assert!(details.contains("UNRESOLVED_SECRET_REF"), "{body}");
    assert!(
        details.contains("tasks[0].function.input.path"),
        "the refusal must name the field: {body}"
    );
}

/// The same document, through `POST /workflows/validate` — the endpoint CI
/// calls before it ever writes, so it must agree with create.
#[tokio::test]
async fn validate_reports_the_same_stray_reference() {
    let app = common::test_app().await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/validate",
            Some(common::workflow_with_tasks(
                "stray wf",
                json!([{
                    "id": "call", "name": "Call",
                    "function": { "name": "http_call", "input": {
                        "connector": "crm", "path": "env://API_BASE"
                    } }
                }]),
            )),
        ))
        .await
        .expect("validate");
    let body = body_json(resp).await;
    assert_eq!(body["data"]["valid"], json!(false), "{body}");
    let errors = body["data"]["errors"].to_string();
    assert!(
        errors.contains("tasks[0].function.input.path"),
        "validate must name the same field create refuses: {body}"
    );
    assert!(
        errors.contains("env://API_BASE"),
        "and quote the reference it found: {body}"
    );
}

/// An update carries the same check — otherwise the refusal is one PATCH away
/// from being bypassed.
#[tokio::test]
async fn a_stray_reference_is_refused_on_update() {
    let app = common::test_app().await;
    let (status, created) = create_workflow(&app, common::simple_log_workflow("update wf")).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let wf_id = created["data"]["workflow_id"].as_str().unwrap();

    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/workflows/{wf_id}"),
            Some(json!({ "tasks": [{
                "id": "call", "name": "Call",
                "function": { "name": "send_email", "input": {
                    "connector": "smtp", "to": "ops@example.com",
                    "subject": "hi", "text": "vault://secret/data/x#y"
                } }
            }] })),
        ))
        .await
        .expect("update");
    let status = resp.status();
    let body = body_json(resp).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["details"]
            .to_string()
            .contains("UNRESOLVED_SECRET_REF"),
        "{body}"
    );
}

/// And the five fields that *do* resolve one stay accepted — the check is a
/// field allowlist, not a ban on the `env://` characters.
#[tokio::test]
async fn a_reference_in_a_secret_bearing_field_is_still_accepted() {
    let app = common::test_app().await;
    let (status, body) = create_workflow(
        &app,
        hmac_workflow("legacy wf", json!(format!("env://{SECRET_ENV}"))),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "crypto.key resolves a reference and must keep working: {body}"
    );
}

/// The schema type-check has to admit the new spelling too: `crypto.key` is
/// declared a string, and `{"secret": …}` is an object. Without the `secret`
/// flag being consulted, the store form would be rejected as a type error.
#[tokio::test]
async fn a_secret_node_satisfies_a_string_typed_secret_field() {
    let app = common::test_app().await;
    let (status, body) =
        create_workflow(&app, hmac_workflow("node wf", json!({"secret": "k"}))).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a {{\"secret\": …}} node must satisfy a string-typed secret field: {body}"
    );
}

// ---------------------------------------------------------------------------
// offline: dry-run and the case runner
// ---------------------------------------------------------------------------

fn write_scratch(dir: &std::path::Path, name: &str, content: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, content).expect("write fixture");
    path.to_string_lossy().into_owned()
}

fn signing_workflow_json() -> &'static str {
    r#"{
        "name": "offline signer",
        "condition": true,
        "tasks": [{
            "id": "sign", "name": "Sign",
            "function": {"name": "crypto", "input": {
                "op": "hmac", "algorithm": "sha256",
                "key": {"secret": "partner_hmac"},
                "data": "order-4711",
                "output": "data.mac"
            }}
        }]
    }"#
}

/// `dry-run --secrets` supplies the stand-ins an offline run has no config to
/// resolve. Without it the engine has no store and refuses the workflow, so a
/// workflow that signs anything would be untestable.
#[test]
fn dry_run_takes_stand_in_secrets() {
    let scratch = common::ScratchDir::new("dry-run-secrets");
    let dir = scratch.path();
    let wf = write_scratch(dir, "wf.json", signing_workflow_json());
    let input = write_scratch(dir, "input.json", r#"{"id":"ORD-1"}"#);
    let secrets = write_scratch(dir, "secrets.json", r#"{"partner_hmac":"stand-in-key"}"#);

    let out = std::process::Command::new(common::orion_bin())
        .args(["dry-run", "-w", &wf, "-i", &input, "--secrets", &secrets])
        .output()
        .expect("invoke dry-run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout={stdout}\nstderr={stderr}");
    assert!(
        stdout.contains("\"mac\""),
        "the signing task must have run: {stdout}"
    );
    assert!(
        !stdout.contains("stand-in-key"),
        "even a stand-in must not be echoed into the printed run: {stdout}"
    );
}

/// And without the flag the run refuses rather than signing with `null` — the
/// same "an engine with no store refuses a workflow that names one" rule the
/// serving path applies.
#[test]
fn dry_run_without_secrets_refuses_a_workflow_that_names_one() {
    let scratch = common::ScratchDir::new("dry-run-no-secrets");
    let dir = scratch.path();
    let wf = write_scratch(dir, "wf.json", signing_workflow_json());
    let input = write_scratch(dir, "input.json", r#"{"id":"ORD-1"}"#);

    let out = std::process::Command::new(common::orion_bin())
        .args(["dry-run", "-w", &wf, "-i", &input])
        .output()
        .expect("invoke dry-run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "an undeclared secret must not run offline either: stdout={stdout}"
    );
    assert!(
        stderr.contains("partner_hmac"),
        "the refusal must name the key: stderr={stderr}"
    );
}

/// A `*.case.json` carries the same block, so a regression suite can cover a
/// workflow that signs. The case also pins `metadata.vars`, which offline is
/// supplied rather than stamped — there is no config file to stamp it from.
#[test]
fn a_case_file_takes_secrets_and_vars() {
    let scratch = common::ScratchDir::new("case-secrets");
    let dir = scratch.path();
    write_scratch(
        dir,
        "wf.json",
        r#"{
            "name": "offline signer",
            "condition": true,
            "tasks": [
                {
                    "id": "sign", "name": "Sign",
                    "function": {"name": "crypto", "input": {
                        "op": "hmac", "algorithm": "sha256",
                        "key": {"secret": "partner_hmac"},
                        "data": "order-4711",
                        "output": "data.mac"
                    }}
                },
                {
                    "id": "topic", "name": "Topic",
                    "function": {"name": "map", "input": {"mappings": [
                        {"path": "data.topic", "logic": {"var": "metadata.vars.topic_prefix"}}
                    ]}}
                }
            ]
        }"#,
    );
    write_scratch(
        dir,
        "ok.case.json",
        r#"{
            "name": "signs and reads a var",
            "workflow": "wf.json",
            "input": {"id": "ORD-1"},
            "secrets": {"partner_hmac": "stand-in-key"},
            "metadata": {"vars": {"topic_prefix": "eu-west"}},
            "expect": {"data.topic": "eu-west"}
        }"#,
    );

    let out = std::process::Command::new(common::orion_bin())
        .args(["test", dir.to_str().unwrap()])
        .output()
        .expect("run test");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "suite failed: {stdout}");
    assert!(stdout.contains("1 passed, 0 failed"), "{stdout}");
    assert!(
        !stdout.contains("stand-in-key"),
        "the runner must not echo a stand-in value: {stdout}"
    );
}

/// A case's `metadata.vars` is shape-checked, because at runtime the key is
/// force-stamped from one object — a case writing a string there would be
/// asserting on state no ingress can produce.
#[test]
fn an_offline_case_refuses_a_non_object_vars() {
    let err = orion::engine::utils::prepare_offline_metadata(json!({ "vars": "eu-west" }))
        .expect_err("a string `vars` is not a shape any ingress builds");
    assert!(err.contains("metadata.vars"), "{err}");
    assert!(err.contains("object"), "{err}");

    orion::engine::utils::prepare_offline_metadata(json!({ "vars": { "topic_prefix": "eu" } }))
        .expect("an object passes through");
}

/// The regression the stamping helper's own contract implies: a caller that
/// omits `metadata` entirely still gets the instance's vars.
///
/// `TestWorkflowRequest.metadata` is `#[serde(default)]`, so an omitted field
/// arrives as `Value::Null`. The data route normalises before stamping and
/// this one does not, so a helper that skipped a non-object made the endpoint
/// answer differently from the request it stands in for — which is the one
/// thing it must not do.
#[tokio::test]
async fn the_admin_test_endpoint_stamps_vars_when_metadata_is_omitted() {
    let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
    let wf_id = common::create_and_activate_workflow(&app, metadata_echo_workflow("omitted wf"))
        .await
        .to_string();

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/admin/workflows/{wf_id}/test"),
            Some(json!({ "data": {} })),
        ))
        .await
        .expect("test endpoint");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["data"]["output"]["meta"]["vars"]["topic_prefix"],
        json!("eu-west"),
        "an omitted metadata must still be stamped: {body}"
    );
}

/// The stamp is total over the shapes an ingress can hand it, so a fifth
/// ingress cannot forget to normalise first.
#[test]
fn the_stamp_replaces_a_non_object_metadata_rather_than_skipping_it() {
    let declared = json!({ "topic_prefix": "eu-west" });

    let mut null = json!(null);
    orion::engine::stamp_vars(&mut null, Some(&declared));
    assert_eq!(null["vars"]["topic_prefix"], json!("eu-west"), "{null}");

    // With nothing declared there is nothing to stamp, so a non-object is left
    // exactly as it was — the key is absent either way.
    let mut still_null = json!(null);
    orion::engine::stamp_vars(&mut still_null, None);
    assert_eq!(still_null, json!(null));
}

/// `{"secret": ["name"]}` is the one-element-array argument datalogic
/// normalises to the string form, so the engine resolves it. Every Orion
/// surface that reads a secret node has to agree, or the same node resolves in
/// a condition, fails in a handler field, and is missing from `lint`'s
/// `[secrets]` deployment inventory.
#[tokio::test]
async fn the_array_spelling_of_a_secret_reads_the_same_store() {
    let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
    common::create_and_activate_channel(
        &app,
        "sign-string-arg",
        hmac_workflow("string arg wf", json!({"secret": "partner_hmac"})),
    )
    .await;
    common::create_and_activate_channel(
        &app,
        "sign-array-arg",
        hmac_workflow("array arg wf", json!({"secret": ["partner_hmac"]})),
    )
    .await;

    let payload = json!({"data": {"payload": "order-4711"}});
    let (string_status, string_body) = post(&app, "sign-string-arg", payload.clone()).await;
    let (array_status, array_body) = post(&app, "sign-array-arg", payload).await;
    assert_eq!(string_status, StatusCode::OK, "{string_body}");
    assert_eq!(
        array_status,
        StatusCode::OK,
        "the array spelling must not quarantine the channel: {array_body}"
    );
    assert!(
        !array_body["data"]["mac"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "no MAC was produced: {array_body}"
    );
    assert_eq!(
        string_body["data"]["mac"], array_body["data"]["mac"],
        "both spellings name the same secret, so both must resolve to it"
    );
}

/// The same node, seen by `lint`'s deployment inventory. A set that names a
/// secret the target instance must declare has to be listed whichever spelling
/// it used, or the checklist is short by exactly the entry that quarantines a
/// channel on deploy.
#[test]
fn lint_inventories_both_spellings_of_a_secret_reference() {
    for spelling in [
        json!({"secret": "partner_hmac"}),
        json!({"secret": ["partner_hmac"]}),
    ] {
        let scratch = common::ScratchDir::new("lint-secret-spelling");
        write_scratch(
            scratch.path(),
            "wf.json",
            &hmac_workflow("lint wf", spelling.clone()).to_string(),
        );
        let out = std::process::Command::new(common::orion_bin())
            .args(["lint", scratch.path().to_str().expect("utf-8 path")])
            .output()
            .expect("invoke lint");
        // The inventory is a note, and notes go to stderr; the summary line is
        // what lands on stdout.
        let rendered = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            rendered.contains("[secrets.reference]") && rendered.contains("partner_hmac"),
            "{spelling} must appear in the [secrets] inventory: {rendered}"
        );
    }
}

/// `jwt_verify.keys` is an array and stays one. Marking the field as
/// key-material-bearing must not let a bare `{"secret": …}` stand in for the
/// array itself: the handler reads `keys` with `as_array()`, so it would find
/// no static key and verify against JWKS alone — silently, and only for tokens
/// the operator meant the static key to cover.
#[tokio::test]
async fn a_secret_node_cannot_stand_in_for_the_keys_array() {
    let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
    let (status, body) = create_workflow(
        &app,
        json!({
            "id": "keys-shape", "name": "keys shape", "priority": 0, "condition": true,
            "tasks": [{
                "id": "verify", "name": "Verify",
                "function": {"name": "jwt_verify", "input": {
                    "token": {"var": "data.token"},
                    "algorithms": ["HS256"],
                    "keys": {"secret": "partner_hmac"},
                    "jwks_url": "https://idp.example.com/.well-known/jwks.json"
                }}
            }]
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a secret node where an array belongs must be refused: {body}"
    );
    assert!(
        body.to_string().contains("keys"),
        "the refusal must name the field: {body}"
    );
}

/// An `issuer`/`audience` array is `resolvable`, so a `{"var": …}` element the
/// request does not carry folds to `null`. That is an absent accepted value,
/// not a malformed one: dropping it can only narrow what verification accepts,
/// so it cannot turn a rejected token into an accepted one. Refusing the whole
/// task instead would break workflows that predate the store.
#[tokio::test]
async fn an_absent_audience_element_is_dropped_rather_than_failing_the_task() {
    let app = common::test_app_with_config(config_with_vars_and_secrets()).await;
    let wf_id = common::create_and_activate_workflow(
        &app,
        json!({
            "id": "aud-partial", "name": "aud partial", "priority": 0, "condition": true,
            "tasks": [{
                "id": "verify", "name": "Verify",
                "function": {"name": "jwt_verify", "input": {
                    "token": {"var": "data.token"},
                    "algorithms": ["HS256"],
                    "keys": [{"algorithm": "HS256", "key": {"secret": "partner_hmac"}}],
                    // The second element is absent from every request below.
                    "audience": [{"var": "data.aud1"}, {"var": "data.aud2"}],
                    "output": "data.claims"
                }}
            }]
        }),
    )
    .await
    .to_string();

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/admin/workflows/{wf_id}/test"),
            Some(json!({ "data": { "token": "not-a-jwt", "aud1": "orders-api" } })),
        ))
        .await
        .expect("test endpoint");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let rendered = body.to_string();
    // The token is junk, so the task fails — but on the token, not on the
    // shape of `audience`. The old code dropped the null; the intervening
    // change turned it into "'audience' must be a string ...".
    assert!(
        !rendered.contains("'audience' must be a string"),
        "an absent element must be dropped, not refused: {rendered}"
    );
}

/// A `--secrets` / case-file store is the only one that can hold a non-string,
/// so its shape is checked where the file is named rather than at the first
/// task that reads one — a case-file run reports a task error under the case
/// name and nothing else.
#[test]
fn an_offline_secret_must_be_a_string() {
    let scratch = common::ScratchDir::new("offline-secret-kind");
    let dir = scratch.path();
    let wf = write_scratch(dir, "wf.json", signing_workflow_json());
    let input = write_scratch(dir, "input.json", r#"{"id":"ORD-1"}"#);
    let secrets = write_scratch(dir, "secrets.json", r#"{"partner_hmac": 12345}"#);

    let out = std::process::Command::new(common::orion_bin())
        .args(["dry-run", "-w", &wf, "-i", &input, "--secrets", &secrets])
        .output()
        .expect("invoke dry-run");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "a non-string stand-in must fail at load: {combined}"
    );
    assert!(
        combined.contains("partner_hmac") && combined.contains("string"),
        "the failure must name the entry and the kind it needed: {combined}"
    );
}
