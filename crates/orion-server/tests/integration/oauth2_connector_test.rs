//! #268: managed OAuth2 connector auth, end to end — a workflow's `http_call`
//! through a deployed oauth2 connection against an in-process fake IdP + fake
//! API. No containers: both backends are axum listeners, the same pattern as
//! the fake Vault.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use crate::common;

/// The client secrets the deployed connectors reference through `env://` —
/// a connection holds references only — set before `main()` while the
/// process is single-threaded, in the reserved `ORION_SECRET_*` namespace.
#[ctor::ctor(unsafe)]
#[allow(unsafe_code)]
fn install_oauth2_connector_env() {
    // SAFETY: runs pre-main on the sole thread of the process.
    unsafe {
        std::env::set_var("ORION_SECRET_TEST_OAUTH2_CLIENT", "cs-secret");
        std::env::set_var("ORION_SECRET_TEST_ZOOM_CLIENT", "zoom-secret");
    }
}

/// `connector` with its client secret moved to the `env://` reference a
/// deployed connection carries instead of the value.
fn by_reference(mut connector: serde_json::Value, var: &str) -> serde_json::Value {
    connector["config"]["auth"]["client_secret"] = json!(format!("env://{var}"));
    connector
}

/// The fake IdP: counts token requests, requires the expected client Basic
/// auth, and issues `tok-<n>`.
struct Idp {
    hits: AtomicU64,
}

async fn start_idp(idp: Arc<Idp>) -> String {
    async fn token(
        State(idp): State<Arc<Idp>>,
        headers: HeaderMap,
        body: String,
    ) -> (StatusCode, axum::Json<serde_json::Value>) {
        let n = idp.hits.fetch_add(1, Ordering::SeqCst) + 1;
        let ok_auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("Basic "));
        // Both static-credential grants are accepted, and each must carry the
        // parameters that make it addressable — so a request missing the Zoom
        // `account_id` is refused here rather than quietly succeeding.
        let ok_grant = body.contains("grant_type=client_credentials")
            || (body.contains("grant_type=account_credentials")
                && body.contains("account_id=zoom-acct-1"));
        if !(ok_auth && ok_grant) {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({ "error": "invalid_client" })),
            );
        }
        (
            StatusCode::OK,
            axum::Json(json!({
                "access_token": format!("tok-{n}"),
                "token_type": "Bearer",
                "expires_in": 3600
            })),
        )
    }
    let app = axum::Router::new()
        .route("/oauth2/token", axum::routing::post(token))
        .with_state(idp);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind idp");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve idp");
    });
    format!("http://{addr}/oauth2/token")
}

/// The fake partner API: 401 without a `tok-*` Bearer, JSON with one.
async fn start_api() -> String {
    async fn orders(headers: HeaderMap) -> (StatusCode, axum::Json<serde_json::Value>) {
        let authed = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("Bearer tok-"));
        if !authed {
            return (
                StatusCode::UNAUTHORIZED,
                axum::Json(json!({ "error": "missing or stale token" })),
            );
        }
        (
            StatusCode::OK,
            axum::Json(json!({ "orders": [ { "id": 7 } ] })),
        )
    }
    let app = axum::Router::new().route("/", axum::routing::get(orders));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind api");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve api");
    });
    format!("http://{addr}")
}

fn oauth2_connector(name: &str, api_url: &str, token_url: &str) -> serde_json::Value {
    json!({
        "id": name,
        "name": name,
        "connector_type": "http",
        "config": {
            "type": "http",
            "url": api_url,
            "method": "GET",
            // Both fakes listen on localhost.
            "allow_private_urls": true,
            "auth": {
                "type": "oauth2",
                "grant": "client_credentials",
                "token_url": token_url,
                "client_id": "svc-app",
                "client_secret": "cs-secret",
                "scopes": ["orders.read"]
            }
        }
    })
}

/// #273: the same connector shape for Zoom Server-to-Server OAuth. The fake
/// IdP refuses the request unless it carries both
/// `grant_type=account_credentials` and the account id, so reaching the API at
/// all proves the wire shape.
fn zoom_connector(name: &str, api_url: &str, token_url: &str) -> serde_json::Value {
    json!({
        "id": name,
        "name": name,
        "connector_type": "http",
        "config": {
            "type": "http",
            "url": api_url,
            "method": "GET",
            "allow_private_urls": true,
            "auth": {
                "type": "oauth2",
                "grant": "account_credentials",
                "token_url": token_url,
                "client_id": "zoom-client",
                "client_secret": "zoom-secret",
                "account_id": "zoom-acct-1"
            }
        }
    })
}

/// #273: end to end over the grant Zoom moved every server-side integration
/// to. No workaround existed — `http_call` headers are static, there is no
/// `headers_logic` in either tree, and `apply_auth`'s OAuth2 arm deliberately
/// sends nothing, so a workflow could fetch a Zoom token but never use it.
#[tokio::test]
async fn an_account_credentials_connector_acquires_and_applies_its_token() {
    let app = common::test_app().await;
    let idp = Arc::new(Idp {
        hits: AtomicU64::new(0),
    });
    let token_url = start_idp(Arc::clone(&idp)).await;
    let api_url = start_api().await;

    common::create_connector(
        &app,
        by_reference(
            zoom_connector("zoom", &api_url, &token_url),
            "ORION_SECRET_TEST_ZOOM_CLIENT",
        ),
    )
    .await;
    common::create_and_activate_channel(
        &app,
        "zoom-ch",
        common::workflow_with_tasks(
            "FetchMeetings",
            json!([{
                "id": "t1", "name": "Fetch", "function": { "name": "http_call", "input": {
                    "connector": "zoom",
                    "method": "GET",
                    "output": "data.meetings"
                } }
            }]),
        ),
    )
    .await;

    for _ in 0..3 {
        let resp = app
            .clone()
            .oneshot(common::json_request(
                "POST",
                "/api/v2/data/zoom-ch",
                Some(json!({"data": {}})),
            ))
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = common::body_json(resp).await;
        assert_eq!(
            body["data"]["meetings"]["orders"][0]["id"], 7,
            "the API answered through the managed token: {body}"
        );
    }
    assert_eq!(
        idp.hits.load(Ordering::SeqCst),
        1,
        "the grant inherits caching for free — three calls, one acquisition"
    );
}

/// The headline path: a workflow calls a partner API through an oauth2
/// connector; Orion acquires the token, applies it, and one acquisition
/// serves every subsequent request.
#[tokio::test]
async fn http_call_acquires_applies_and_caches_the_token() {
    let app = common::test_app().await;
    let idp = Arc::new(Idp {
        hits: AtomicU64::new(0),
    });
    let token_url = start_idp(Arc::clone(&idp)).await;
    let api_url = start_api().await;

    common::create_connector(
        &app,
        by_reference(
            oauth2_connector("partner", &api_url, &token_url),
            "ORION_SECRET_TEST_OAUTH2_CLIENT",
        ),
    )
    .await;
    common::create_and_activate_channel(
        &app,
        "orders-ch",
        common::workflow_with_tasks(
            "FetchOrders",
            json!([{
                "id": "t1", "name": "Fetch", "function": { "name": "http_call", "input": {
                    "connector": "partner",
                    "method": "GET",
                    "output": "data.orders_response"
                } }
            }]),
        ),
    )
    .await;

    for _ in 0..3 {
        let resp = app
            .clone()
            .oneshot(common::json_request(
                "POST",
                "/api/v2/data/orders-ch",
                Some(json!({"data": {}})),
            ))
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = common::body_json(resp).await;
        assert_eq!(
            body["data"]["orders_response"]["orders"][0]["id"], 7,
            "the API answered through the managed token: {body}"
        );
    }
    assert_eq!(
        idp.hits.load(Ordering::SeqCst),
        1,
        "three calls, one token acquisition"
    );
}
