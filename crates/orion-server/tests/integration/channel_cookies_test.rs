//! #270: `config.request.cookies_to_metadata` — named request cookies in the
//! workflow context.
//!
//! `cookie` is one of the four `CREDENTIAL_HEADERS` masked before request
//! metadata is built, and that masking is right: the metadata map is persisted
//! verbatim into `traces.result_json` and `trace_dlq.metadata_json`, so a
//! plaintext value there is a plaintext credential at rest. The consequence was
//! absolute, though — **no** cookie value could reach a workflow by any route,
//! which makes a browser-pinning flow keyed on an opaque `browser_uuid`
//! unbuildable end to end.
//!
//! This allowlist is additive and never unmasks the raw header.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use crate::common::{self, body_json, json_request};

/// A workflow that copies the whole metadata object into `data` so a test can
/// see exactly what the engine received.
fn metadata_echo_workflow(name: &str) -> serde_json::Value {
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

/// POST with an optional `Cookie` header and hand back the metadata the
/// workflow saw.
async fn meta_with_cookie(
    app: &axum::Router,
    channel: &str,
    cookie: Option<&str>,
    body: serde_json::Value,
) -> serde_json::Value {
    let mut builder = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/data/{channel}"))
        .header("content-type", "application/json");
    if let Some(jar) = cookie {
        builder = builder.header("cookie", jar);
    }
    let resp = app
        .clone()
        .oneshot(
            builder
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await["data"]["meta"].clone()
}

async fn cookie_channel(app: &axum::Router, name: &str, allowlist: serde_json::Value) {
    common::create_and_activate_channel_with_config(
        app,
        name,
        metadata_echo_workflow(&format!("{name} WF")),
        json!({ "request": { "cookies_to_metadata": allowlist } }),
    )
    .await;
}

/// The motivating flow: an opaque browser id the workflow matches against its
/// own stored records.
#[tokio::test]
async fn an_allowlisted_cookie_reaches_the_workflow() {
    let app = common::test_app().await;
    cookie_channel(&app, "pin-ch", json!(["browser_uuid"])).await;

    let meta = meta_with_cookie(
        &app,
        "pin-ch",
        Some("browser_uuid=abc-123; session=secret-token"),
        json!({}),
    )
    .await;

    assert_eq!(meta["cookies"]["browser_uuid"], "abc-123", "{meta}");
    assert!(
        meta["cookies"].get("session").is_none(),
        "an unlisted cookie must never be copied: {meta}"
    );
    // The allowlist is additive: the raw header stays masked exactly as before.
    assert_eq!(
        meta["headers"]["cookie"], "******",
        "the raw Cookie header must still be masked: {meta}"
    );
}

/// A listed-but-absent cookie is simply not present — never `null`, never an
/// error. Matches how the `claims_to_metadata` filter behaves.
#[tokio::test]
async fn an_absent_cookie_is_absent_not_null() {
    let app = common::test_app().await;
    cookie_channel(&app, "absent-ch", json!(["browser_uuid"])).await;

    let meta = meta_with_cookie(&app, "absent-ch", Some("other=1"), json!({})).await;
    assert!(
        meta.get("cookies").is_none() || meta["cookies"].get("browser_uuid").is_none(),
        "an absent cookie must not be stamped as null: {meta}"
    );

    // No Cookie header at all is the same story.
    let meta = meta_with_cookie(&app, "absent-ch", None, json!({})).await;
    assert!(meta.get("cookies").is_none(), "{meta}");
}

/// Every channel today has no block, and must expose nothing.
#[tokio::test]
async fn a_channel_without_the_block_exposes_nothing() {
    let app = common::test_app().await;
    common::create_and_activate_channel(&app, "plain-ch", metadata_echo_workflow("Plain WF")).await;

    let meta = meta_with_cookie(&app, "plain-ch", Some("browser_uuid=abc"), json!({})).await;
    assert!(
        meta.get("cookies").is_none(),
        "an absent block must expose nothing: {meta}"
    );
    assert_eq!(meta["headers"]["cookie"], "******");
}

/// **The spoofing test.** `build_request_metadata` uses the caller's
/// `req.metadata` as its base, and stamps `params`/`query` only when non-empty
/// — so an envelope-supplied value for those survives. The same shape for
/// cookies would be session forgery, given that the motivating use is matching
/// the value against stored session records. `metadata.cookies` is therefore
/// platform-reserved: stamped from the allowlist, and stripped when there is
/// none.
#[tokio::test]
async fn envelope_supplied_cookies_cannot_be_forged() {
    let app = common::test_app().await;

    // (a) No block configured: a caller-supplied `metadata.cookies` is stripped.
    common::create_and_activate_channel(&app, "spoof-off", metadata_echo_workflow("Spoof A")).await;
    let meta = meta_with_cookie(
        &app,
        "spoof-off",
        None,
        json!({ "metadata": { "cookies": { "browser_uuid": "forged" } }, "data": {} }),
    )
    .await;
    assert!(
        meta.get("cookies").is_none(),
        "a caller must not be able to invent metadata.cookies: {meta}"
    );

    // (b) Block configured, no real cookie sent: still stripped, not merged.
    cookie_channel(&app, "spoof-on", json!(["browser_uuid"])).await;
    let meta = meta_with_cookie(
        &app,
        "spoof-on",
        None,
        json!({ "metadata": { "cookies": { "browser_uuid": "forged" } }, "data": {} }),
    )
    .await;
    assert!(
        meta.get("cookies").is_none(),
        "an allowlisted channel must not adopt a caller-supplied value: {meta}"
    );

    // (c) Block configured and a real cookie sent: the real one wins outright.
    let meta = meta_with_cookie(
        &app,
        "spoof-on",
        Some("browser_uuid=real-value"),
        json!({ "metadata": { "cookies": { "browser_uuid": "forged" } }, "data": {} }),
    )
    .await;
    assert_eq!(
        meta["cookies"]["browser_uuid"], "real-value",
        "the Cookie header is the only source: {meta}"
    );
}

/// `channel_call` propagates metadata verbatim, so an allowlisted cookie
/// reaches a sub-channel — the same way verified claims do.
#[tokio::test]
async fn an_allowlisted_cookie_survives_a_channel_call() {
    let app = common::test_app().await;

    // The callee echoes the metadata it received.
    common::create_and_activate_channel(&app, "callee-ch", metadata_echo_workflow("Callee WF"))
        .await;

    // The caller invokes it and captures the reply.
    common::create_and_activate_channel_with_config(
        &app,
        "caller-ch",
        common::workflow_with_tasks(
            "Caller WF",
            json!([{
                "id": "call", "name": "Call",
                "function": { "name": "channel_call", "input": {
                    "channel": "callee-ch",
                    "response_path": "data.inner"
                } }
            }]),
        ),
        json!({ "request": { "cookies_to_metadata": ["browser_uuid"] } }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/data/caller-ch")
                .header("content-type", "application/json")
                .header("cookie", "browser_uuid=abc-123")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["data"]["inner"]["meta"]["cookies"]["browser_uuid"], "abc-123",
        "a called channel inherits the caller's metadata verbatim: {body}"
    );
}

/// A typo here is an invisibly dead feature — serde cannot catch it, so
/// validation must.
#[tokio::test]
async fn structurally_impossible_names_are_refused() {
    let app = common::test_app().await;
    for bad in [
        json!([]),
        json!([""]),
        json!(["a=b"]),
        json!(["a;b"]),
        json!(["a b"]),
    ] {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/admin/channels",
                Some(json!({
                    "name": format!("bad-{}", bad[0].as_str().unwrap_or("empty")),
                    "channel_type": "sync",
                    "protocol": "rest",
                    "methods": ["POST"],
                    "route_pattern": "/bad",
                    "config": { "request": { "cookies_to_metadata": bad } }
                })),
            ))
            .await
            .expect("request");
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "{bad} must be refused"
        );
        let body = body_json(resp).await;
        let details = body["error"]["details"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(
            details
                .iter()
                .any(|d| d["path"] == "channel.config.request.cookies_to_metadata"),
            "{bad}: expected a field-pathed detail, got {body}"
        );
    }
}
