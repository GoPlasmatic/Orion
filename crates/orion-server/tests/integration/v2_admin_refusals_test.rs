//! The v2 admin API's refusals, over HTTP: what a deployment or connection
//! request is turned away for, and with which status. The happy path —
//! upload, admit, connect, plan, apply, converge, roll back — is
//! `cli_and_server_contract_end_to_end_against_running_server`.

use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common::{body_json, json_request, test_app};

const PACKAGES: &str = "/api/v2/admin/connector-packages";
const CONNECTIONS: &str = "/api/v2/admin/connections/crm/revisions";
const DEPLOYMENTS: &str = "/api/v2/admin/deployments";

async fn send(app: &Router, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(json_request(method, uri, Some(body)))
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

/// The fixture's helper package as an upload body, admitted on upload.
fn helper_package_upload() -> Value {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/v2/package-identity/bundle.json");
    let bundle: Value = serde_json::from_str(&std::fs::read_to_string(dir).unwrap()).unwrap();
    let pkg = &bundle["providers"][1];
    let mut files = pkg["payloads"].as_object().unwrap().clone();
    let inventory = orion::test_support::canonical_bytes(&pkg["inventory"]).unwrap();
    files.insert(
        "inventory.json".to_string(),
        Value::String(String::from_utf8(inventory).unwrap()),
    );
    json!({"files": files, "admit": true})
}

/// A deployed package and a manifest declaring it, with no members.
async fn deployed_package(app: &Router) -> (String, Value) {
    let (status, uploaded) = send(app, "POST", PACKAGES, helper_package_upload()).await;
    assert_eq!(status, StatusCode::CREATED, "{uploaded}");
    let digest = uploaded["package"]["digest"].as_str().unwrap().to_string();
    let pkg = &uploaded["package"];
    let manifest = json!({
        "schema_version": 2,
        "providers": [{
            "publisher": pkg["publisher"],
            "provider_id": pkg["provider_id"],
            "version": pkg["version"],
            "digest": digest,
        }],
        "members": []
    });
    (digest, manifest)
}

async fn apply(app: &Router, manifest: &Value, base: u64, key: &str) -> Value {
    crate::common::deploy::apply_body(app, base, manifest.clone(), key).await
}

/// The same idempotency key replays the receipt it committed; a stale
/// base revision is a conflict, not a second commit.
#[tokio::test]
async fn a_replay_answers_its_receipt_and_a_stale_base_conflicts() {
    let app = test_app().await;
    let (_, manifest) = deployed_package(&app).await;

    let (status, first) = send(
        &app,
        "POST",
        DEPLOYMENTS,
        apply(&app, &manifest, 0, "k1").await,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    assert_eq!(first["committed_revision"], 1);

    let (status, replay) = send(
        &app,
        "POST",
        DEPLOYMENTS,
        apply(&app, &manifest, 0, "k1").await,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{replay}");
    assert_eq!(
        replay["committed_revision"], 1,
        "a replay commits nothing new"
    );

    let (status, stale) = send(
        &app,
        "POST",
        DEPLOYMENTS,
        apply(&app, &manifest, 0, "k2").await,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{stale}");
}

/// A revoked provider package cannot be deployed.
#[tokio::test]
async fn a_revoked_provider_is_not_deployed() {
    let app = test_app().await;
    let (digest, manifest) = deployed_package(&app).await;
    let (status, revoked) = send(
        &app,
        "POST",
        &format!("{PACKAGES}/{digest}/revocations"),
        json!({"reason": "compromised"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{revoked}");

    let (status, body) = send(
        &app,
        "POST",
        DEPLOYMENTS,
        apply(&app, &manifest, 0, "k1").await,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "PROVIDER_REVOKED", "{body}");
    let (_, listed) = send(&app, "GET", DEPLOYMENTS, Value::Null).await;
    assert_eq!(
        listed["head_revision"], 0,
        "nothing was committed: {listed}"
    );
}

/// A credential in a connection's public configuration is refused; it
/// belongs behind the revision's secret locator.
#[tokio::test]
async fn a_secret_in_public_config_is_refused() {
    let app = test_app().await;
    let (digest, _) = deployed_package(&app).await;
    for public_config in [
        json!({"url": "https://crm.example.com", "api_key": "sk_live"}),
        json!({"connection_string": "postgres://user:hunter2@db/app"}),
    ] {
        let (status, body) = send(
            &app,
            "POST",
            CONNECTIONS,
            json!({
                "revision": 1,
                "provider_digest": digest,
                "public_config": public_config,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(
            body["error"]["code"], "CONNECTION_PUBLIC_SECRET_FORBIDDEN",
            "{body}"
        );
    }
}

/// A secret locator is a reference, never the secret: a literal is refused,
/// as is the single `secret_locator` field the map replaced.
#[tokio::test]
async fn a_secret_locator_must_be_a_reference() {
    let app = test_app().await;
    let (digest, _) = deployed_package(&app).await;
    let (status, body) = send(
        &app,
        "POST",
        CONNECTIONS,
        json!({
            "revision": 1,
            "provider_digest": digest,
            "public_config": {"url": "https://crm.example.com", "auth": {"type": "bearer"}},
            "secret_locators": {"auth.token": "sk_live_inline"},
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["error"]["code"], "CONNECTION_SECRET_LOCATOR_INVALID",
        "{body}"
    );

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            CONNECTIONS,
            Some(json!({
                "revision": 1,
                "provider_digest": digest,
                "public_config": {"url": "https://crm.example.com"},
                "secret_locator": "env://CRM_TOKEN",
            })),
        ))
        .await
        .unwrap();
    assert!(
        resp.status().is_client_error(),
        "the single-locator field is unknown: {}",
        resp.status()
    );
}
