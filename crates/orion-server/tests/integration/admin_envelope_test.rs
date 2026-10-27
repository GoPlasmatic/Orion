//! Proposal R17: every admin-plane 2xx body carries its payload under a
//! top-level `data` key, and nothing else at the top level except the three
//! pagination counters on list endpoints.
//!
//! One shape is what lets a client write one unwrapping function; a mix of
//! `{"data": …}`, the paginated `{data, total, limit, offset}` and bare objects
//! would not. These tests pin the single shape endpoint by endpoint;
//! `admin_2xx_bodies_all_carry_the_data_envelope` is the one that would catch
//! a *new* handler regressing to a bare object.
//!
//! The deployment surface (`/definitions`, `/deployments`, `/connections`, …)
//! answers bare bodies by design and is not held to this shape.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common;
use crate::common::{body_json, json_request, test_app};

/// Assert `body`'s top-level keys are exactly `data` — the single-item shape.
#[track_caller]
fn assert_data_envelope(body: &Value, what: &str) {
    let obj = body
        .as_object()
        .unwrap_or_else(|| panic!("{what}: body is not a JSON object: {body}"));
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["data"],
        "{what}: expected exactly a `data` envelope, got {keys:?} in {body}"
    );
}

/// Assert `body` is the paginated shape: `data` plus the three counters.
#[track_caller]
fn assert_paginated_envelope(body: &Value, what: &str) {
    let obj = body
        .as_object()
        .unwrap_or_else(|| panic!("{what}: body is not a JSON object: {body}"));
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["data", "limit", "offset", "total"],
        "{what}: expected the paginated envelope, got {keys:?} in {body}"
    );
    assert!(
        body["data"].is_array(),
        "{what}: paginated `data` must be an array, got {}",
        body["data"]
    );
}

async fn get(app: &axum::Router, uri: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "GET {uri}");
    body_json(resp).await
}

/// The endpoints that returned bare objects before R17, plus the two that
/// hand-rolled an envelope instead of using the shared helper.
#[tokio::test]
async fn admin_2xx_bodies_all_carry_the_data_envelope() {
    let app = test_app().await;

    for uri in [
        "/api/v2/admin/engine/status",
        "/api/v2/admin/functions",
        "/api/v2/admin/connectors/circuit-breakers",
        "/api/v2/admin/backups",
    ] {
        assert_data_envelope(&get(&app, uri).await, uri);
    }

    for uri in ["/api/v2/admin/audit-logs", "/api/v2/admin/trace-dlq"] {
        assert_paginated_envelope(&get(&app, uri).await, uri);
    }
}

/// The trace list is the one paginated endpoint that does not always carry
/// `total` (D8): the count is a full scan of the filtered set, so it is opt-in.
/// `include_total=true` puts it back, byte for byte.
#[tokio::test]
async fn trace_list_omits_total_unless_asked() {
    let app = test_app().await;

    let body = get(&app, "/api/v2/admin/traces").await;
    let mut keys: Vec<&str> = body
        .as_object()
        .expect("object body")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["data", "limit", "offset"],
        "an unasked-for `total` costs a COUNT(*) per page: {body}"
    );

    let counted = get(&app, "/api/v2/admin/traces?include_total=true").await;
    assert_eq!(
        counted["total"], 0,
        "include_total=true must restore the count: {counted}"
    );
}

/// The single-trace read is the one admin endpoint whose envelope the rest of
/// the suite cannot catch: `common::poll_trace_until_done` unwraps `data` for
/// its callers, so without this test the wrapper would be unpinned.
#[tokio::test]
async fn single_trace_read_is_enveloped() {
    let app = test_app().await;
    common::create_and_activate_channel(
        &app,
        "envelope-trace",
        common::simple_log_workflow("Envelope Trace Workflow"),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/envelope-trace/async",
            Some(json!({"data": {"value": 1}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let submit = body_json(resp).await;
    let trace_id = submit["trace_id"].as_str().unwrap().to_string();
    let token = submit["trace_token"].as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v2/admin/traces/{trace_id}?token={token}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_data_envelope(&body, "GET /traces/{id}");
    assert_eq!(body["data"]["id"], trace_id);
}
