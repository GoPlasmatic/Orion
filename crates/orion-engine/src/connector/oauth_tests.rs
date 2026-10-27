use super::*;
use crate::connector::config::AuthConfig;
use crate::connector::test_support::StubConnectorRepo;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use serde_json::{Value, json};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// What the fake IdP records and how it answers — the observable half of
/// every test below.
struct IdpState {
    hits: AtomicU64,
    /// The `refresh_token` presented on the most recent refresh.
    last_refresh_token: Mutex<Option<String>>,
    /// Whether the last request carried HTTP Basic client auth.
    last_had_basic: Mutex<bool>,
    last_form: Mutex<Vec<(String, String)>>,
    /// The canned answer: `(status, body)`.
    respond: Mutex<(u16, Value)>,
}

impl IdpState {
    fn ok(body: Value) -> Arc<Self> {
        Arc::new(Self {
            hits: AtomicU64::new(0),
            last_refresh_token: Mutex::new(None),
            last_had_basic: Mutex::new(false),
            last_form: Mutex::new(Vec::new()),
            respond: Mutex::new((200, body)),
        })
    }
    fn hits(&self) -> u64 {
        self.hits.load(Ordering::SeqCst)
    }
    fn set_response(&self, status: u16, body: Value) {
        *self.respond.lock().expect("test") = (status, body);
    }
}

/// An in-process token endpoint speaking just enough RFC 6749.
async fn fake_idp(state: Arc<IdpState>) -> String {
    async fn token(
        State(st): State<Arc<IdpState>>,
        headers: HeaderMap,
        body: String,
    ) -> (StatusCode, axum::Json<Value>) {
        let form: Vec<(String, String)> = url::form_urlencoded::parse(body.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        st.hits.fetch_add(1, Ordering::SeqCst);
        *st.last_had_basic.lock().expect("test") = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("Basic "));
        *st.last_refresh_token.lock().expect("test") = form
            .iter()
            .find(|(k, _)| k == "refresh_token")
            .map(|(_, v)| v.clone());
        *st.last_form.lock().expect("test") = form;
        let (status, body) = st.respond.lock().expect("test").clone();
        (
            StatusCode::from_u16(status).expect("test status"),
            axum::Json(body),
        )
    }
    let app = axum::Router::new()
        .route("/token", axum::routing::post(token))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://{addr}/token")
}

fn manager_with(repo: Arc<StubConnectorRepo>) -> OAuthTokenManager {
    let manager = OAuthTokenManager::new();
    manager.init(OAuthRuntimeDeps {
        http_client: reqwest::Client::new(),
        repo,
        lease: None,
    });
    manager
}

fn cc_config(token_url: &str) -> OAuth2Config {
    OAuth2Config {
        grant: "client_credentials".to_string(),
        token_url: token_url.to_string(),
        client_id: "cid".to_string(),
        client_secret: "csecret".to_string(),
        client_auth: "basic".to_string(),
        refresh_token: None,
        scopes: vec!["api.read".to_string(), "api.write".to_string()],
        audience: Some("https://api.example.com".to_string()),
        resource: None,
        account_id: None,
        extra_params: HashMap::new(),
        refresh_margin_secs: 1,
    }
}

fn rt_config(token_url: &str, seed: &str) -> OAuth2Config {
    OAuth2Config {
        grant: "refresh_token".to_string(),
        refresh_token: Some(seed.to_string()),
        scopes: Vec::new(),
        audience: None,
        ..cc_config(token_url)
    }
}

/// Zoom Server-to-Server: the account id replaces the audience, and there
/// is no seed to carry.
fn ac_config(token_url: &str) -> OAuth2Config {
    OAuth2Config {
        grant: "account_credentials".to_string(),
        account_id: Some("zoom-acct-1".to_string()),
        audience: None,
        scopes: Vec::new(),
        ..cc_config(token_url)
    }
}

// ---- client_credentials: acquisition, caching, request shape ----

#[tokio::test]
async fn a_cached_token_serves_every_call_until_the_margin() {
    let idp = IdpState::ok(json!({
        "access_token": "tok-1", "token_type": "Bearer", "expires_in": 3600
    }));
    let url = fake_idp(Arc::clone(&idp)).await;
    let manager = manager_with(Arc::new(StubConnectorRepo::with(vec![])));
    let cfg = cc_config(&url);

    for _ in 0..5 {
        let token = manager
            .access_token("crm", &cfg, true)
            .await
            .expect("token");
        assert_eq!(token, "tok-1");
    }
    assert_eq!(idp.hits(), 1, "one acquisition serves the cache window");

    // The request carried the RFC 6749 shape: Basic client auth, the
    // grant, the space-joined scopes, and the audience.
    assert!(*idp.last_had_basic.lock().expect("test"));
    let form = idp.last_form.lock().expect("test").clone();
    let get = |k: &str| {
        form.iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(get("grant_type").as_deref(), Some("client_credentials"));
    assert_eq!(get("scope").as_deref(), Some("api.read api.write"));
    assert_eq!(get("audience").as_deref(), Some("https://api.example.com"));
    assert_eq!(
        get("client_id"),
        None,
        "basic auth puts nothing in the body"
    );
}

// ---- account_credentials (Zoom Server-to-Server) ----

/// #273: the grant's whole request shape —
/// `grant_type=account_credentials`, the account id, and Basic client
/// auth, which is what Zoom expects and Orion's default.
#[tokio::test]
async fn account_credentials_sends_the_zoom_request_shape() {
    let idp = IdpState::ok(json!({
        "access_token": "zoom-tok", "token_type": "Bearer", "expires_in": 3600
    }));
    let url = fake_idp(Arc::clone(&idp)).await;
    let manager = manager_with(Arc::new(StubConnectorRepo::with(vec![])));
    let cfg = ac_config(&url);

    let token = manager
        .access_token("zoom", &cfg, true)
        .await
        .expect("token");
    assert_eq!(token, "zoom-tok");

    assert!(
        *idp.last_had_basic.lock().expect("test"),
        "Zoom authenticates the client with HTTP Basic"
    );
    let form = idp.last_form.lock().expect("test").clone();
    let get = |k: &str| {
        form.iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(get("grant_type").as_deref(), Some("account_credentials"));
    assert_eq!(get("account_id").as_deref(), Some("zoom-acct-1"));
    assert_eq!(
        get("client_id"),
        None,
        "basic auth puts nothing in the body"
    );
}

/// The grant inherits the cache for free — it goes through the same
/// `cache_token` path as `client_credentials`, with no rotation state.
#[tokio::test]
async fn an_account_credentials_token_is_cached_like_any_other() {
    let idp = IdpState::ok(json!({
        "access_token": "zoom-tok", "expires_in": 3600
    }));
    let url = fake_idp(Arc::clone(&idp)).await;
    let repo = Arc::new(StubConnectorRepo::with(vec![]));
    let manager = manager_with(Arc::clone(&repo) as Arc<StubConnectorRepo>);
    let cfg = ac_config(&url);

    for _ in 0..5 {
        assert_eq!(
            manager
                .access_token("zoom", &cfg, true)
                .await
                .expect("token"),
            "zoom-tok"
        );
    }
    assert_eq!(idp.hits(), 1, "one acquisition serves the cache window");
    assert!(
        repo.get_oauth_state("zoom").await.expect("repo").is_none(),
        "re-acquired from static credentials, so there is no rotation state \
             to persist and no cluster lease to take"
    );
}

/// ⚠️ The upgrade hazard, pinned. `fingerprint` hashes the whole
/// serialized `OAuth2Config`, and a persisted state row is adopted only on
/// a fingerprint match. If `account_id` serialized as `"account_id": null`
/// when unset, **every existing refresh_token connector** would change
/// fingerprint on upgrade, discard its persisted state and fall back to
/// the config seed — which, after any prior rotation, is a spent refresh
/// token. `skip_serializing_if` is what prevents that, and this test is
/// what stops someone "tidying" it away.
#[test]
fn an_unset_account_id_does_not_move_an_existing_fingerprint() {
    let cfg = rt_config("https://idp.example/token", "rt-seed");
    let serialized = serde_json::to_string(&cfg).expect("json");
    assert!(
        !serialized.contains("account_id"),
        "an unset account_id must not serialize at all, or it changes the \
             fingerprint of every stored connector: {serialized}"
    );
}

#[tokio::test]
async fn client_auth_body_moves_the_credentials_into_the_form() {
    let idp = IdpState::ok(json!({ "access_token": "t", "expires_in": 3600 }));
    let url = fake_idp(Arc::clone(&idp)).await;
    let manager = manager_with(Arc::new(StubConnectorRepo::with(vec![])));
    let cfg = OAuth2Config {
        client_auth: "body".to_string(),
        ..cc_config(&url)
    };
    manager
        .access_token("crm", &cfg, true)
        .await
        .expect("token");
    assert!(!*idp.last_had_basic.lock().expect("test"));
    let form = idp.last_form.lock().expect("test").clone();
    assert!(form.iter().any(|(k, v)| k == "client_id" && v == "cid"));
    assert!(
        form.iter()
            .any(|(k, v)| k == "client_secret" && v == "csecret")
    );
}

/// The single-flight guarantee: concurrent first requests produce one
/// token acquisition, not a race.
#[tokio::test]
async fn concurrent_requests_share_one_acquisition() {
    let idp = IdpState::ok(json!({ "access_token": "t", "expires_in": 3600 }));
    let url = fake_idp(Arc::clone(&idp)).await;
    let manager = Arc::new(manager_with(Arc::new(StubConnectorRepo::with(vec![]))));
    let cfg = Arc::new(cc_config(&url));

    let mut handles = Vec::new();
    for _ in 0..8 {
        let manager = Arc::clone(&manager);
        let cfg = Arc::clone(&cfg);
        handles.push(tokio::spawn(async move {
            manager.access_token("crm", &cfg, true).await
        }));
    }
    for h in handles {
        h.await.expect("join").expect("token");
    }
    assert_eq!(idp.hits(), 1, "eight callers, one token request");
}

// ---- 401 invalidation cannot be dropped ----

/// An invalidation is not lost while another task holds the state lock.
///
/// An `invalidate` that tried `state.try_lock()` and skipped when another
/// task held it would skip precisely when it matters, because the task
/// holding it is the one about to publish a token. Here the lock is held
/// for the whole invalidation, so such an `invalidate` would do nothing at
/// all and the rejected token would stay fast-path eligible until its
/// refresh margin: a connector answering 401 after 401 with a good refresh
/// one call away.
#[tokio::test]
async fn invalidation_is_not_lost_when_the_state_lock_is_held() {
    let idp = IdpState::ok(json!({ "access_token": "at-1", "expires_in": 3600 }));
    let url = fake_idp(Arc::clone(&idp)).await;
    let manager = manager_with(Arc::new(StubConnectorRepo::with(vec![])));
    let cfg = cc_config(&url);

    let first = manager
        .access_token("crm", &cfg, true)
        .await
        .expect("token");
    assert_eq!(first, "at-1");
    assert_eq!(idp.hits(), 1);

    // The API answers 401 while something else owns the entry's state —
    // an acquisition for another config, a refresh, anything.
    let entry = manager.entry("crm").await;
    let held = entry.state.lock().await;
    manager.invalidate("crm").await;
    drop(held);

    idp.set_response(200, json!({ "access_token": "at-2", "expires_in": 3600 }));
    let second = manager
        .access_token("crm", &cfg, true)
        .await
        .expect("token");

    assert_eq!(
        second, "at-2",
        "the rejected token must not be served again"
    );
    assert_eq!(idp.hits(), 2, "the invalidation must force a token request");
}

/// An invalidation that lands *while* a token request is in flight rejects
/// the token that request is about to publish.
///
/// This is why the generation is read before the fetch and stamped onto
/// the result, rather than re-read after it: a fresh read would say "no
/// invalidations since I published", which is false — one arrived in the
/// window, and the token being published is exactly the one it rejected.
/// Stale-stamping is the safe direction; the next caller refetches.
#[tokio::test]
async fn an_invalidation_during_a_fetch_rejects_the_token_it_publishes() {
    let idp = IdpState::ok(json!({ "access_token": "at-1", "expires_in": 3600 }));
    let url = fake_idp(Arc::clone(&idp)).await;
    let manager = Arc::new(manager_with(Arc::new(StubConnectorRepo::with(vec![]))));
    let cfg = Arc::new(cc_config(&url));

    // Stand where the in-flight acquisition stands: the generation has
    // been read, the token not yet published.
    let entry = manager.entry("crm").await;
    let observed = entry.invalidated.load(std::sync::atomic::Ordering::Acquire);

    // The 401 arrives now, before the publish.
    manager.invalidate("crm").await;

    // …and the acquisition publishes, stamped with what it observed.
    {
        let mut st = entry.state.lock().await;
        st.generation = observed;
        st.token = Some(CachedToken {
            access_token: "at-stale".to_string(),
            expires_at: Instant::now() + Duration::from_secs(3600),
            expires_at_epoch_ms: 0,
        });
        entry.fast.store(Arc::new(FastToken {
            config: Some((*cfg).clone()),
            token: st.token.clone(),
            generation: observed,
        }));
    }

    idp.set_response(200, json!({ "access_token": "at-2", "expires_in": 3600 }));
    let next = manager
        .access_token("crm", &cfg, true)
        .await
        .expect("token");

    assert_eq!(
        next, "at-2",
        "a token published after the 401 that rejected it must not be served"
    );
    assert_eq!(idp.hits(), 1, "exactly one refetch");
}

/// The generation does not make every call refetch: with no invalidation,
/// the fast path still answers from cache.
#[tokio::test]
async fn the_generation_does_not_disturb_the_cached_fast_path() {
    let idp = IdpState::ok(json!({ "access_token": "at-1", "expires_in": 3600 }));
    let url = fake_idp(Arc::clone(&idp)).await;
    let manager = manager_with(Arc::new(StubConnectorRepo::with(vec![])));
    let cfg = cc_config(&url);

    for _ in 0..5 {
        assert_eq!(
            manager
                .access_token("crm", &cfg, true)
                .await
                .expect("token"),
            "at-1"
        );
    }
    assert_eq!(idp.hits(), 1, "five calls, one token request");
}

// ---- refresh_token: seed, rotation, persistence, adoption ----

#[tokio::test]
async fn rotation_persists_and_the_next_refresh_uses_the_new_token() {
    let idp = IdpState::ok(json!({
        "access_token": "at-1", "expires_in": 3600, "refresh_token": "rt-2"
    }));
    let url = fake_idp(Arc::clone(&idp)).await;
    let repo = Arc::new(StubConnectorRepo::with(vec![]));
    let manager = manager_with(Arc::clone(&repo) as Arc<StubConnectorRepo>);
    let cfg = rt_config(&url, "rt-1");

    manager
        .access_token("crm", &cfg, true)
        .await
        .expect("token");
    assert_eq!(
        idp.last_refresh_token.lock().expect("test").as_deref(),
        Some("rt-1"),
        "the first refresh presents the seed"
    );

    // The rotation was persisted with the fingerprint.
    let row = repo
        .get_oauth_state("crm")
        .await
        .expect("read")
        .expect("state row");
    assert_eq!(row.fingerprint, fingerprint(&cfg));
    let persisted: PersistedState = serde_json::from_str(&row.state_json).expect("state json");
    assert_eq!(persisted.refresh_token, "rt-2");
    assert_eq!(persisted.access_token, "at-1");

    // Invalidate the access token: the next refresh presents the
    // *rotated* token, not the burned seed.
    manager.invalidate("crm").await;
    idp.set_response(
        200,
        json!({ "access_token": "at-2", "expires_in": 3600, "refresh_token": "rt-3" }),
    );
    manager
        .access_token("crm", &cfg, true)
        .await
        .expect("token");
    assert_eq!(
        idp.last_refresh_token.lock().expect("test").as_deref(),
        Some("rt-2")
    );
}

/// A fresh process (or another node) adopts persisted state instead of
/// spending a refresh — zero IdP traffic.
#[tokio::test]
async fn persisted_state_is_adopted_without_touching_the_idp() {
    let idp = IdpState::ok(json!({ "access_token": "never", "expires_in": 3600 }));
    let url = fake_idp(Arc::clone(&idp)).await;
    let repo = Arc::new(StubConnectorRepo::with(vec![]));
    let cfg = rt_config(&url, "rt-seed");
    let state = PersistedState {
        access_token: "adopted".to_string(),
        expires_at_epoch_ms: epoch_ms_now() + 3_600_000,
        refresh_token: "rt-live".to_string(),
    };
    repo.put_oauth_state(
        "crm",
        &fingerprint(&cfg),
        &serde_json::to_string(&state).expect("json"),
    )
    .await
    .expect("seed state");

    let manager = manager_with(Arc::clone(&repo) as Arc<StubConnectorRepo>);
    let token = manager
        .access_token("crm", &cfg, true)
        .await
        .expect("token");
    assert_eq!(token, "adopted");
    assert_eq!(idp.hits(), 0, "adoption costs no token request");
}

/// Editing the connector discards state minted under the old config —
/// which is exactly the burned-token recovery story: re-seed, refresh.
#[tokio::test]
async fn a_fingerprint_mismatch_falls_back_to_the_seed() {
    let idp = IdpState::ok(json!({
        "access_token": "at", "expires_in": 3600, "refresh_token": "rt-next"
    }));
    let url = fake_idp(Arc::clone(&idp)).await;
    let repo = Arc::new(StubConnectorRepo::with(vec![]));
    let stale = PersistedState {
        access_token: "stale".to_string(),
        expires_at_epoch_ms: epoch_ms_now() + 3_600_000,
        refresh_token: "rt-stale".to_string(),
    };
    repo.put_oauth_state(
        "crm",
        "an-old-fingerprint",
        &serde_json::to_string(&stale).expect("json"),
    )
    .await
    .expect("seed state");

    let manager = manager_with(Arc::clone(&repo) as Arc<StubConnectorRepo>);
    let cfg = rt_config(&url, "rt-reseeded");
    manager
        .access_token("crm", &cfg, true)
        .await
        .expect("token");
    assert_eq!(
        idp.last_refresh_token.lock().expect("test").as_deref(),
        Some("rt-reseeded"),
        "stale state must lose to the freshly-seeded config"
    );
}

// ---- failure taxonomy ----

/// `invalid_grant` is non-retryable and negative-cached: a burned token
/// is never retry-looped against the IdP.
#[tokio::test]
async fn a_rejection_is_negative_cached_and_non_retryable() {
    let idp = IdpState::ok(json!({})); // replaced below
    idp.set_response(
        400,
        json!({ "error": "invalid_grant", "error_description": "revoked" }),
    );
    let url = fake_idp(Arc::clone(&idp)).await;
    let manager = manager_with(Arc::new(StubConnectorRepo::with(vec![])));
    let cfg = rt_config(&url, "rt-burned");

    let err = manager
        .access_token("crm", &cfg, true)
        .await
        .expect_err("burned token");
    assert!(!err.retryable(), "{err}");
    assert!(err.to_string().contains("invalid_grant"), "{err}");
    assert!(err.to_string().contains("re-seed"), "{err}");
    assert!(
        !err.to_string().contains("revoked"),
        "the free-text description is logged, never surfaced: {err}"
    );

    for _ in 0..3 {
        let err = manager
            .access_token("crm", &cfg, true)
            .await
            .expect_err("still cached");
        assert!(!err.retryable(), "{err}");
    }
    assert_eq!(idp.hits(), 1, "the rejection is answered from memory");
}

#[tokio::test]
async fn a_server_error_is_retryable_and_not_cached() {
    let idp = IdpState::ok(json!({}));
    idp.set_response(503, json!({ "error": "temporarily_unavailable" }));
    let url = fake_idp(Arc::clone(&idp)).await;
    let manager = manager_with(Arc::new(StubConnectorRepo::with(vec![])));
    let cfg = cc_config(&url);

    for _ in 0..2 {
        let err = manager
            .access_token("crm", &cfg, true)
            .await
            .expect_err("5xx");
        assert!(err.retryable(), "{err}");
    }
    assert_eq!(idp.hits(), 2, "transport failures are not negative-cached");
}

#[tokio::test]
async fn config_mistakes_are_named_without_a_request() {
    let manager = manager_with(Arc::new(StubConnectorRepo::with(vec![])));
    let mut cfg = cc_config("http://127.0.0.1:9/token");
    cfg.grant = "password".to_string();
    let err = manager
        .access_token("crm", &cfg, true)
        .await
        .expect_err("ROPC is not a thing here");
    assert!(err.to_string().contains("password"), "{err}");
    assert!(err.to_string().contains(OAuth2Grant::VALUES), "{err}");

    let cfg = OAuth2Config {
        refresh_token: None,
        ..rt_config("http://127.0.0.1:9/token", "x")
    };
    let err = manager
        .access_token("crm", &cfg, true)
        .await
        .expect_err("no seed");
    assert!(err.to_string().contains("refresh_token"), "{err}");
}

/// An uninitialised manager (a bare registry in a unit test) refuses
/// clearly instead of panicking.
#[tokio::test]
async fn an_uninitialised_manager_refuses_cleanly() {
    let manager = OAuthTokenManager::new();
    let err = manager
        .access_token("crm", &cc_config("http://x/token"), true)
        .await
        .expect_err("no deps");
    assert!(err.to_string().contains("not initialised"), "{err}");
    assert!(!err.retryable());
}

// ---- effective_auth: the seam the request paths use ----

#[tokio::test]
async fn effective_auth_passes_static_variants_through() {
    let manager = OAuthTokenManager::new();
    let http = crate::connector::HttpConnectorConfig {
        auth: Some(AuthConfig::Bearer {
            token: "static".to_string(),
        }),
        ..http_config_base()
    };
    let auth = effective_auth(&manager, "crm", &http)
        .await
        .expect("static auth needs no runtime");
    assert!(matches!(
        auth.as_deref(),
        Some(AuthConfig::Bearer { token }) if token == "static"
    ));

    let no_auth = crate::connector::HttpConnectorConfig {
        auth: None,
        ..http_config_base()
    };
    assert!(
        effective_auth(&manager, "crm", &no_auth)
            .await
            .expect("no auth")
            .is_none()
    );
}

#[tokio::test]
async fn effective_auth_resolves_oauth2_to_a_bearer() {
    let idp = IdpState::ok(json!({ "access_token": "resolved", "expires_in": 3600 }));
    let url = fake_idp(Arc::clone(&idp)).await;
    let manager = manager_with(Arc::new(StubConnectorRepo::with(vec![])));
    let http = crate::connector::HttpConnectorConfig {
        auth: Some(AuthConfig::OAuth2(Box::new(cc_config(&url)))),
        allow_private_urls: true,
        ..http_config_base()
    };
    let auth = effective_auth(&manager, "crm", &http)
        .await
        .expect("resolves");
    assert!(matches!(
        auth.as_deref(),
        Some(AuthConfig::Bearer { token }) if token == "resolved"
    ));
}

fn http_config_base() -> crate::connector::HttpConnectorConfig {
    serde_json::from_value(json!({
        "url": "https://api.example.com",
        "method": "GET"
    }))
    .expect("base http config")
}

/// A token endpoint that streams without declaring a length cannot make
/// this buffer past `MAX_TOKEN_RESPONSE_BYTES`.
///
/// The declared-length check was already here and is no cap on its own: a
/// chunked response skips it entirely, and the check that followed ran on
/// a body `bytes()` had already read to its end. This path also runs on
/// every token refresh for every OAuth2 connector, so the flood is
/// repeatable on a schedule Orion sets itself.
#[tokio::test]
async fn a_flooding_token_endpoint_is_cut_off_at_the_cap() {
    const CHUNK: usize = 64 * 1024;
    const CHUNKS: usize = 128; // 8 MiB against a 64 KiB cap
    let (url, server) = crate::http_body::flood_server(CHUNK, CHUNKS).await;

    let err = request_token_inner(
        &reqwest::Client::new(),
        TokenEndpoint {
            token_url: &url,
            client_id: "cid",
            client_secret: "csecret",
            client_auth: "basic",
        },
        // The flood server is on loopback; the SSRF check would refuse it
        // before a body was read.
        true,
        vec![("grant_type", "client_credentials".to_string())],
    )
    .await
    .expect_err("must refuse an oversized token response");

    assert!(
        matches!(err, OAuthError::Transport(ref m) if m.contains("token response")),
        "unexpected error: {err:?}"
    );
    crate::http_body::assert_stopped_early(
        server.await.expect("test server"),
        CHUNK * CHUNKS,
        "the token request",
    );
}
