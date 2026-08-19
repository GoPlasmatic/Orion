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
