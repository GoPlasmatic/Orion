//! Proposal R17: every admin-plane 2xx body carries its payload under a
//! top-level `data` key, and nothing else at the top level except the paging
//! fields (`limit`, `next_cursor`, `total`) on list endpoints.
//!
//! One shape is what lets a client write one unwrapping function; a mix of
//! `{"data": …}`, the paginated `{data, limit, next_cursor?, total?}` and bare objects
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

/// Assert `body` is the page shape: `data` and `limit`, plus `next_cursor` and
/// `total` only when there is something to say.
#[track_caller]
fn assert_page_envelope(body: &Value, what: &str) {
    let obj = body
        .as_object()
        .unwrap_or_else(|| panic!("{what}: body is not a JSON object: {body}"));
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["data", "limit"],
        "{what}: expected the page envelope, got {keys:?} in {body}"
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

    for uri in [
        "/api/v2/admin/audit-logs",
        "/api/v2/admin/trace-dlq",
        "/api/v2/admin/traces",
    ] {
        assert_page_envelope(&get(&app, uri).await, uri);
    }
}

/// A list's `total` is opt-in: the count is a full scan of the filtered set, so
/// it is paid only when asked for. `include_total=true` puts it back.
#[tokio::test]
async fn list_total_is_omitted_unless_asked() {
    let app = test_app().await;

    for uri in [
        "/api/v2/admin/traces",
        "/api/v2/admin/audit-logs",
        "/api/v2/admin/trace-dlq",
    ] {
        let body = get(&app, uri).await;
        assert!(
            body.get("total").is_none(),
            "an unasked-for `total` costs a COUNT(*) per page: {uri}: {body}"
        );
        let counted = get(&app, &format!("{uri}?include_total=true")).await;
        assert!(
            counted["total"].is_number(),
            "include_total=true must restore the count: {uri}: {counted}"
        );
    }
}

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
