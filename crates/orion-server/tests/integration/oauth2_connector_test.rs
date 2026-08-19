//! #268: managed OAuth2 connector auth, end to end — a workflow's `http_call`
//! through an oauth2 connector against an in-process fake IdP + fake API, the
//! probe endpoint acquiring a real token, and the admin read masking the
//! credential halves. No containers: both backends are axum listeners, the
//! same pattern as the fake Vault and the in-process SMTP exchange.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use crate::common;

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
        let ok_grant = body.contains("grant_type=client_credentials");
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

    common::create_connector(&app, oauth2_connector("partner", &api_url, &token_url)).await;
    common::create_and_activate_channel(
        &app,
        "orders-ch",
        common::workflow_with_tasks(
            "FetchOrders",
            json!([{
                "id": "t1", "name": "Fetch", "function": { "name": "http_call", "input": {
                    "connector": "partner",
                    "method": "GET",
                    "response_path": "data.orders_response"
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
                "/api/v1/data/orders-ch",
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

/// `POST /connectors/{id}/test` acquires a real token — the probe validates
/// the whole OAuth setup before any workflow depends on it, and a rejecting
/// IdP therefore fails the probe.
#[tokio::test]
async fn the_probe_exercises_the_whole_oauth_setup() {
    let app = common::test_app().await;
    let idp = Arc::new(Idp {
        hits: AtomicU64::new(0),
    });
    let token_url = start_idp(Arc::clone(&idp)).await;
    let api_url = start_api().await;

    let id =
        common::create_connector(&app, oauth2_connector("probe-ok", &api_url, &token_url)).await;
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            &format!("/api/v1/admin/connectors/{id}/test"),
            None,
        ))
        .await
        .expect("probe");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["data"]["reachable"], true, "{body}");
    assert!(
        idp.hits.load(Ordering::SeqCst) >= 1,
        "the probe must acquire a real token"
    );

    // A connector whose credentials the IdP rejects: the probe fails instead
    // of reporting a half-validated setup as healthy.
    let mut bad = oauth2_connector("probe-bad", &api_url, &token_url);
    bad["config"]["auth"]["client_auth"] = json!("body"); // fake IdP requires Basic
    let id = common::create_connector(&app, bad).await;
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            &format!("/api/v1/admin/connectors/{id}/test"),
            None,
        ))
        .await
        .expect("probe");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["data"]["reachable"], false, "{body}");
}

/// The admin read masks the credential halves and keeps the structure —
/// grant, token endpoint, client id — readable, at the API surface.
#[tokio::test]
async fn the_admin_read_masks_oauth2_credentials() {
    let app = common::test_app().await;
    let id = common::create_connector(
        &app,
        oauth2_connector(
            "masked",
            "https://api.example.com",
            "https://idp.example.com/t",
        ),
    )
    .await;
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "GET",
            &format!("/api/v1/admin/connectors/{id}"),
            None,
        ))
        .await
        .expect("read");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    let auth = &body["data"]["config"]["auth"];
    assert_eq!(auth["client_secret"], "******", "{body}");
    assert_eq!(auth["grant"], "client_credentials", "{body}");
    assert_eq!(auth["token_url"], "https://idp.example.com/t", "{body}");
    assert_eq!(auth["client_id"], "svc-app", "{body}");
}

/// The authoring matrix reaches the API: a bad grant is a 400 at create.
#[tokio::test]
async fn a_bad_oauth2_block_is_refused_at_create() {
    let app = common::test_app().await;
    let mut bad = oauth2_connector("bad", "https://api.example.com", "https://idp/t");
    bad["config"]["auth"]["grant"] = json!("password");
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(bad),
        ))
        .await
        .expect("create");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = common::body_json(resp).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("unknown OAuth2 grant"),
        "{body}"
    );
}
