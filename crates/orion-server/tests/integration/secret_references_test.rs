//! B5 (Tier-2 ergonomics): `env://` secret references in deployed
//! connections.
//!
//! Verifies that:
//!   - a connection authored with `"token": "env://VAR_NAME"` is stored as
//!     the reference and serves with the resolved env-var value (the engine
//!     sees the secret, the deployment store stays clean)
//!   - plain strings (https://..., postgres://...) pass through unchanged

use crate::common::{self, body_json, json_request, test_app};
use axum::http::{HeaderMap, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Set before `main()`, while the process is still single-threaded — the only
/// context where `set_var` cannot race a concurrent `getenv` from another test's
/// runtime threads (which is why `std::env::set_var` is `unsafe` in Rust 2024).
/// The var is uniquely named, read-only thereafter, and lives for the process
/// lifetime; no cleanup is needed or attempted.
///
/// It sits in the reserved `ORION_SECRET_*` namespace (C4d) because it is set
/// for the whole test binary, including the tests that call `load_config` —
/// and every other `ORION_*` name is now refused at startup as a misspelled
/// override. That is exactly the convention an operator follows for an
/// `env://` secret they want to keep in the ORION namespace.
/// `ctor(unsafe)` rather than a bare `ctor`: 1.0 made the marker mandatory
/// because a pre-main constructor runs before the Rust runtime is initialised,
/// so the obligation was always there — 0.4 just did not make you write it.
/// It is the same obligation the SAFETY note below already discharges.
#[ctor::ctor(unsafe)]
#[allow(unsafe_code)]
fn install_b5_env_fixture() {
    // SAFETY: runs pre-main on the sole thread of the process; no concurrent
    // reader of the environment can exist yet.
    unsafe {
        std::env::set_var("ORION_SECRET_B5_TEST_TOKEN", "resolved-secret-value");
    }
}

/// The connection revisions `GET /connections` lists under `id`.
async fn connection_rows(app: &axum::Router, id: &str) -> Vec<Value> {
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v2/admin/connections", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await["connections"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["connection_id"] == id)
        .cloned()
        .collect()
}

/// `/health`'s `failed_to_load` list.
async fn load_failures(app: &axum::Router) -> Vec<Value> {
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/health", None))
        .await
        .unwrap();
    body_json(resp).await["connectors"]["failed_to_load"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// A server that echoes the `Authorization` header it received.
async fn start_header_echo() -> std::net::SocketAddr {
    let mock = axum::Router::new().route(
        "/api",
        axum::routing::post(|headers: HeaderMap| async move {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            axum::Json(json!({ "authorization": auth }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, mock).await.unwrap();
    });
    addr
}

/// The connection stores the `env://` reference, never the value, and the
/// generation built from it resolves the reference: a request through it
/// carries the resolved token.
#[tokio::test]
async fn connector_with_env_reference_resolves_at_engine_load() {
    let app = test_app().await;
    let addr = start_header_echo().await;

    common::create_connector(
        &app,
        json!({
            "name": "secret-http",
            "connector_type": "http",
            "config": {
                "url": format!("http://{addr}"),
                "method": "POST",
                "allow_private_urls": true,
                "auth": { "type": "bearer", "token": "env://ORION_SECRET_B5_TEST_TOKEN" }
            }
        }),
    )
    .await;

    // The stored revision holds the reference, not what it resolves to.
    let rows = connection_rows(&app, "secret-http").await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let stored = rows[0].to_string();
    assert!(
        !stored.contains("resolved-secret-value"),
        "the resolved value must not reach the deployment store: {stored}"
    );
    assert!(
        stored.contains("env://ORION_SECRET_B5_TEST_TOKEN"),
        "the reference is what is stored: {stored}"
    );

    // The registry resolved it at load: no load issue, and the call carries
    // the resolved token.
    assert!(load_failures(&app).await.is_empty());
    common::create_and_activate_channel(
        &app,
        "b5-secret",
        common::workflow_with_tasks(
            "b5",
            json!([{
                "id": "call", "name": "Call",
                "function": { "name": "http_call", "input": {
                    "connector": "secret-http",
                    "method": "POST",
                    "path": "/api",
                    "body": {},
                    "output": "data.echo"
                }}
            }]),
        ),
    )
    .await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/b5-secret",
            Some(json!({ "data": {} })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["data"]["echo"]["authorization"], "Bearer resolved-secret-value",
        "{body}"
    );
}

#[tokio::test]
async fn plain_url_passes_through_unchanged() {
    let app = test_app().await;
    // postgres://... is a real connection string, not a secret reference:
    // it stays in the public config as written and is no locator.
    common::create_connector(
        &app,
        json!({
            "name": "plain-db",
            "connector_type": "db",
            "config": {
                "connection_string": "postgres://localhost/example",
            }
        }),
    )
    .await;
    let rows = connection_rows(&app, "plain-db").await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0]["public_config"]["connection_string"], "postgres://localhost/example",
        "{}",
        rows[0]
    );
    assert!(
        rows[0]["secret_locators"]
            .as_object()
            .is_none_or(|m| m.is_empty()),
        "{}",
        rows[0]
    );
    assert!(load_failures(&app).await.is_empty());
}
