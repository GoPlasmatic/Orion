//! #278: `config.request.body_mode` — opting out of envelope detection.
//!
//! `auto` (the default) treats **any** object carrying a top-level `data` or
//! `metadata` key as the Orion envelope: it takes that key as the payload and
//! discards every sibling field, silently, with a normal `200`. For a migrated
//! model that owns the name `data` — the FCM/push payload shape among them —
//! that is data loss on write that nothing reports.
//!
//! `payload` takes the parsed body verbatim. The two differ for exactly one
//! input shape (a top-level object carrying those keys), which is what keeps
//! this small.

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use crate::common::{self, body_json, json_request};

/// The motivating body: a push notification whose model owns `data`.
fn push_body() -> serde_json::Value {
    json!({
        "platform": "ios",
        "version_code": 42,
        "force": true,
        "title": "Hello",
        "data": { "title": "inner", "message": "m" }
    })
}

/// POST a body and hand back the payload the engine saw.
///
/// The request body becomes the message **payload**, not `data` — `data`
/// starts empty — so `common::echo_workflow` parses it into `data.input`,
/// which is what these tests read.
async fn seen(app: &axum::Router, channel: &str, body: serde_json::Value) -> serde_json::Value {
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/data/{channel}"),
            Some(body),
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await["data"]["input"].clone()
}

/// The defect, and the fix. On a payload-mode channel every sibling survives.
#[tokio::test]
async fn payload_mode_keeps_the_whole_body() {
    let app = common::test_app().await;
    common::create_and_activate_channel_with_config(
        &app,
        "push-ch",
        common::echo_workflow("Push WF"),
        json!({ "request": { "body_mode": "payload" } }),
    )
    .await;

    let seen = seen(&app, "push-ch", push_body()).await;
    assert_eq!(seen["platform"], "ios", "{seen}");
    assert_eq!(seen["version_code"], 42);
    assert_eq!(seen["force"], true);
    assert_eq!(seen["title"], "Hello");
    assert_eq!(
        seen["data"]["message"], "m",
        "the model's own `data` field is preserved as a field, not unwrapped"
    );
}

/// The same body on a default channel is still truncated. This pins the
/// default: an accidental flip of it becomes a loud test failure rather than a
/// silent wire-contract change for every existing channel.
#[tokio::test]
async fn auto_mode_still_truncates_and_is_the_default() {
    let app = common::test_app().await;
    common::create_and_activate_channel(&app, "auto-ch", common::echo_workflow("Auto WF")).await;

    let seen = seen(&app, "auto-ch", push_body()).await;
    assert_eq!(
        seen["title"], "inner",
        "auto mode takes the `data` key as the payload: {seen}"
    );
    assert!(
        seen.get("platform").is_none(),
        "the siblings are dropped — the behaviour #278 exists to make opt-out-able"
    );
}

/// An explicit `"auto"` must be byte-identical to omitting the block.
#[tokio::test]
async fn explicit_auto_equals_an_absent_block() {
    let app = common::test_app().await;
    common::create_and_activate_channel_with_config(
        &app,
        "explicit-ch",
        common::echo_workflow("Explicit WF"),
        json!({ "request": { "body_mode": "auto" } }),
    )
    .await;

    let seen = seen(&app, "explicit-ch", push_body()).await;
    assert_eq!(seen["title"], "inner");
    assert!(seen.get("platform").is_none());
}

/// The shapes that already took the payload path in `auto` must behave
/// identically in `payload` — which is what makes this a one-shape change.
#[tokio::test]
async fn arrays_scalars_and_plain_objects_are_unchanged_by_the_mode() {
    let app = common::test_app().await;
    common::create_and_activate_channel(&app, "parity-auto", common::echo_workflow("Parity A"))
        .await;
    common::create_and_activate_channel_with_config(
        &app,
        "parity-payload",
        common::echo_workflow("Parity P"),
        json!({ "request": { "body_mode": "payload" } }),
    )
    .await;

    for body in [
        json!([1, 2, 3]),
        json!("a string"),
        json!(7),
        json!({ "amount": 5, "currency": "EUR" }),
    ] {
        let auto = seen(&app, "parity-auto", body.clone()).await;
        let payload = seen(&app, "parity-payload", body.clone()).await;
        assert_eq!(auto, payload, "the modes must agree on {body}");
    }
}

/// An empty body stays `{}` in both modes — payload mode must not turn it into
/// `null`, or every `GET`/`DELETE` REST channel breaks.
#[tokio::test]
async fn an_empty_body_is_an_empty_object_in_payload_mode() {
    let app = common::test_app().await;
    common::create_and_activate_channel_with_config(
        &app,
        "empty-ch",
        common::echo_workflow("Empty WF"),
        json!({ "request": { "body_mode": "payload" } }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request("POST", "/api/v1/data/empty-ch", None))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["input"], json!({}));
}

/// In payload mode a caller-supplied `metadata` is *data*, not metadata — the
/// documented trade-off, and a small security win, since `params`/`query` are
/// stamped only when non-empty and a caller-supplied value for them survives
/// in `auto`.
#[tokio::test]
async fn caller_metadata_becomes_data_in_payload_mode() {
    let app = common::test_app().await;
    common::create_and_activate_channel_with_config(
        &app,
        "meta-ch",
        common::echo_workflow("Meta WF"),
        json!({ "request": { "body_mode": "payload" } }),
    )
    .await;

    let seen = seen(
        &app,
        "meta-ch",
        json!({ "metadata": { "spoofed": true }, "x": 1 }),
    )
    .await;
    assert_eq!(seen["x"], 1);
    assert_eq!(
        seen["metadata"]["spoofed"], true,
        "a caller cannot reach the metadata object at all: {seen}"
    );
}

/// `/async` is the same function and the same call site, so the mode applies
/// there too — asserted rather than assumed, including that the persisted
/// input is the whole body.
#[tokio::test]
async fn payload_mode_applies_to_the_async_path() {
    let app = common::test_app().await;
    common::create_and_activate_channel_with_config(
        &app,
        "async-ch",
        common::echo_workflow("Async WF"),
        json!({ "request": { "body_mode": "payload" } }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/async-ch/async",
            Some(push_body()),
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let accepted = body_json(resp).await;
    let trace_id = accepted["trace_id"]
        .as_str()
        .expect("a trace id")
        .to_string();
    let token = accepted["trace_token"].as_str().map(str::to_string);

    // The persisted input must be the whole body, not the unwrapped `data`.
    let trace = common::poll_trace_until_done(&app, &trace_id, 30, token.as_deref()).await;
    assert_eq!(trace["status"], "completed");
    let input = &trace["message"]["context"]["data"]["input"];
    assert_eq!(
        input["platform"], "ios",
        "the async path applies the mode identically: {trace}"
    );
    assert_eq!(input["title"], "Hello", "siblings survive on /async too");
    // The persisted payload is the whole body, not the unwrapped `data`.
    assert_eq!(trace["message"]["payload"]["version_code"], 42);
}
