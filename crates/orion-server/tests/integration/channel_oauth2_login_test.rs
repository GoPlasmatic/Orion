//! Inbound OAuth2 sign-in (#307): the channel completes a browser
//! authorization-code grant and the workflow gets the grant, not the protocol.
//!
//! Driven against a live in-process identity provider rather than a stub,
//! because the two halves worth testing are both round trips: the redirect a
//! browser follows, and the exchange that follows it. The IdP records what it
//! was sent, which is how the CSRF case below can assert not just the `401` but
//! that nothing downstream of the check ran.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tower::ServiceExt;

use crate::common;

// ---------------------------------------------------------------------------
// A fake identity provider
// ---------------------------------------------------------------------------

/// What the provider recorded, and how it answers.
struct Idp {
    /// Token-endpoint requests. The CSRF test asserts this stays at zero.
    token_hits: AtomicU64,
    /// The form of the most recent token request.
    last_form: std::sync::Mutex<Vec<(String, String)>>,
    /// Authorization codes not yet spent. RFC 6749 §4.1.2 makes a code
    /// single-use, and the replay test depends on that being real.
    unspent: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl Idp {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            token_hits: AtomicU64::new(0),
            last_form: std::sync::Mutex::new(Vec::new()),
            unspent: std::sync::Mutex::new(["good-code".to_string()].into_iter().collect()),
        })
    }
    fn token_hits(&self) -> u64 {
        self.token_hits.load(Ordering::SeqCst)
    }
    fn form(&self, key: &str) -> Option<String> {
        self.last_form
            .lock()
            .expect("test")
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }
}

/// Enough of RFC 6749 §4.1.3 to be worth testing against, including the two
/// provider behaviours that broke the first implementation: GitHub answers
/// `200` with an `error` body for a spent code, and needs `Accept:
/// application/json` to answer JSON at all.
async fn start_idp(idp: Arc<Idp>) -> String {
    async fn token(
        axum::extract::State(idp): axum::extract::State<Arc<Idp>>,
        headers: axum::http::HeaderMap,
        body: String,
    ) -> (StatusCode, axum::Json<Value>) {
        idp.token_hits.fetch_add(1, Ordering::SeqCst);
        let form: Vec<(String, String)> = url::form_urlencoded::parse(body.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let code = form
            .iter()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        *idp.last_form.lock().expect("test") = form;

        // Mirrors GitHub: without an explicit Accept this endpoint would answer
        // form-encoded, and Orion's parse would fail with a misleading error.
        assert!(
            headers
                .get("accept")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.contains("application/json")),
            "the token request must ask for JSON"
        );

        if !idp.unspent.lock().expect("test").remove(&code) {
            // 200 with an error body — GitHub's shape for a spent or forged
            // code, and the one a status-only classification gets wrong.
            return (
                StatusCode::OK,
                axum::Json(json!({"error": "bad_verification_code"})),
            );
        }
        (
            StatusCode::OK,
            axum::Json(json!({
                "access_token": "gho_the_access_token",
                "token_type": "bearer",
                "scope": "read:user",
                "expires_in": 3600
            })),
        )
    }

    let app = axum::Router::new()
        .route("/token", axum::routing::post(token))
        .with_state(idp);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://127.0.0.1:{}", addr.port())
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A 32-byte HS256 secret, per RFC 7518 §3.2.
const STATE_SECRET: &str = "0123456789abcdef0123456789abcdef";

fn app_config() -> orion::config::AppConfig {
    let mut config = orion::config::AppConfig::default();
    config.trace_storage.mode = orion::config::TraceStorageMode::Sync;
    // The mock provider is on loopback, which the SSRF check refuses by
    // default. This is the flag that exists for exactly this case.
    config.oauth2_login.allow_private_token_urls = true;
    config
}

fn login_config(idp: &str) -> Value {
    json!({
        "authorize_url": "https://idp.example.com/authorize",
        "token_url": format!("{idp}/token"),
        "client_id": "client-123",
        "client_secret": "the-client-secret",
        "redirect_uri": "https://app.example.com/v1/auth/idp/callback",
        "callback_path": "/v1/auth/idp/callback",
        "scopes": ["read:user"],
        "state_secret": STATE_SECRET
    })
}

/// A workflow that echoes what it was handed, so a test can assert on the
/// grant rather than on a side effect.
fn echo_grant_workflow() -> Value {
    json!({
        "name": "signin",
        "description": "echo the grant",
        "condition": true,
        "tasks": [{
            "id": "echo",
            "name": "Echo the grant",
            "function": { "name": "map", "input": { "mappings": [
                { "path": "data.token", "logic": { "var": "metadata.oauth.access_token" } },
                { "path": "data.token_type", "logic": { "var": "metadata.oauth.token_type" } },
                { "path": "data.scope", "logic": { "var": "metadata.oauth.scope" } },
                { "path": "data.return_to", "logic": { "var": "metadata.oauth.return_to" } }
            ] } }
        }]
    })
}

/// Create and activate a `rest`/`GET` channel carrying an `oauth2_login` block.
async fn deploy(app: &axum::Router, login: Value, workflow: Value) -> Value {
    deploy_with_config(app, json!({ "oauth2_login": login }), workflow).await
}

/// The same, for a channel that declares other guards alongside the sign-in —
/// `deduplication`, say, which create-time validation permits and which shares
/// the callback's admission path.
async fn deploy_with_config(app: &axum::Router, config: Value, workflow: Value) -> Value {
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(workflow),
        ))
        .await
        .expect("create workflow");
    let status = resp.status();
    let wf_body = common::body_json(resp).await;
    assert_eq!(status, StatusCode::CREATED, "{wf_body}");
    let workflow_id = wf_body["data"]["workflow_id"]
        .as_str()
        .expect("workflow_id")
        .to_string();

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{workflow_id}/status"),
            Some(json!({"status": "active"})),
        ))
        .await
        .expect("activate workflow");
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "signin",
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["GET"],
                "route_pattern": "/v1/auth/idp",
                "workflow_id": workflow_id,
                "config": config
            })),
        ))
        .await
        .expect("create channel");
    let status = resp.status();
    let created = common::body_json(resp).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "GET",
            "/api/v1/admin/channels?status=draft",
            None,
        ))
        .await
        .expect("list");
    let body = common::body_json(resp).await;
    let channel_id = body["data"][0]["channel_id"]
        .as_str()
        .expect("channel_id")
        .to_string();

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{channel_id}/status"),
            Some(json!({"status": "active"})),
        ))
        .await
        .expect("activate channel");
    let status = resp.status();
    let activated = common::body_json(resp).await;
    assert_eq!(status, StatusCode::OK, "{activated}");
    json!({ "channel_id": channel_id })
}

fn get(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(c) = cookie {
        builder = builder.header("cookie", c);
    }
    builder.body(Body::empty()).expect("request")
}

/// The `name=value` pair of a `Set-Cookie`, ready to send back as a `Cookie`.
fn cookie_pair(response: &axum::http::Response<Body>) -> String {
    response
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .expect("a Set-Cookie")
        .to_string()
}

fn query_param(location: &str, name: &str) -> Option<String> {
    url::Url::parse(location)
        .ok()?
        .query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

/// Begin a sign-in and hand back `(state, cookie)`.
async fn begin(app: &axum::Router) -> (String, String) {
    let resp = app
        .clone()
        .oneshot(get("/api/v1/data/v1/auth/idp", None))
        .await
        .expect("authorize");
    assert_eq!(resp.status(), StatusCode::FOUND);
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .expect("a Location")
        .to_string();
    let cookie = cookie_pair(&resp);
    (query_param(&location, "state").expect("a state"), cookie)
}

// ---------------------------------------------------------------------------
// The authorize leg
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_authorize_leg_redirects_with_state_and_pkce() {
    let idp = Idp::new();
    let url = start_idp(Arc::clone(&idp)).await;
    let app = common::test_app_with_config(app_config()).await;
    deploy(&app, login_config(&url), echo_grant_workflow()).await;

    let resp = app
        .clone()
        .oneshot(get("/api/v1/data/v1/auth/idp", None))
        .await
        .expect("authorize");

    assert_eq!(resp.status(), StatusCode::FOUND);
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .expect("a Location")
        .to_string();
    assert!(location.starts_with("https://idp.example.com/authorize?"));
    assert_eq!(
        query_param(&location, "client_id").as_deref(),
        Some("client-123")
    );
    assert_eq!(
        query_param(&location, "response_type").as_deref(),
        Some("code")
    );
    assert_eq!(
        query_param(&location, "scope").as_deref(),
        Some("read:user")
    );
    assert_eq!(
        query_param(&location, "code_challenge_method").as_deref(),
        Some("S256")
    );
    assert!(query_param(&location, "state").is_some());
    assert!(query_param(&location, "code_challenge").is_some());

    let set_cookie = resp
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .expect("a Set-Cookie");
    assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
    assert!(set_cookie.contains("SameSite=Lax"), "{set_cookie}");

    // The workflow is not entered on this leg, so the provider has seen
    // nothing and no session exists yet.
    assert_eq!(idp.token_hits(), 0);
}

// ---------------------------------------------------------------------------
// The callback leg
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_matching_callback_exchanges_the_code_and_runs_the_workflow() {
    let idp = Idp::new();
    let url = start_idp(Arc::clone(&idp)).await;
    let app = common::test_app_with_config(app_config()).await;
    deploy(&app, login_config(&url), echo_grant_workflow()).await;

    let (state, cookie) = begin(&app).await;
    let resp = app
        .clone()
        .oneshot(get(
            &format!("/api/v1/data/v1/auth/idp/callback?code=good-code&state={state}"),
            Some(&cookie),
        ))
        .await
        .expect("callback");

    assert_eq!(resp.status(), StatusCode::OK);
    // The state cookie is retired by the response that spent it.
    let cleared = resp
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("orion_oauth_state="))
        .expect("the state cookie is cleared");
    assert!(cleared.contains("Max-Age=0"), "{cleared}");

    let body = common::body_json(resp).await;
    assert_eq!(body["data"]["token"], "gho_the_access_token");
    assert_eq!(body["data"]["token_type"], "bearer");
    assert_eq!(body["data"]["scope"], "read:user");

    // The exchange sent what RFC 6749 §4.1.3 requires, PKCE included.
    assert_eq!(idp.token_hits(), 1);
    assert_eq!(
        idp.form("grant_type").as_deref(),
        Some("authorization_code")
    );
    assert_eq!(idp.form("code").as_deref(), Some("good-code"));
    assert_eq!(
        idp.form("redirect_uri").as_deref(),
        Some("https://app.example.com/v1/auth/idp/callback")
    );
    assert!(idp.form("code_verifier").is_some());
}

/// The incident #307 reports, asserted directly. A callback arriving with no
/// state cookie ran the token exchange, wrote the user row and minted a 30-day
/// session, with nothing in the response saying the check had failed.
///
/// The `401` is the smaller half of this test. The load-bearing assertion is
/// that the identity provider saw nothing: the refusal happens *before* the
/// exchange, so there is nothing downstream of it to undo.
#[tokio::test]
async fn a_callback_with_no_state_cookie_is_refused_before_anything_runs() {
    let idp = Idp::new();
    let url = start_idp(Arc::clone(&idp)).await;
    let app = common::test_app_with_config(app_config()).await;
    deploy(&app, login_config(&url), echo_grant_workflow()).await;

    let (state, _cookie) = begin(&app).await;
    let resp = app
        .clone()
        .oneshot(get(
            &format!("/api/v1/data/v1/auth/idp/callback?code=good-code&state={state}"),
            None,
        ))
        .await
        .expect("callback");

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(idp.token_hits(), 0, "the code must not be exchanged");
}

#[tokio::test]
async fn a_state_that_does_not_match_the_cookie_is_refused() {
    let idp = Idp::new();
    let url = start_idp(Arc::clone(&idp)).await;
    let app = common::test_app_with_config(app_config()).await;
    deploy(&app, login_config(&url), echo_grant_workflow()).await;

    let (_state, cookie) = begin(&app).await;
    let resp = app
        .clone()
        .oneshot(get(
            "/api/v1/data/v1/auth/idp/callback?code=good-code&state=some-other-state",
            Some(&cookie),
        ))
        .await
        .expect("callback");

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(idp.token_hits(), 0);
}

/// A state cookie from a *different* sign-in is a valid, correctly signed,
/// unexpired token — and still must not complete this callback.
#[tokio::test]
async fn a_state_from_another_sign_in_is_refused() {
    let idp = Idp::new();
    let url = start_idp(Arc::clone(&idp)).await;
    let app = common::test_app_with_config(app_config()).await;
    deploy(&app, login_config(&url), echo_grant_workflow()).await;

    let (state_a, _cookie_a) = begin(&app).await;
    let (_state_b, cookie_b) = begin(&app).await;

    let resp = app
        .clone()
        .oneshot(get(
            &format!("/api/v1/data/v1/auth/idp/callback?code=good-code&state={state_a}"),
            Some(&cookie_b),
        ))
        .await
        .expect("callback");

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(idp.token_hits(), 0);
}

/// Replaying a completed callback. The state cookie was cleared by the first
/// response, but a client that kept it still cannot get a second session: the
/// authorization code is single-use at the provider, which is where that
/// defence actually lives.
#[tokio::test]
async fn replaying_a_spent_code_is_refused() {
    let idp = Idp::new();
    let url = start_idp(Arc::clone(&idp)).await;
    let app = common::test_app_with_config(app_config()).await;
    deploy(&app, login_config(&url), echo_grant_workflow()).await;

    let (state, cookie) = begin(&app).await;
    let uri = format!("/api/v1/data/v1/auth/idp/callback?code=good-code&state={state}");

    let first = app
        .clone()
        .oneshot(get(&uri, Some(&cookie)))
        .await
        .expect("callback");
    assert_eq!(first.status(), StatusCode::OK);

    let second = app
        .clone()
        .oneshot(get(&uri, Some(&cookie)))
        .await
        .expect("replay");
    // The provider answers `200` with an `error` body, which must classify as a
    // rejection (401) rather than a retryable transport failure (503).
    assert_eq!(second.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(idp.token_hits(), 2);
}

/// The user pressed Cancel. Still a `401` — no session was established — and
/// still no exchange.
#[tokio::test]
async fn a_provider_error_is_refused() {
    let idp = Idp::new();
    let url = start_idp(Arc::clone(&idp)).await;
    let app = common::test_app_with_config(app_config()).await;
    deploy(&app, login_config(&url), echo_grant_workflow()).await;

    let (_state, cookie) = begin(&app).await;
    let resp = app
        .clone()
        .oneshot(get(
            "/api/v1/data/v1/auth/idp/callback?error=access_denied",
            Some(&cookie),
        ))
        .await
        .expect("callback");

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(idp.token_hits(), 0);
}

// ---------------------------------------------------------------------------
// return_to
// ---------------------------------------------------------------------------

#[tokio::test]
async fn return_to_is_carried_only_when_the_allow_list_permits_it() {
    let idp = Idp::new();
    let url = start_idp(Arc::clone(&idp)).await;
    let app = common::test_app_with_config(app_config()).await;
    let mut login = login_config(&url);
    login["return_to"] = json!({
        "param": "next",
        "allow_list": ["https://app.example.com/"]
    });
    deploy(&app, login, echo_grant_workflow()).await;

    for (next, expected) in [
        (
            "https://app.example.com/dashboard",
            json!("https://app.example.com/dashboard"),
        ),
        // Not on the allow-list: dropped silently, so the workflow sees
        // nothing rather than an attacker-chosen destination.
        ("https://evil.example.com/", Value::Null),
    ] {
        let encoded: String = url::form_urlencoded::byte_serialize(next.as_bytes()).collect();
        let resp = app
            .clone()
            .oneshot(get(
                &format!("/api/v1/data/v1/auth/idp?next={encoded}"),
                None,
            ))
            .await
            .expect("authorize");
        assert_eq!(resp.status(), StatusCode::FOUND);
        let cookie = cookie_pair(&resp);
        let state = query_param(
            resp.headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .expect("a Location"),
            "state",
        )
        .expect("a state");

        idp.unspent
            .lock()
            .expect("test")
            .insert("good-code".to_string());
        let resp = app
            .clone()
            .oneshot(get(
                &format!("/api/v1/data/v1/auth/idp/callback?code=good-code&state={state}"),
                Some(&cookie),
            ))
            .await
            .expect("callback");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = common::body_json(resp).await;
        assert_eq!(body["data"]["return_to"], expected, "next={next}");
    }
}

/// `return_to` is checked on the authorize leg, where the request's own query
/// string is. With `run_workflow_on_authorize` the redirect is built *after*
/// the workflow, by which point that query is gone — so the checked value has
/// to be carried across, and the only copy otherwise within reach would be the
/// one in `metadata`, where a caller's envelope can survive.
#[tokio::test]
async fn return_to_survives_the_workflow_on_the_authorize_leg() {
    let idp = Idp::new();
    let url = start_idp(Arc::clone(&idp)).await;
    let app = common::test_app_with_config(app_config()).await;
    let mut login = login_config(&url);
    login["run_workflow_on_authorize"] = json!(true);
    login["return_to"] = json!({
        "param": "next",
        "allow_list": ["https://app.example.com/"]
    });
    deploy(&app, login, echo_grant_workflow()).await;

    let resp = app
        .clone()
        .oneshot(get(
            "/api/v1/data/v1/auth/idp?next=https%3A%2F%2Fapp.example.com%2Finbox",
            None,
        ))
        .await
        .expect("authorize");
    assert_eq!(resp.status(), StatusCode::FOUND);
    let cookie = cookie_pair(&resp);
    let state = query_param(
        resp.headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .expect("a Location"),
        "state",
    )
    .expect("a state");

    let resp = app
        .clone()
        .oneshot(get(
            &format!("/api/v1/data/v1/auth/idp/callback?code=good-code&state={state}"),
            Some(&cookie),
        ))
        .await
        .expect("callback");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        common::body_json(resp).await["data"]["return_to"],
        "https://app.example.com/inbox"
    );
}

// ---------------------------------------------------------------------------
// Reserved metadata
// ---------------------------------------------------------------------------

/// `metadata.oauth` is platform-reserved. Without the strip at ingress, a
/// caller could put an access token in an envelope and have a workflow trust it
/// as Orion's — on the authorize leg, where no grant exists at all.
#[tokio::test]
async fn a_caller_cannot_forge_the_grant() {
    let idp = Idp::new();
    let url = start_idp(Arc::clone(&idp)).await;
    let app = common::test_app_with_config(app_config()).await;
    let mut login = login_config(&url);
    login["run_workflow_on_authorize"] = json!(true);
    deploy(&app, login, echo_grant_workflow()).await;

    let request = Request::builder()
        .method("GET")
        .uri("/api/v1/data/v1/auth/idp")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "data": {},
                "metadata": { "oauth": { "access_token": "forged" } }
            })
            .to_string(),
        ))
        .expect("request");

    // The workflow runs, contributes nothing, and Orion redirects — the point
    // being that the forged token never reached it.
    let resp = app.clone().oneshot(request).await.expect("authorize");
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert_eq!(idp.token_hits(), 0);
}

// ---------------------------------------------------------------------------
// run_workflow_on_authorize
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_workflow_can_contribute_to_the_authorize_redirect() {
    let idp = Idp::new();
    let url = start_idp(Arc::clone(&idp)).await;
    let app = common::test_app_with_config(app_config()).await;
    let mut login = login_config(&url);
    login["run_workflow_on_authorize"] = json!(true);
    deploy(
        &app,
        login,
        json!({
            "name": "signin",
            "condition": true,
            "tasks": [{
                "id": "hint",
                "name": "Contribute a login hint",
                "function": { "name": "map", "input": { "mappings": [{
                    "path": "data._orion.oauth2.authorize",
                    "logic": {
                        "extra_params": { "login_hint": "a@b.com", "state": "hijacked" },
                        "scopes": ["read:user", "user:email"]
                    }
                }] } }
            }]
        }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(get("/api/v1/data/v1/auth/idp", None))
        .await
        .expect("authorize");
    assert_eq!(resp.status(), StatusCode::FOUND);
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .expect("a Location")
        .to_string();

    assert_eq!(
        query_param(&location, "login_hint").as_deref(),
        Some("a@b.com")
    );
    assert_eq!(
        query_param(&location, "scope").as_deref(),
        Some("read:user user:email")
    );
    // A workflow cannot reach the parameters that carry the flow's security
    // properties, whatever it writes.
    assert_ne!(query_param(&location, "state").as_deref(), Some("hijacked"));
}

/// The workflow's own shaped response wins on the authorize leg — that is how
/// a sign-in is refused without Orion needing a vocabulary for refusal.
#[tokio::test]
async fn a_workflow_can_refuse_the_sign_in() {
    let idp = Idp::new();
    let url = start_idp(Arc::clone(&idp)).await;
    let app = common::test_app_with_config(app_config()).await;
    let mut login = login_config(&url);
    login["run_workflow_on_authorize"] = json!(true);

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "signin",
                "condition": true,
                "tasks": [{
                    "id": "refuse",
                    "name": "Refuse the sign-in",
                    "function": { "name": "map", "input": { "mappings": [
                        { "path": "data.body", "logic": { "error": "closed" } },
                        { "path": "data._orion.response",
                          "logic": { "status": 503, "body_path": "data.body" } }
                    ] } }
                }]
            })),
        ))
        .await
        .expect("create workflow");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let workflow_id = common::body_json(resp).await["data"]["workflow_id"]
        .as_str()
        .expect("workflow_id")
        .to_string();
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{workflow_id}/status"),
            Some(json!({"status": "active"})),
        ))
        .await
        .expect("activate");
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "signin",
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["GET"],
                "route_pattern": "/v1/auth/idp",
                "workflow_id": workflow_id,
                "config": {
                    "response": { "mode": "shaped" },
                    "oauth2_login": login
                }
            })),
        ))
        .await
        .expect("create channel");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let channel_id = common::body_json(resp).await["data"]["channel_id"]
        .as_str()
        .expect("channel_id")
        .to_string();
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{channel_id}/status"),
            Some(json!({"status": "active"})),
        ))
        .await
        .expect("activate channel");
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(get("/api/v1/data/v1/auth/idp", None))
        .await
        .expect("authorize");
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(resp.headers().get("location").is_none());
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// `/async` is stripped before route matching, so without an explicit refusal
/// the callback would resolve with the sign-in guard off and run the workflow
/// with no grant — a sign-in that answers `202` and established nothing.
#[tokio::test]
async fn a_callback_cannot_be_submitted_asynchronously() {
    let idp = Idp::new();
    let url = start_idp(Arc::clone(&idp)).await;
    let app = common::test_app_with_config(app_config()).await;
    deploy(&app, login_config(&url), echo_grant_workflow()).await;

    let resp = app
        .clone()
        .oneshot(get(
            "/api/v1/data/v1/auth/idp/callback/async?code=good-code&state=x",
            None,
        ))
        .await
        .expect("async callback");
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(idp.token_hits(), 0);
}

// ---------------------------------------------------------------------------
// Config refusals — asserted on create and on update alike
// ---------------------------------------------------------------------------

async fn assert_refused(app: &axum::Router, config: Value, expect: &str) {
    // Create
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": format!("refused-{}", uuid::Uuid::new_v4().simple()),
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["GET"],
                "route_pattern": "/v1/auth/idp",
                "config": config,
            })),
        ))
        .await
        .expect("create");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "create: {expect}");
    let body = common::body_json(resp).await;
    assert!(
        body.to_string().contains(expect),
        "create: expected {expect:?} in {body}"
    );

    // Update, against a stored draft that is otherwise valid.
    let name = format!("draft-{}", uuid::Uuid::new_v4().simple());
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": name,
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["GET"],
                "route_pattern": "/v1/auth/idp",
                "config": {},
            })),
        ))
        .await
        .expect("create draft");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let channel_id = common::body_json(resp).await["data"]["channel_id"]
        .as_str()
        .expect("channel_id")
        .to_string();

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "PUT",
            &format!("/api/v1/admin/channels/{channel_id}"),
            Some(json!({ "config": config })),
        ))
        .await
        .expect("update");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "update: {expect}");
}

#[tokio::test]
async fn the_config_refusals_hold_on_create_and_update() {
    let app = common::test_app_with_config(app_config()).await;
    let base = login_config("https://idp.example.com");

    // A plaintext token endpoint that is not loopback.
    let mut http_token = base.clone();
    http_token["token_url"] = json!("http://idp.example.com/token");
    assert_refused(&app, json!({ "oauth2_login": http_token }), "https").await;

    // The response cache would replay one visitor's sign-in to the next.
    assert_refused(
        &app,
        json!({
            "oauth2_login": base,
            "cache": { "enabled": true, "ttl_secs": 60 }
        }),
        "response cache",
    )
    .await;

    // The callback and the authorize leg are two requests, so two paths.
    let mut same_path = login_config("https://idp.example.com");
    same_path["callback_path"] = json!("/v1/auth/idp");
    assert_refused(&app, json!({ "oauth2_login": same_path }), "route_pattern").await;

    // `Strict` withholds the cookie on the callback, so every sign-in fails.
    let mut strict = login_config("https://idp.example.com");
    strict["state_cookie"] = json!({ "same_site": "strict" });
    assert_refused(&app, json!({ "oauth2_login": strict }), "cross-site").await;

    // Overriding `state` would disable the CSRF binding.
    let mut reserved = login_config("https://idp.example.com");
    reserved["extra_authorize_params"] = json!({ "state": "chosen" });
    assert_refused(&app, json!({ "oauth2_login": reserved }), "state").await;

    // A guard that silently does not run is the whole reason for the strict
    // parse — a typo inside this block is refused like any other.
    let mut typo = login_config("https://idp.example.com");
    typo["callbackpath"] = json!("/x");
    assert_refused(&app, json!({ "oauth2_login": typo }), "callbackpath").await;
}

/// Both legs are routes, so a channel with nowhere for the provider to send
/// the browser back to is not a sign-in channel.
#[tokio::test]
async fn a_non_rest_channel_cannot_declare_a_sign_in() {
    let app = common::test_app_with_config(app_config()).await;
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "kafka-signin",
                "channel_type": "async",
                "protocol": "kafka",
                "topic": "signin",
                "consumer_group": "orion",
                "config": { "oauth2_login": login_config("https://idp.example.com") },
            })),
        ))
        .await
        .expect("create");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = common::body_json(resp).await;
    assert!(body.to_string().contains("rest"), "{body}");
}

// ---------------------------------------------------------------------------
// Masking
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_channel_read_masks_the_secrets_and_keeps_the_pointers() {
    let app = common::test_app_with_config(app_config()).await;
    let mut login = login_config("https://idp.example.com");
    // A literal is masked; a reference survives, because the stored config
    // never held the value and masking the pointer breaks export → import.
    login["state_secret"] = json!("env://ORION_SECRET_OAUTH_STATE");

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "masked-signin",
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["GET"],
                "route_pattern": "/v1/auth/idp",
                "config": { "oauth2_login": login },
            })),
        ))
        .await
        .expect("create");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let channel_id = common::body_json(resp).await["data"]["channel_id"]
        .as_str()
        .expect("channel_id")
        .to_string();

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "GET",
            &format!("/api/v1/admin/channels/{channel_id}"),
            None,
        ))
        .await
        .expect("read");
    let body = common::body_json(resp).await;
    let read = &body["data"]["config"]["oauth2_login"];

    assert_ne!(read["client_secret"], "the-client-secret");
    assert_eq!(read["state_secret"], "env://ORION_SECRET_OAUTH_STATE");
    // The client id travels in every user's address bar; masking it would only
    // make a channel read useless for reviewing the flow.
    assert_eq!(read["client_id"], "client-123");
}

// ---------------------------------------------------------------------------
// The grant must not reach disk
// ---------------------------------------------------------------------------

/// `redact_paths` is a path list, not a metadata-wide prune, so a new
/// platform-reserved key is covered only by being named there. Without
/// `metadata.oauth` on it, the access token is cloned into every step snapshot
/// and written to `traces.task_trace_json` for any channel that opts into
/// per-task capture.
///
/// Asserted against the stored row rather than the API projection: the trace
/// *read* already strips `context.metadata`, so a missing entry would not
/// surface through the API and this test would pass while the token sat on
/// disk.
#[tokio::test]
async fn the_access_token_never_reaches_the_persisted_trace() {
    let idp = Idp::new();
    let url = start_idp(Arc::clone(&idp)).await;
    let app = common::test_app_with_config(app_config()).await;

    let mut login = login_config(&url);
    login["state_secret"] = json!(STATE_SECRET);
    // Deploy with per-task capture turned on — the setting that makes the
    // snapshots exist at all.
    // Deliberately *not* the echo workflow used elsewhere. That one copies the
    // token into `data`, which is the caller's response body and is persisted
    // because the author asked for it — a workflow writing a secret into its
    // own output is the author's decision, not a leak. This one proves it read
    // the grant without republishing it, so the only way the token can appear
    // below is if Orion put it there.
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "signin",
                "condition": true,
                "tasks": [{
                    "id": "observe",
                    "name": "Observe the grant without echoing it",
                    "function": { "name": "map", "input": { "mappings": [{
                        "path": "data.saw_a_token",
                        "logic": { "!!": { "var": "metadata.oauth.access_token" } }
                    }] } }
                }]
            })),
        ))
        .await
        .expect("create workflow");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let workflow_id = common::body_json(resp).await["data"]["workflow_id"]
        .as_str()
        .expect("workflow_id")
        .to_string();
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{workflow_id}/status"),
            Some(json!({"status": "active"})),
        ))
        .await
        .expect("activate workflow");
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "signin",
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["GET"],
                "route_pattern": "/v1/auth/idp",
                "workflow_id": workflow_id,
                "config": {
                    "tracing": { "task_details": true },
                    "oauth2_login": login
                }
            })),
        ))
        .await
        .expect("create channel");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let channel_id = common::body_json(resp).await["data"]["channel_id"]
        .as_str()
        .expect("channel_id")
        .to_string();
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{channel_id}/status"),
            Some(json!({"status": "active"})),
        ))
        .await
        .expect("activate channel");
    assert_eq!(resp.status(), StatusCode::OK);

    let (state, cookie) = begin(&app).await;
    let resp = app
        .clone()
        .oneshot(get(
            &format!("/api/v1/data/v1/auth/idp/callback?code=good-code&state={state}"),
            Some(&cookie),
        ))
        .await
        .expect("callback");
    assert_eq!(resp.status(), StatusCode::OK);
    // The workflow did read it, so this is a redaction test and not a
    // did-the-feature-run test.
    assert_eq!(common::body_json(resp).await["data"]["saw_a_token"], true);

    let body = common::wait_for_body(&app, "/api/v1/admin/traces", |b| {
        b["data"]
            .as_array()
            .is_some_and(|a| a.iter().any(|r| r["channel"] == "signin"))
    })
    .await;
    let trace_id = body["data"]
        .as_array()
        .and_then(|a| a.iter().find(|r| r["channel"] == "signin"))
        .and_then(|r| r["id"].as_str())
        .expect("a trace row")
        .to_string();

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "GET",
            &format!("/api/v1/admin/traces/{trace_id}"),
            None,
        ))
        .await
        .expect("trace detail");
    assert_eq!(resp.status(), StatusCode::OK);
    let detail = common::body_json(resp).await;

    let rendered = detail.to_string();
    assert!(
        !rendered.contains("gho_the_access_token"),
        "the access token reached the persisted trace: {rendered}"
    );
}

// ---------------------------------------------------------------------------
// The spent state cookie is retired on every outcome
// ---------------------------------------------------------------------------

/// Clearing the state cookie is the **only** single-use enforcement Orion
/// performs — the module doc says so, and says why: the state lives in a signed
/// cookie rather than a stored row, so nothing else can retire it. That makes
/// the outcomes where the clear was skipped exactly the outcomes a replay would
/// follow.
///
/// It was skipped on three of them. `append_cookies` sat on the success arms
/// only, so a callback whose workflow timed out, hit an engine error, or
/// overran `max_result_size_bytes` answered 504/500 and left the spent cookie
/// in the browser until its own `exp` — while the comment above
/// `response_cookies` claimed it was appended "to whatever the workflow
/// answers, including a failure".
///
/// `max_result_size_bytes` is the outcome this drives because it is the
/// deterministic one: no sleeping, no unreachable host, just a workflow whose
/// echoed grant is larger than a one-byte cap.
#[tokio::test]
async fn a_failing_callback_still_retires_the_spent_state_cookie() {
    let idp = Idp::new();
    let url = start_idp(Arc::clone(&idp)).await;

    let mut config = app_config();
    // Small enough that any real envelope exceeds it, and the callback ends in
    // `OrionError::ResponseTooLarge` rather than a `200`.
    config.trace_queue.max_result_size_bytes = 1;
    let app = common::test_app_with_config(config).await;
    deploy(&app, login_config(&url), echo_grant_workflow()).await;

    let (state, cookie) = begin(&app).await;
    let resp = app
        .clone()
        .oneshot(get(
            &format!("/api/v1/data/v1/auth/idp/callback?code=good-code&state={state}"),
            Some(&cookie),
        ))
        .await
        .expect("callback");

    let status = resp.status();
    assert!(
        status.is_server_error(),
        "the cap must actually fire, or this test proves nothing: {status}"
    );

    let cleared = resp
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("orion_oauth_state="))
        .expect("the spent state cookie must be retired even on a failure");
    assert!(
        cleared.contains("Max-Age=0"),
        "the clear must expire it, not reissue it: {cleared}"
    );
}

/// The other half of the same rule: a channel with no `oauth2_login` has no
/// platform cookies, and its failures must keep propagating as errors rather
/// than being materialised into responses on the way past.
#[tokio::test]
async fn a_failure_on_an_ordinary_channel_carries_no_platform_cookie() {
    let mut config = app_config();
    config.trace_queue.max_result_size_bytes = 1;
    let app = common::test_app_with_config(config).await;

    common::create_and_activate_channel(
        &app,
        "plain",
        common::workflow_with_tasks(
            "Plain",
            json!([{
                "id": "echo",
                "name": "Echo",
                "function": { "name": "map", "input": { "mappings": [
                    { "path": "data.value", "logic": "a-value-larger-than-one-byte" }
                ] } }
            }]),
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/plain",
            Some(json!({"data": {}})),
        ))
        .await
        .expect("request");

    assert!(resp.status().is_server_error(), "{}", resp.status());
    assert_eq!(
        resp.headers().get_all("set-cookie").iter().count(),
        0,
        "a channel with no oauth2_login has no platform cookie to append"
    );
}

/// The authorize leg writes a trace when it runs a workflow.
///
/// With `run_workflow_on_authorize` the workflow really runs before the
/// redirect — it can write rows, call connectors and emit task errors — but the
/// redirect used to `return` from a block sitting *above* `persist_trace_and_cache`,
/// so no trace row was ever written under any `trace_storage.mode`, `sync`
/// included. A failing sign-in workflow was invisible to the trace API, which
/// is the one place an operator would look for it.
#[tokio::test]
async fn the_authorize_leg_persists_a_trace_when_it_runs_a_workflow() {
    let idp = Idp::new();
    let url = start_idp(Arc::clone(&idp)).await;
    let app = common::test_app_with_config(app_config()).await;
    let mut login = login_config(&url);
    login["run_workflow_on_authorize"] = json!(true);
    deploy(&app, login, echo_grant_workflow()).await;

    let resp = app
        .clone()
        .oneshot(get("/api/v1/data/v1/auth/idp", None))
        .await
        .expect("authorize");
    // Still a redirect: routing it through the trace tail must not change what
    // the browser gets, cookie included.
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert!(resp.headers().get("location").is_some(), "a Location");
    assert!(
        resp.headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| v.starts_with("orion_oauth_state=")),
        "the state cookie must still be minted"
    );

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "GET",
            "/api/v1/admin/traces?limit=50",
            None,
        ))
        .await
        .expect("traces");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    let rows = body["data"].as_array().expect("rows");
    assert!(
        rows.iter().any(|r| r["channel"] == "signin"),
        "the authorize leg must leave a trace row: {body}"
    );
}

/// The counterpart: without `run_workflow_on_authorize` no workflow runs on the
/// authorize leg, so there is nothing to trace and the redirect is still built
/// before any of this.
#[tokio::test]
async fn the_authorize_leg_traces_nothing_when_no_workflow_runs() {
    let idp = Idp::new();
    let url = start_idp(Arc::clone(&idp)).await;
    let app = common::test_app_with_config(app_config()).await;
    deploy(&app, login_config(&url), echo_grant_workflow()).await;

    let resp = app
        .clone()
        .oneshot(get("/api/v1/data/v1/auth/idp", None))
        .await
        .expect("authorize");
    assert_eq!(resp.status(), StatusCode::FOUND);

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "GET",
            "/api/v1/admin/traces?limit=50",
            None,
        ))
        .await
        .expect("traces");
    let body = common::body_json(resp).await;
    assert_eq!(
        body["data"].as_array().map(Vec::len),
        Some(0),
        "no workflow ran, so there is nothing to trace: {body}"
    );
}

// ---------------------------------------------------------------------------
// A failed callback must not burn the idempotency key
// ---------------------------------------------------------------------------

/// The dedup claim is taken *before* the sign-in is completed, and every
/// failure in `login.complete` — a missing or mismatched state, a bad nonce, a
/// rejected exchange — happens before the workflow. Nothing ran, so the key has
/// to go back; held, it turns the user's retry into a `409` for the rest of the
/// window, so a sign-in that failed its CSRF check becomes a sign-in that
/// cannot be attempted again.
///
/// The backpressure branch fifteen lines above in `guards.rs` already said the
/// rule out loud ("Nothing ran. Hand the key back."); this path did the
/// opposite. Note that create-time validation refuses `oauth2_login` alongside
/// `cache` but says nothing about `deduplication`, so this combination is
/// deployable and was reachable.
#[tokio::test]
async fn a_failed_callback_does_not_burn_the_idempotency_key() {
    let idp = Idp::new();
    let url = start_idp(Arc::clone(&idp)).await;
    let app = common::test_app_with_config(app_config()).await;
    let mut login = login_config(&url);
    login["run_workflow_on_authorize"] = json!(false);
    deploy_with_config(
        &app,
        json!({
            "oauth2_login": login,
            "deduplication": { "header": "Idempotency-Key", "window_secs": 300 }
        }),
        echo_grant_workflow(),
    )
    .await;

    let (state, cookie) = begin(&app).await;

    // A callback carrying the right key but *no* state cookie: refused by the
    // CSRF check, before the exchange and before the workflow.
    let refused = |with_cookie: Option<&str>| {
        let mut b = Request::builder()
            .method("GET")
            .uri(format!(
                "/api/v1/data/v1/auth/idp/callback?code=good-code&state={state}"
            ))
            .header("Idempotency-Key", "retry-me");
        if let Some(c) = with_cookie {
            b = b.header("cookie", c);
        }
        b.body(Body::empty()).expect("request")
    };

    let resp = app
        .clone()
        .oneshot(refused(None))
        .await
        .expect("first callback");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "no state cookie is a refusal, not a duplicate"
    );

    // The retry — this time with the cookie — must be judged on its merits.
    // While the key was burned this answered `409`, so a user whose first
    // attempt lost its cookie could not sign in again for the whole window.
    let resp = app
        .clone()
        .oneshot(refused(Some(&cookie)))
        .await
        .expect("retry");
    assert_ne!(
        resp.status(),
        StatusCode::CONFLICT,
        "the retry of a callback that never ran must not be a duplicate"
    );
    assert_eq!(resp.status(), StatusCode::OK, "and it should now succeed");
}

/// The other side of the same rule: a callback that *succeeds* keeps its claim,
/// so a genuine replay of one delivery is still refused.
#[tokio::test]
async fn a_successful_callback_still_holds_the_idempotency_key() {
    let idp = Idp::new();
    let url = start_idp(Arc::clone(&idp)).await;
    let app = common::test_app_with_config(app_config()).await;
    deploy_with_config(
        &app,
        json!({
            "oauth2_login": login_config(&url),
            "deduplication": { "header": "Idempotency-Key", "window_secs": 300 }
        }),
        echo_grant_workflow(),
    )
    .await;

    let (state, cookie) = begin(&app).await;
    let call = || {
        Request::builder()
            .method("GET")
            .uri(format!(
                "/api/v1/data/v1/auth/idp/callback?code=good-code&state={state}"
            ))
            .header("Idempotency-Key", "once-only")
            .header("cookie", cookie.clone())
            .body(Body::empty())
            .expect("request")
    };

    let resp = app.clone().oneshot(call()).await.expect("first");
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app.clone().oneshot(call()).await.expect("replay");
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "a replay of a delivery that succeeded is still a duplicate"
    );
}

// ---------------------------------------------------------------------------
// Per-environment values
// ---------------------------------------------------------------------------

/// The book's own example: `client_id` and `redirect_uri` as `var://`, the
/// secrets as `env://`. Create-time validation used to drop every `var://`
/// member before the shape check and then report both as missing fields, so
/// the one complete sign-in example in the reference could not be deployed.
#[tokio::test]
async fn the_documented_example_deploys_from_vars_and_env() {
    // SAFETY: names no other test reads, set before the channel loads.
    unsafe {
        std::env::set_var("ORION_TEST_OAUTH2_DOC_CLIENT_SECRET", "the-client-secret");
        std::env::set_var("ORION_TEST_OAUTH2_DOC_STATE_SECRET", STATE_SECRET);
    }
    let idp = Idp::new();
    let url = start_idp(Arc::clone(&idp)).await;
    let mut config = app_config();
    config.vars = orion::config::VarsConfig(
        [
            (
                "idp_client_id".to_string(),
                toml::Value::String("client-from-vars".to_string()),
            ),
            (
                "app_redirect_uri".to_string(),
                toml::Value::String("https://app.example.com/v1/auth/idp/callback".to_string()),
            ),
        ]
        .into_iter()
        .collect(),
    );
    let app = common::test_app_with_config(config).await;
    let mut login = login_config(&url);
    login["client_id"] = json!("var://idp_client_id");
    login["client_secret"] = json!("env://ORION_TEST_OAUTH2_DOC_CLIENT_SECRET");
    login["redirect_uri"] = json!("var://app_redirect_uri");
    login["state_secret"] = json!("env://ORION_TEST_OAUTH2_DOC_STATE_SECRET");
    deploy(&app, login, echo_grant_workflow()).await;

    let resp = app
        .clone()
        .oneshot(get("/api/v1/data/v1/auth/idp", None))
        .await
        .expect("authorize");
    assert_eq!(resp.status(), StatusCode::FOUND);
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .expect("a Location")
        .to_string();
    assert_eq!(
        query_param(&location, "client_id").as_deref(),
        Some("client-from-vars")
    );
    assert_eq!(
        query_param(&location, "redirect_uri").as_deref(),
        Some("https://app.example.com/v1/auth/idp/callback")
    );

    // The exchange carries the same resolved value, as RFC 6749 §4.1.3
    // requires of the two legs.
    let (state, cookie) = begin(&app).await;
    let resp = app
        .clone()
        .oneshot(get(
            &format!("/api/v1/data/v1/auth/idp/callback?code=good-code&state={state}"),
            Some(&cookie),
        ))
        .await
        .expect("callback");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        idp.form("redirect_uri").as_deref(),
        Some("https://app.example.com/v1/auth/idp/callback")
    );
}

/// `redirect_uri` resolves a secret reference at load, like `client_id`. It
/// used to be refused at create as a non-`https` URL — the scheme check ran
/// on the reference rather than on what it resolves to.
#[tokio::test]
async fn redirect_uri_resolves_an_env_reference_at_load() {
    // SAFETY: a name no other test reads, set before the channel loads.
    unsafe {
        std::env::set_var(
            "ORION_TEST_OAUTH2_REDIRECT_URI",
            "https://app.example.com/v1/auth/idp/callback",
        );
    }
    let idp = Idp::new();
    let url = start_idp(Arc::clone(&idp)).await;
    let app = common::test_app_with_config(app_config()).await;
    let mut login = login_config(&url);
    login["redirect_uri"] = json!("env://ORION_TEST_OAUTH2_REDIRECT_URI");
    deploy(&app, login, echo_grant_workflow()).await;

    let (state, cookie) = begin(&app).await;
    let resp = app
        .clone()
        .oneshot(get(
            &format!("/api/v1/data/v1/auth/idp/callback?code=good-code&state={state}"),
            Some(&cookie),
        ))
        .await
        .expect("callback");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        idp.form("redirect_uri").as_deref(),
        Some("https://app.example.com/v1/auth/idp/callback")
    );
}

/// Deferring the check to load is not skipping it: a reference that resolves
/// to plain `http` fails the same rule there, and the channel is quarantined
/// rather than served with a redirect the secret would travel over.
#[tokio::test]
async fn a_reference_that_resolves_to_plain_http_quarantines_the_channel() {
    // SAFETY: a name no other test reads, set before the channel loads.
    unsafe {
        std::env::set_var(
            "ORION_TEST_OAUTH2_REDIRECT_URI_HTTP",
            "http://app.example.com/v1/auth/idp/callback",
        );
    }
    let app = common::test_app_with_config(app_config()).await;
    let mut login = login_config("https://idp.example.com");
    login["redirect_uri"] = json!("env://ORION_TEST_OAUTH2_REDIRECT_URI_HTTP");
    deploy(&app, login, echo_grant_workflow()).await;

    let resp = app
        .clone()
        .oneshot(get("/api/v1/data/v1/auth/idp", None))
        .await
        .expect("authorize");
    assert_ne!(
        resp.status(),
        StatusCode::FOUND,
        "a quarantined channel must not redirect"
    );

    let resp = app
        .clone()
        .oneshot(common::json_request("GET", "/health", None))
        .await
        .expect("health");
    let body = common::body_json(resp).await;
    let quarantined = body["channels"]["quarantined"]
        .as_array()
        .expect("array")
        .clone();
    let entry = quarantined
        .iter()
        .find(|q| q["channel"] == "signin")
        .unwrap_or_else(|| panic!("health must list the channel: {quarantined:?}"));
    assert!(
        entry.to_string().contains("https"),
        "the reason names the rule: {entry}"
    );
}
