//! Workflow-shaped HTTP responses (`config.response.mode = "shaped"`).
//!
//! Every sync channel used to answer `200` with `{id, status, data, errors}`,
//! whatever happened. That is a reasonable contract between workflows and an
//! unreasonable one for a REST API: no `201` with a `Location`, no `404` for a
//! record that is not there, no `Content-Type` but JSON. Every consumer then
//! carries the same special case — "200 means maybe-error, look inside
//! `errors`" — which is precisely the per-service glue channels exist to
//! delete.
//!
//! Shaping is **opt-in per channel** and the tests below hold both halves of
//! that: a shaped channel gets what its workflow asked for, and a channel that
//! did not opt in is byte-identical to what it always returned.

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common;
use crate::common::{body_json, json_request};

/// A workflow that parses the payload and sets `_orion.response` from the
/// supplied control block.
fn shaping_workflow(name: &str, control: Value) -> Value {
    let mut mappings = vec![json!({
        "path": "data.order",
        "logic": {"var": "data.in"}
    })];
    for (key, value) in control.as_object().expect("control object") {
        mappings.push(json!({
            "path": format!("data._orion.response.{key}"),
            "logic": value
        }));
    }
    common::workflow_with_tasks(
        name,
        json!([
            {"id": "parse", "name": "Parse", "function": {
                "name": "parse_json", "input": {"source": "payload", "target": "in"}}},
            {"id": "shape", "name": "Shape", "function": {
                "name": "map", "input": {"mappings": mappings}}}
        ]),
    )
}

fn shaped_config() -> Value {
    json!({ "response": { "mode": "shaped" } })
}

/// The headline case: a create endpoint that answers `201` with a `Location`.
#[tokio::test]
async fn a_shaped_channel_returns_its_own_status_and_headers() {
    let app = common::test_app().await;
    let workflow = shaping_workflow(
        "created",
        json!({
            "status": 201,
            "headers": {"Location": {"cat": ["/orders/", {"var": "data.in.id"}]}},
            "body_path": "data.order"
        }),
    );
    common::create_and_activate_channel_with_config(&app, "created-ch", workflow, shaped_config())
        .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/created-ch",
            Some(json!({"data": {"id": "ORD-1", "total": 10}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        resp.headers().get("location").unwrap().to_str().unwrap(),
        "/orders/ORD-1"
    );

    // `body_path` selected the order, so the envelope is gone entirely.
    let body = body_json(resp).await;
    assert_eq!(body["id"], "ORD-1");
    assert_eq!(body["total"], 10);
    assert!(
        body.get("errors").is_none() && body.get("status").is_none(),
        "a shaped body is the workflow's own, not the envelope: {body}"
    );
}

/// A workflow can answer `404` — the case that is simply not expressible in
/// the envelope, where every outcome is a `200`.
#[tokio::test]
async fn a_shaped_channel_can_return_a_client_error_status() {
    let app = common::test_app().await;
    let workflow = shaping_workflow(
        "maybe-missing",
        json!({
            "status": {"if": [{"var": "data.in.found"}, 200, 404]},
            "body_path": "data.order"
        }),
    );
    common::create_and_activate_channel_with_config(&app, "missing-ch", workflow, shaped_config())
        .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/missing-ch",
            Some(json!({"data": {"found": false}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// `raw` sends a string field verbatim, which is how a shaped channel returns
/// CSV, XML or plain text rather than a JSON string.
#[tokio::test]
async fn a_shaped_channel_can_return_a_non_json_body() {
    let app = common::test_app().await;
    let workflow = shaping_workflow(
        "csv",
        json!({
            "status": 200,
            "headers": {"Content-Type": "text/csv"},
            "body_path": "data.csv",
            "raw": true
        }),
    );
    // The `csv` field itself has to exist; add it via the same map task.
    let mut workflow = workflow;
    workflow["tasks"][1]["function"]["input"]["mappings"]
        .as_array_mut()
        .unwrap()
        .push(json!({"path": "data.csv", "logic": "id,total\nORD-1,10"}));

    common::create_and_activate_channel_with_config(&app, "csv-ch", workflow, shaped_config())
        .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/csv-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "text/csv"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(bytes.to_vec()).unwrap(),
        "id,total\nORD-1,10"
    );
}

/// A header outside the channel's allowlist is dropped, not honoured — and
/// dropping it does not fail the request.
#[tokio::test]
async fn a_disallowed_header_is_dropped() {
    let app = common::test_app().await;
    let workflow = shaping_workflow(
        "narrow",
        json!({
            "status": 200,
            "headers": {"Location": "/allowed", "X-Custom": "nope"},
            "body_path": "data.order"
        }),
    );
    // Allowlist names Location only, so X-Custom is not grantable.
    let config = json!({
        "response": { "mode": "shaped", "allowed_headers": ["location"] }
    });
    common::create_and_activate_channel_with_config(&app, "narrow-ch", workflow, config).await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/narrow-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers().get("location").is_some(),
        "allowlisted header is set"
    );
    assert!(
        resp.headers().get("x-custom").is_none(),
        "a header outside the allowlist must not be set"
    );
}

/// The forbidden set wins over the allowlist: a channel cannot grant a
/// workflow control of the response framing or the request-correlation id.
#[tokio::test]
async fn a_forbidden_header_is_refused_even_when_allowlisted() {
    let app = common::test_app().await;
    let workflow = shaping_workflow(
        "forbidden",
        json!({
            "status": 200,
            "headers": {
                "Transfer-Encoding": "chunked",
                "Content-Length": "999",
                "X-Request-Id": "forged"
            },
            "body_path": "data.order"
        }),
    );
    let config = json!({
        "response": {
            "mode": "shaped",
            "allowed_headers": ["transfer-encoding", "content-length", "x-request-id"]
        }
    });
    common::create_and_activate_channel_with_config(&app, "forbidden-ch", workflow, config).await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/forbidden-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("transfer-encoding"),
        None,
        "hop-by-hop headers belong to the protocol layer"
    );
    assert_ne!(
        resp.headers()
            .get("x-request-id")
            .map(|v| v.to_str().unwrap_or_default()),
        Some("forged"),
        "the correlation id the trace is keyed by must not be workflow-settable"
    );
}

/// A cache hit replays the status and headers, not just the body.
///
/// This is the failure the feature invites: the response cache stored one
/// string and served it as a `200`, so a cached `201 Created` came back `200`
/// on the second identical request and the channel's contract changed
/// depending on cache state.
#[tokio::test]
async fn a_cached_shaped_response_replays_its_status_and_headers() {
    let app = common::test_app().await;
    let workflow = shaping_workflow(
        "cached-created",
        json!({
            "status": 201,
            "headers": {"Location": "/orders/CACHED"},
            "body_path": "data.order"
        }),
    );
    let config = json!({
        "response": { "mode": "shaped" },
        "cache": { "enabled": true, "ttl_secs": 300 }
    });
    common::create_and_activate_channel_with_config(&app, "cached-ch", workflow, config).await;

    let request = || {
        json_request(
            "POST",
            "/api/v1/data/cached-ch",
            Some(json!({"data": {"id": "CACHED"}})),
        )
    };

    let first = app.clone().oneshot(request()).await.unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);
    let first_location = first
        .headers()
        .get("location")
        .map(|v| v.to_str().unwrap().to_string());
    let first_body = axum::body::to_bytes(first.into_body(), usize::MAX)
        .await
        .unwrap();

    // Same payload — served from the response cache this time.
    let second = app.clone().oneshot(request()).await.unwrap();
    assert_eq!(
        second.status(),
        StatusCode::CREATED,
        "a cache hit must not downgrade a 201 to a 200"
    );
    assert_eq!(
        second
            .headers()
            .get("location")
            .map(|v| v.to_str().unwrap().to_string()),
        first_location,
        "a cache hit must replay the headers it stored"
    );
    let second_body = axum::body::to_bytes(second.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(first_body, second_body);
}

/// A channel that did not opt in is unchanged, including when its workflow
/// happens to write an `_orion` key.
///
/// The default is what every stored channel already has, so this is the test
/// that says the feature costs existing deployments nothing.
#[tokio::test]
async fn an_envelope_channel_is_untouched_by_the_control_key() {
    let app = common::test_app().await;
    let workflow = shaping_workflow(
        "envelope",
        json!({"status": 201, "headers": {"Location": "/nope"}}),
    );
    // No `response` config at all — the pre-1.0 default.
    common::create_and_activate_channel(&app, "envelope-ch", workflow).await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/envelope-ch",
            Some(json!({"data": {"id": "ORD-2"}})),
        ))
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a channel that did not opt in still answers 200"
    );
    assert!(
        resp.headers().get("location").is_none(),
        "and sets no header its workflow asked for"
    );

    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert!(body["errors"].as_array().is_some_and(|e| e.is_empty()));
    // The control block stays visible as ordinary data, because on this channel
    // that is all it ever was.
    assert_eq!(body["data"]["_orion"]["response"]["status"], 201);
}

/// A shaped channel whose workflow sets no control block still answers.
///
/// Falling back rather than 500ing matters: a cosmetic authoring slip should
/// not take the endpoint down.
#[tokio::test]
async fn a_shaped_channel_without_a_control_block_falls_back_to_the_envelope() {
    let app = common::test_app().await;
    let workflow = common::workflow_with_tasks(
        "no-control",
        json!([
            {"id": "parse", "name": "Parse", "function": {
                "name": "parse_json", "input": {"source": "payload", "target": "in"}}}
        ]),
    );
    common::create_and_activate_channel_with_config(
        &app,
        "no-control-ch",
        workflow,
        shaped_config(),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/no-control-ch",
            Some(json!({"data": {"x": 1}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok", "fell back to the envelope: {body}");
}

/// An out-of-range status is ignored rather than fatal, for the same reason.
#[tokio::test]
async fn an_invalid_status_falls_back_to_200() {
    let app = common::test_app().await;
    let workflow = shaping_workflow(
        "bad-status",
        json!({"status": 9999, "body_path": "data.order"}),
    );
    common::create_and_activate_channel_with_config(
        &app,
        "bad-status-ch",
        workflow,
        shaped_config(),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/bad-status-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
