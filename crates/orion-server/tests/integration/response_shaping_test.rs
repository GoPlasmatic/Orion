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

// ---------------------------------------------------------------------------
// Cookies (#298)
// ---------------------------------------------------------------------------

/// Collect every value of a repeated header, in order.
fn all_headers(resp: &axum::response::Response, name: &str) -> Vec<String> {
    resp.headers()
        .get_all(name)
        .iter()
        .map(|v| v.to_str().expect("header is ASCII").to_string())
        .collect()
}

/// A channel that shapes its response *and* may set cookies.
fn shaped_cookie_config() -> Value {
    json!({ "response": { "mode": "shaped", "cookies": true } })
}

/// The case the issue was filed for: finishing an OAuth login sets the session
/// cookie **and** clears the spent state cookie in the same `302`.
///
/// Before this, one response could carry one `Set-Cookie`: the control block is
/// a JSON object, and `HeaderMap::insert` collapsed a repeated name to the last
/// value even once an array produced two.
#[tokio::test]
async fn a_shaped_response_can_set_and_clear_a_cookie_at_once() {
    let app = common::test_app().await;
    let workflow = shaping_workflow(
        "oauth-callback",
        json!({
            "status": 302,
            "headers": {"Location": "/welcome"},
            "cookies": [
                {"name": "session", "value": {"var": "data.in.token"},
                 "path": "/", "http_only": true, "secure": true,
                 "same_site": "Lax", "max_age": 2592000},
                {"name": "oauth_state", "value": "", "path": "/", "max_age": 0}
            ]
        }),
    );
    common::create_and_activate_channel_with_config(
        &app,
        "oauth-cb-ch",
        workflow,
        shaped_cookie_config(),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/oauth-cb-ch",
            Some(json!({"data": {"token": "jwt-abc"}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::FOUND);
    let cookies = all_headers(&resp, "set-cookie");
    assert_eq!(
        cookies,
        vec![
            "session=jwt-abc; Path=/; Max-Age=2592000; SameSite=Lax; HttpOnly; Secure",
            "oauth_state=; Path=/; Max-Age=0",
        ],
        "both cookies must reach the wire, in the order the workflow wrote them"
    );
}

/// The raw escape hatch: an array under a `set-cookie` header key. Nothing in
/// the mechanism is cookie-specific, so a repeated `link` works the same way.
#[tokio::test]
async fn an_array_header_value_sets_the_header_once_per_element() {
    let app = common::test_app().await;
    let workflow = shaping_workflow(
        "multi-link",
        json!({
            "status": 200,
            "headers": {"Link": ["</a>; rel=next", "</b>; rel=prev"]},
            "body_path": "data.order"
        }),
    );
    common::create_and_activate_channel_with_config(
        &app,
        "multi-link-ch",
        workflow,
        json!({ "response": { "mode": "shaped", "allowed_headers": ["link", "content-type"] } }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/multi-link-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();

    assert_eq!(
        all_headers(&resp, "link"),
        vec!["</a>; rel=next", "</b>; rel=prev"]
    );
}

/// The half of the `insert`/`append` split that guards the *old* behaviour:
/// a workflow's `content-type` still replaces the JSON default rather than
/// appearing beside it.
#[tokio::test]
async fn a_single_valued_header_still_replaces_the_default() {
    let app = common::test_app().await;
    let workflow = shaping_workflow(
        "csv",
        json!({
            "status": 200,
            "headers": {"Content-Type": "text/csv"},
            "body_path": "data.order.rows",
            "raw": true
        }),
    );
    common::create_and_activate_channel_with_config(&app, "csv-ch", workflow, shaped_config())
        .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/csv-ch",
            Some(json!({"data": {"rows": "a,b\n1,2"}})),
        ))
        .await
        .unwrap();

    assert_eq!(
        all_headers(&resp, "content-type"),
        vec!["text/csv"],
        "a replaced header must not be duplicated"
    );
}

/// Cookies are off unless the channel says so, and the switch is independent
/// of `allowed_headers` — this channel never lists `set-cookie` and still
/// serves JSON, which is the ergonomic point of the separate gate.
#[tokio::test]
async fn cookies_need_the_channel_to_enable_them() {
    let app = common::test_app().await;
    let workflow = shaping_workflow(
        "no-cookies",
        json!({
            "status": 200,
            "cookies": [{"name": "session", "value": "leaked"}],
            "body_path": "data.order"
        }),
    );
    common::create_and_activate_channel_with_config(
        &app,
        "no-cookies-ch",
        workflow,
        shaped_config(),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/no-cookies-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        all_headers(&resp, "set-cookie").is_empty(),
        "a channel that did not enable cookies must not set one"
    );
    assert_eq!(
        all_headers(&resp, "content-type"),
        vec!["application/json"],
        "and it still serves JSON without re-listing content-type"
    );
}

/// A value carrying `;` would let a workflow that interpolates user input into
/// a cookie inject further attributes — `HttpOnly` off, a wider `Path`. The
/// cookie is dropped rather than emitted, and the rest of the response stands.
#[tokio::test]
async fn a_cookie_value_that_could_inject_attributes_is_refused() {
    let app = common::test_app().await;
    let workflow = shaping_workflow(
        "inject",
        json!({
            "status": 200,
            "cookies": [
                {"name": "evil", "value": "x; Path=/; HttpOnly"},
                {"name": "good", "value": "ok"}
            ],
            "body_path": "data.order"
        }),
    );
    common::create_and_activate_channel_with_config(
        &app,
        "inject-ch",
        workflow,
        shaped_cookie_config(),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/inject-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        all_headers(&resp, "set-cookie"),
        vec!["good=ok"],
        "the injecting cookie is dropped and the valid one still ships"
    );
}
