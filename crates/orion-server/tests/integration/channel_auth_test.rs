//! Per-channel authentication on the HTTP data plane, end to end.
//!
//! `admin_auth` covers `/api/v1/admin` and nothing else, so a data channel was
//! reachable by anyone who could reach the port. The unit tests in
//! `src/channel/auth.rs` cover the credential comparisons themselves; these
//! cover the parts only a whole server can show — that the guard is actually
//! wired into every HTTP ingress, that an unauthenticated caller is stopped
//! before the guards behind it, and that a channel with no `auth` key is
//! untouched.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use tower::ServiceExt;

use crate::common;
use crate::common::{body_json, json_request};

type HmacSha256 = Hmac<Sha256>;

fn api_key_config(key: &str) -> Value {
    json!({ "auth": { "mode": "api_key", "keys": [key], "header": "X-API-Key" } })
}

/// A request carrying an explicit header, which `json_request` does not build.
fn request_with_header(uri: &str, header: (&str, &str), body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header(header.0, header.1)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

#[tokio::test]
async fn a_request_without_a_key_is_refused() {
    let app = common::test_app().await;
    common::create_and_activate_channel_with_config(
        &app,
        "guarded",
        common::echo_workflow("guarded-wf"),
        api_key_config("s3cret"),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/guarded",
            Some(json!({"data": {"x": 1}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_request_with_the_wrong_key_is_refused() {
    let app = common::test_app().await;
    common::create_and_activate_channel_with_config(
        &app,
        "guarded2",
        common::echo_workflow("guarded2-wf"),
        api_key_config("s3cret"),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(request_with_header(
            "/api/v1/data/guarded2",
            ("X-API-Key", "wrong"),
            json!({"data": {"x": 1}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_request_with_the_right_key_is_served() {
    let app = common::test_app().await;
    common::create_and_activate_channel_with_config(
        &app,
        "guarded3",
        common::echo_workflow("guarded3-wf"),
        api_key_config("s3cret"),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(request_with_header(
            "/api/v1/data/guarded3",
            ("X-API-Key", "s3cret"),
            json!({"data": {"x": 1}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
}

/// `/async` must not be a way around the channel's authentication.
///
/// This is S1 applied to the guard that can least afford a gap: a channel that
/// refuses anonymous callers on `POST /orders` but accepts them on
/// `POST /orders/async` is not authenticated.
#[tokio::test]
async fn the_async_submission_path_is_authenticated_too() {
    let app = common::test_app().await;
    common::create_and_activate_channel_with_config(
        &app,
        "guarded-async",
        common::echo_workflow("guarded-async-wf"),
        api_key_config("s3cret"),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/guarded-async/async",
            Some(json!({"data": {"x": 1}})),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "an unauthenticated /async submission must not be queued"
    );

    let resp = app
        .clone()
        .oneshot(request_with_header(
            "/api/v1/data/guarded-async/async",
            ("X-API-Key", "s3cret"),
            json!({"data": {"x": 1}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

/// An authenticated channel cannot be probed through its idempotency window.
///
/// If the auth guard ran after deduplication, an anonymous caller could claim
/// a key belonging to a real caller and have the genuine request answered
/// `409`. The `401` here is what says the ordering holds.
#[tokio::test]
async fn an_unauthenticated_caller_cannot_claim_an_idempotency_key() {
    let app = common::test_app().await;
    let config = json!({
        "auth": { "mode": "api_key", "keys": ["s3cret"], "header": "X-API-Key" },
        "deduplication": { "header": "Idempotency-Key", "window_secs": 300 }
    });
    common::create_and_activate_channel_with_config(
        &app,
        "guarded-dedup",
        common::echo_workflow("guarded-dedup-wf"),
        config,
    )
    .await;

    // Anonymous, carrying a key the real caller is about to use.
    let resp = app
        .clone()
        .oneshot(common::post_with_idempotency_key(
            "/api/v1/data/guarded-dedup",
            "token-1",
            json!({"data": {"x": 1}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // The genuine caller still gets served: the key was never claimed.
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/data/guarded-dedup")
        .header("content-type", "application/json")
        .header("X-API-Key", "s3cret")
        .header("Idempotency-Key", "token-1")
        .body(Body::from(
            serde_json::to_vec(&json!({"data": {"x": 1}})).unwrap(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the refused caller must not have burned the real caller's key"
    );
}

/// The webhook case the README advertises and Orion could not previously do:
/// a GitHub-format `sha256=<hex>` signature over the raw body.
#[tokio::test]
async fn an_hmac_signed_webhook_is_verified_against_the_raw_body() {
    let app = common::test_app().await;
    let config = json!({
        "auth": {
            "mode": "hmac",
            "secret": "whsec_test",
            "header": "X-Hub-Signature-256",
            "signature_prefix": "sha256="
        }
    });
    common::create_and_activate_channel_with_config(
        &app,
        "webhook",
        common::echo_workflow("webhook-wf"),
        config,
    )
    .await;

    // The exact bytes matter: the signature is over the wire body, so the test
    // signs and sends the same serialization rather than two equal values.
    let body = serde_json::to_vec(&json!({"action": "opened", "number": 42})).unwrap();
    let mut mac = HmacSha256::new_from_slice(b"whsec_test").unwrap();
    mac.update(&body);
    let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

    let signed = |sig: &str, payload: &[u8]| {
        Request::builder()
            .method("POST")
            .uri("/api/v1/data/webhook")
            .header("content-type", "application/json")
            .header("X-Hub-Signature-256", sig)
            .body(Body::from(payload.to_vec()))
            .unwrap()
    };

    let resp = app
        .clone()
        .oneshot(signed(&signature, &body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "a correctly signed webhook");

    // One byte different in the body, same signature.
    let tampered = serde_json::to_vec(&json!({"action": "opened", "number": 43})).unwrap();
    let resp = app
        .clone()
        .oneshot(signed(&signature, &tampered))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "a tampered body must not verify against the original signature"
    );

    let resp = app
        .clone()
        .oneshot(signed("sha256=00", &body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// An `env://` secret is resolved at channel load, so the credential never has
/// to sit in the stored config.
#[tokio::test]
async fn an_api_key_can_come_from_the_environment() {
    // SAFETY: single-threaded test setup before the app is built.
    unsafe { std::env::set_var("ORION_TEST_CHANNEL_KEY", "from-env") };

    let app = common::test_app().await;
    common::create_and_activate_channel_with_config(
        &app,
        "env-guarded",
        common::echo_workflow("env-guarded-wf"),
        json!({ "auth": {
            "mode": "api_key",
            "keys": ["env://ORION_TEST_CHANNEL_KEY"],
            "header": "X-API-Key"
        }}),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(request_with_header(
            "/api/v1/data/env-guarded",
            ("X-API-Key", "from-env"),
            json!({"data": {}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The literal reference must not be accepted as the key itself.
    let resp = app
        .clone()
        .oneshot(request_with_header(
            "/api/v1/data/env-guarded",
            ("X-API-Key", "env://ORION_TEST_CHANNEL_KEY"),
            json!({"data": {}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    unsafe { std::env::remove_var("ORION_TEST_CHANNEL_KEY") };
}

/// A channel with no `auth` key is unauthenticated, exactly as before.
///
/// This is the default every stored channel already has, so it is the test that
/// says the feature costs existing deployments nothing.
#[tokio::test]
async fn a_channel_without_auth_is_unchanged() {
    let app = common::test_app().await;
    common::create_and_activate_channel(&app, "open", common::echo_workflow("open-wf")).await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/open",
            Some(json!({"data": {"x": 1}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// An `auth` block that cannot be compiled quarantines the channel rather than
/// serving it unauthenticated.
///
/// Loading it with the guard silently absent is the worst reading of the
/// operator's intent — they asked for authentication and would get none — so
/// this follows the N3/N4 posture and refuses at every ingress instead.
#[tokio::test]
async fn a_channel_whose_auth_cannot_be_built_is_quarantined() {
    let app = common::test_app().await;
    common::create_and_activate_channel_with_config(
        &app,
        "broken-auth",
        common::echo_workflow("broken-auth-wf"),
        json!({ "auth": {
            "mode": "api_key",
            "keys": ["env://ORION_TEST_DEFINITELY_UNSET_KEY"]
        }}),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/broken-auth",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::OK,
        "a channel whose auth failed to build must not serve traffic"
    );
}

/// H3: channel reads mask `auth.keys` / `auth.secret`, and the masked shape
/// round-trips through PUT without corrupting the live credential — the same
/// F34 cycle connectors have always had, proven end to end against the data
/// plane: after a GET → edit → PUT, the original key still authenticates.
#[tokio::test]
async fn channel_auth_keys_are_masked_on_read_and_survive_a_put_round_trip() {
    let app = common::test_app().await;
    let (channel_id, _wf) = common::create_and_activate_channel_full(
        &app,
        "masked-rt",
        common::echo_workflow("masked-rt-wf"),
        api_key_config("sk-live-9"),
    )
    .await;

    // GET masks the key.
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/channels/{channel_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(
        body["data"]["config"]["auth"]["keys"][0], "******",
        "the admin read must not return the literal key: {body}"
    );

    // A new draft, edited from the masked GET shape, PUT back verbatim.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/admin/channels/{channel_id}/versions"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let mut config = body["data"]["config"].clone();
    config["rate_limit"] = json!({"requests_per_second": 50});
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/channels/{channel_id}"),
            Some(json!({"config": config})),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "{}",
        common::body_json(resp).await
    );
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{channel_id}/status"),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The ORIGINAL key still authenticates: the PUT restored it rather than
    // persisting the sentinel as the credential.
    let resp = app
        .clone()
        .oneshot(request_with_header(
            "/api/v1/data/masked-rt",
            ("x-api-key", "sk-live-9"),
            json!({"data": {"ok": true}}),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the real key must survive the masked round-trip"
    );

    // And the sentinel itself is not a working credential.
    let resp = app
        .clone()
        .oneshot(request_with_header(
            "/api/v1/data/masked-rt",
            ("x-api-key", "******"),
            json!({"data": {"ok": true}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// A create carrying the sentinel has nothing to restore from — it is a
/// copied-from-a-GET mistake and must be refused, not persisted as the key.
#[tokio::test]
async fn channel_create_rejects_the_mask_sentinel() {
    let app = common::test_app().await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "mask-reject",
                "channel_type": "sync",
                "protocol": "http",
                "methods": ["POST"],
                "route_pattern": "/mask-reject",
                "workflow_id": "any-wf",
                "config": {"auth": {"mode": "api_key", "keys": ["******"]}}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = common::body_json(resp).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("masked placeholder"),
        "{body}"
    );
}

// ============================================================
// #264: activation-time auth validation + generalized HMAC config
// ============================================================

#[tokio::test]
async fn broken_auth_configs_are_refused_at_create_not_quarantined() {
    let app = common::test_app().await;

    for (auth, expected) in [
        // Each of these was previously accepted and only failed at engine
        // reload, taking the channel into quarantine.
        (json!({"mode": "api_key"}), "auth.keys"),
        (json!({"mode": "hmac"}), "auth.secret"),
        (
            json!({"mode": "hmac", "secret": "s", "preset": "gitlab"}),
            "preset",
        ),
        (
            json!({"mode": "hmac", "secret": "s", "message": "v0:{ts}:{body}"}),
            "placeholder",
        ),
        (
            json!({"mode": "hmac", "secret": "s", "tolerance_secs": 300}),
            "auth.timestamp",
        ),
        (
            json!({"mode": "hmac", "secret": "s",
                   "signature_prefix": "v0=", "signature_key": "v1"}),
            "mutually exclusive",
        ),
        // #331: a scheme is a name. This one could never match one, and used
        // to refuse every caller while every offline check passed.
        (
            json!({"mode": "api_key", "keys": ["k"], "header": "X-Key", "scheme": "Key="}),
            "auth.scheme",
        ),
    ] {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/admin/channels",
                Some(json!({
                    "name": "bad-auth-channel",
                    "channel_type": "sync",
                    "protocol": "rest",
                    "route_pattern": "/hooks/bad",
                    "methods": ["POST"],
                    "workflow_id": "wf-x",
                    "config": {"auth": auth}
                })),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "expected 400 for {auth}"
        );
        let body = body_json(resp).await;
        assert!(
            body["error"].to_string().contains(expected),
            "{auth} should have reported '{expected}', got {}",
            body["error"]
        );
    }

    // A preset config with an env:// secret is structurally fine and must be
    // accepted even though the variable is unset on this host — resolution
    // stays load-time so bundles validate anywhere.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "zoom-hooks",
                "channel_type": "sync",
                "protocol": "rest",
                "route_pattern": "/hooks/zoom",
                "methods": ["POST"],
                "workflow_id": "wf-x",
                "config": {"auth": {"mode": "hmac", "preset": "zoom",
                                     "secret": "env://UNSET_ZOOM_SECRET_264"}}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

// ============================================================
// #267: the jwt mode, end to end
// ============================================================

const JWT_SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn jwt_channel_config(extra: Value) -> Value {
    let mut auth = json!({
        "mode": "jwt",
        "algorithms": ["HS256"],
        "jwt_keys": [{"algorithm": "HS256", "key": JWT_SECRET}],
    });
    if let (Some(auth_obj), Some(extra_obj)) = (auth.as_object_mut(), extra.as_object()) {
        for (k, v) in extra_obj {
            auth_obj.insert(k.clone(), v.clone());
        }
    }
    json!({ "auth": auth })
}

fn mint_jwt(claims: Value) -> String {
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(JWT_SECRET.as_bytes()),
    )
    .expect("test")
}

fn fresh_claims() -> Value {
    json!({
        "sub": "user-42",
        "roles": ["teacher"],
        "exp": chrono::Utc::now().timestamp() + 3600,
    })
}

/// A workflow that copies the verified identity out of metadata, proving the
/// claims actually reach workflow logic — the whole point of the mode.
fn claims_echo_workflow(id: &str) -> Value {
    json!({
        "workflow_id": id, "name": id, "condition": true,
        "tasks": [{
            "id": "t1", "name": "copy claim",
            "function": {"name": "map", "input": {"mappings": [
                {"path": "data.whoami", "logic": {"var": "metadata.auth.claims.sub"}}
            ]}}
        }]
    })
}

#[tokio::test]
async fn a_verified_jwt_exposes_claims_to_the_workflow() {
    let app = common::test_app().await;
    common::create_and_activate_channel_with_config(
        &app,
        "me",
        claims_echo_workflow("me-wf"),
        jwt_channel_config(json!({})),
    )
    .await;

    // No token → 401 with the RFC 6750 challenge.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/me",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    // No credential at all: RFC 6750 §3.1's bare challenge, no error code.
    assert_eq!(challenge_of(&resp), "Bearer");

    // A valid token → the workflow reads claims.sub from its context.
    let token = mint_jwt(fresh_claims());
    let resp = app
        .clone()
        .oneshot(request_with_header(
            "/api/v1/data/me",
            ("Authorization", &format!("Bearer {token}")),
            json!({"data": {}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["whoami"], "user-42");

    // An expired token → 401 whose challenge names expiry — the one hinted
    // cause — so clients know to refresh.
    let mut expired = fresh_claims();
    expired["exp"] = json!(chrono::Utc::now().timestamp() - 3600);
    let resp = app
        .clone()
        .oneshot(request_with_header(
            "/api/v1/data/me",
            ("Authorization", &format!("Bearer {}", mint_jwt(expired))),
            json!({"data": {}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let challenge = resp
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(challenge.contains("token expired"), "{challenge}");
}

#[tokio::test]
async fn authorization_logic_answers_403_for_verified_but_insufficient_claims() {
    let app = common::test_app().await;
    common::create_and_activate_channel_with_config(
        &app,
        "admin-only",
        claims_echo_workflow("admin-wf"),
        jwt_channel_config(json!({
            "authorization_logic": {"in": ["admin", {"var": "claims.roles"}]}
        })),
    )
    .await;

    // Verified teacher → 403, not 401: the identity held, the rights did not.
    let token = mint_jwt(fresh_claims());
    let resp = app
        .clone()
        .oneshot(request_with_header(
            "/api/v1/data/admin-only",
            ("Authorization", &format!("Bearer {token}")),
            json!({"data": {}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let mut admin = fresh_claims();
    admin["roles"] = json!(["admin"]);
    let resp = app
        .clone()
        .oneshot(request_with_header(
            "/api/v1/data/admin-only",
            ("Authorization", &format!("Bearer {}", mint_jwt(admin))),
            json!({"data": {}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn optional_jwt_admits_tokenless_and_still_rejects_invalid() {
    let app = common::test_app().await;
    common::create_and_activate_channel_with_config(
        &app,
        "maybe-auth",
        claims_echo_workflow("maybe-wf"),
        jwt_channel_config(json!({"required": false})),
    )
    .await;

    // No token: served, with no identity in context (the mapping writes null).
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/maybe-auth",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["data"]["whoami"].is_null());

    // Garbage token: still 401 — optional never means invalid passes.
    let resp = app
        .clone()
        .oneshot(request_with_header(
            "/api/v1/data/maybe-auth",
            ("Authorization", "Bearer not.a.token"),
            json!({"data": {}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

fn challenge_of(resp: &axum::response::Response) -> String {
    resp.headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// #331 over the wire. RFC 9110 §11.1 makes the scheme case-insensitive and
/// lets any run of spaces separate it from the credential; the channel used to
/// strip a byte-exact `"Bearer "` and refuse all three of the spellings below.
/// The jwt channel is the issue's reproduction — `"scheme": "Bearer"`, no
/// trailing space, which refused every caller.
#[tokio::test]
async fn conforming_authorization_headers_are_accepted() {
    let app = common::test_app().await;
    common::create_and_activate_channel_with_config(
        &app,
        "bearer-key",
        common::echo_workflow("bearer-key-wf"),
        json!({ "auth": { "mode": "api_key", "keys": ["s3cret"] } }),
    )
    .await;
    common::create_and_activate_channel_with_config(
        &app,
        "bearer-jwt",
        claims_echo_workflow("bearer-jwt-wf"),
        jwt_channel_config(json!({
            "source": { "header": "Authorization", "scheme": "Bearer" }
        })),
    )
    .await;
    let token = mint_jwt(fresh_claims());

    for (channel, credential) in [("bearer-key", "s3cret"), ("bearer-jwt", token.as_str())] {
        let uri = format!("/api/v1/data/{channel}");
        for spelling in ["Bearer", "bearer", "BEARER", "Bearer "] {
            let resp = app
                .clone()
                .oneshot(request_with_header(
                    &uri,
                    ("Authorization", &format!("{spelling} {credential}")),
                    json!({"data": {}}),
                ))
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "{channel}: {spelling:?} <credential>"
            );
        }
        // No separator is not the scheme.
        let resp = app
            .clone()
            .oneshot(request_with_header(
                &uri,
                ("Authorization", &format!("Bearer{credential}")),
                json!({"data": {}}),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{channel}: Bearer<credential>"
        );
    }
}

/// What a refused bearer presentation says on the wire: the bare challenge
/// when no bearer credential was presented (RFC 6750 §3.1), `invalid_token`
/// when one was — and the body the same either way.
#[tokio::test]
async fn a_foreign_scheme_gets_the_bare_challenge_and_a_bad_token_does_not() {
    let app = common::test_app().await;
    common::create_and_activate_channel_with_config(
        &app,
        "challenged",
        claims_echo_workflow("challenged-wf"),
        jwt_channel_config(json!({})),
    )
    .await;

    let mut bodies = Vec::new();
    for (value, want) in [
        ("Basic dXNlcjpwYXNz", "Bearer"),
        ("Bearer not.a.token", "Bearer error=\"invalid_token\""),
    ] {
        let resp = app
            .clone()
            .oneshot(request_with_header(
                "/api/v1/data/challenged",
                ("Authorization", value),
                json!({"data": {}}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{value}");
        assert_eq!(challenge_of(&resp), want, "{value}");
        let body = body_json(resp).await;
        bodies.push(body["error"]["message"].clone());
    }
    assert_eq!(bodies[0], bodies[1], "the body never names the cause");
}

#[tokio::test]
async fn broken_jwt_configs_are_refused_at_create() {
    let app = common::test_app().await;
    for (auth, expected) in [
        (json!({"mode": "jwt"}), "algorithms"),
        (
            json!({"mode": "jwt", "algorithms": ["HS256"]}),
            "jwt_keys and/or auth.jwks_url",
        ),
        (
            json!({"mode": "jwt", "algorithms": ["ES512"],
                   "jwt_keys": [{"algorithm": "HS256", "key": "k"}]}),
            "ES512",
        ),
        (
            json!({"mode": "jwt", "algorithms": ["RS256"],
                   "jwks_url": "http://issuer.example.com/jwks"}),
            "HTTPS",
        ),
        (
            json!({"mode": "jwt", "algorithms": ["HS256"],
                   "jwt_keys": [{"algorithm": "HS256", "key": "k"}],
                   "source": {"header": "Authorization", "scheme": "Bearer:"}}),
            "auth.source.scheme",
        ),
    ] {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/admin/channels",
                Some(json!({
                    "name": "bad-jwt",
                    "channel_type": "sync",
                    "protocol": "rest",
                    "route_pattern": "/hooks/jwt",
                    "methods": ["POST"],
                    "workflow_id": "wf-x",
                    "config": {"auth": auth}
                })),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "expected 400 for {auth}"
        );
        let body = body_json(resp).await;
        assert!(
            body["error"].to_string().contains(expected),
            "{auth} should have reported '{expected}', got {}",
            body["error"]
        );
    }
}

/// S12 on the data plane: a channel's own `auth.keys` must not face unlimited
/// online guessing.
///
/// The admin API key has had a failed-attempt budget since S12. A channel's
/// key had none — and it is the *public* credential, on a port anyone who
/// knows a channel name can reach, behind a rate limit that is off by default.
/// After the grace period the guard refuses without even comparing, so the
/// attacker's rate collapses to the backoff rather than the request rate.
///
/// The refusal is byte-identical to a wrong key: a caller must not be able to
/// tell from the response that it is being throttled on credentials, or it
/// learns when to pause.
#[tokio::test]
async fn repeated_wrong_keys_put_a_client_into_backoff() {
    let app = common::test_app().await;
    common::create_and_activate_channel_with_config(
        &app,
        "guessable",
        common::echo_workflow("guessable-wf"),
        api_key_config("the-real-key"),
    )
    .await;

    let wrong = |app: axum::Router| async move {
        app.oneshot(request_with_header(
            "/api/v1/data/guessable",
            ("X-API-Key", "wrong"),
            json!({"data": {"x": 1}}),
        ))
        .await
        .unwrap()
    };

    // The grace period: a human fat-fingering a key is not an attacker.
    for attempt in 0..5 {
        let resp = wrong(app.clone()).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "attempt {attempt} must be refused"
        );
    }

    // Past it, the *correct* key is refused too — the client is locked out,
    // not the credential rejected. This is the assertion that fails without
    // the budget: before it, a right key always worked no matter how many
    // wrong ones preceded it.
    let resp = app
        .clone()
        .oneshot(request_with_header(
            "/api/v1/data/guessable",
            ("X-API-Key", "the-real-key"),
            json!({"data": {"x": 1}}),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "a locked-out client is refused even with the right key"
    );
    let body = body_json(resp).await;
    assert_eq!(
        body["error"]["message"], "Channel authentication failed",
        "the lockout must be indistinguishable from a wrong key: {body}"
    );
}

/// The budget is per `(channel, client)`. A shared egress address is the norm
/// on the data plane, so one misconfigured integration must not lock its whole
/// NAT out of every other channel.
#[tokio::test]
async fn a_lockout_on_one_channel_does_not_reach_another() {
    let app = common::test_app().await;
    for name in ["ch-a", "ch-b"] {
        common::create_and_activate_channel_with_config(
            &app,
            name,
            common::echo_workflow(&format!("{name}-wf")),
            api_key_config("shared-key"),
        )
        .await;
    }

    for _ in 0..8 {
        let _ = app
            .clone()
            .oneshot(request_with_header(
                "/api/v1/data/ch-a",
                ("X-API-Key", "wrong"),
                json!({"data": {"x": 1}}),
            ))
            .await
            .unwrap();
    }

    let resp = app
        .clone()
        .oneshot(request_with_header(
            "/api/v1/data/ch-b",
            ("X-API-Key", "shared-key"),
            json!({"data": {"x": 1}}),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "guessing at ch-a must not lock this client out of ch-b"
    );
}

/// #354: `metadata.auth` is platform-reserved. A caller's envelope used to
/// survive wherever no claims were merged over it — a channel with no `auth`,
/// a party-level mode, and a `jwt` channel with `required: false` called
/// without a token — so a workflow read `metadata.auth.claims.sub` as a
/// verified identity the caller had simply written.
#[tokio::test]
async fn a_caller_cannot_supply_metadata_auth() {
    let app = common::test_app().await;
    common::create_and_activate_channel_with_config(
        &app,
        "forge-optional",
        claims_echo_workflow("forge-optional-wf"),
        jwt_channel_config(json!({"required": false})),
    )
    .await;
    common::create_and_activate_channel_with_config(
        &app,
        "forge-open",
        claims_echo_workflow("forge-open-wf"),
        json!({}),
    )
    .await;
    common::create_and_activate_channel_with_config(
        &app,
        "forge-key",
        claims_echo_workflow("forge-key-wf"),
        api_key_config("k1"),
    )
    .await;

    let forged = json!({"data": {}, "metadata": {"auth": {"claims": {"sub": "admin"}}}});
    for uri in [
        "/api/v1/data/forge-optional",
        "/api/v1/data/forge-open",
        "/api/v1/data/forge-open/async",
    ] {
        let resp = app
            .clone()
            .oneshot(json_request("POST", uri, Some(forged.clone())))
            .await
            .unwrap();
        assert!(resp.status().is_success(), "{uri}: {}", resp.status());
        if uri.ends_with("/async") {
            continue;
        }
        let body = body_json(resp).await;
        assert!(
            body["data"]["whoami"].is_null(),
            "{uri}: a forged envelope identity reached the workflow: {body}"
        );
    }
    let resp = app
        .clone()
        .oneshot(request_with_header(
            "/api/v1/data/forge-key",
            ("X-API-Key", "k1"),
            forged.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_json(resp).await["data"]["whoami"].is_null());

    // A verified token still wins, and the forged object does not merge
    // into it.
    let token = mint_jwt(json!({"sub": "alice", "exp": 4_102_444_800u64}));
    let resp = app
        .clone()
        .oneshot(request_with_header(
            "/api/v1/data/forge-optional",
            ("Authorization", &format!("Bearer {token}")),
            forged,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"]["whoami"], "alice");
}

/// #354: the response cache keys on the verified subject. `key_logic` used to
/// be evaluated over the metadata *before* the claims merge, so a key on
/// `metadata.auth.claims.sub` read the caller's envelope: bob, holding a valid
/// token of his own, could store his body under alice's key and alice was
/// then served it.
#[tokio::test]
async fn the_response_cache_keys_on_the_verified_subject() {
    let app = common::test_app().await;
    let mut config = jwt_channel_config(json!({}));
    config["cache"] = json!({
        "enabled": true,
        "ttl_secs": 60,
        "key_logic": {"var": "metadata.auth.claims.sub"}
    });
    let workflow = json!({
        "workflow_id": "per-user-wf", "name": "per-user-wf", "condition": true,
        "tasks": [{
            "id": "parse", "name": "parse",
            "function": {"name": "parse_json", "input": {"source": "payload", "target": "input"}}
        }, {
            "id": "t1", "name": "who and what",
            "function": {"name": "map", "input": {"mappings": [
                {"path": "data.whoami", "logic": {"var": "metadata.auth.claims.sub"}},
                {"path": "data.seen", "logic": {"var": "data.input.n"}}
            ]}}
        }]
    });
    common::create_and_activate_channel_with_config(&app, "per-user", workflow, config).await;
    let alice = mint_jwt(json!({"sub": "alice", "exp": 4_102_444_800u64}));
    let bob = mint_jwt(json!({"sub": "bob", "exp": 4_102_444_800u64}));

    let call = |token: &str, body: Value| {
        let app = app.clone();
        let auth = format!("Bearer {token}");
        async move {
            let resp = app
                .oneshot(request_with_header(
                    "/api/v1/data/per-user",
                    ("Authorization", &auth),
                    body,
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let body = body_json(resp).await;
            (body["data"]["whoami"].clone(), body["data"]["seen"].clone())
        }
    };
    let spoof =
        |n: u64| json!({"data": {"n": n}, "metadata": {"auth": {"claims": {"sub": "alice"}}}});

    // bob claims to be alice in the envelope: his entry is keyed on "bob".
    assert_eq!(call(&bob, spoof(1)).await, (json!("bob"), json!(1)));
    // alice is not served bob's body; her own run is stored under "alice".
    assert_eq!(call(&alice, spoof(2)).await, (json!("alice"), json!(2)));
    // The key is the subject alone, so a different payload is a hit on each
    // subject's own entry.
    assert_eq!(
        call(&bob, json!({"data": {"n": 3}})).await,
        (json!("bob"), json!(1))
    );
    assert_eq!(
        call(&alice, json!({"data": {"n": 4}})).await,
        (json!("alice"), json!(2))
    );
}
