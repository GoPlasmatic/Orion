//! Proposal R17: every admin-plane 2xx body carries its payload under a
//! top-level `data` key, and nothing else at the top level except the three
//! pagination counters on list endpoints.
//!
//! Before 1.0 three envelopes coexisted — `{"data": …}`, the paginated
//! `{data, total, limit, offset}`, and bare objects from ten handlers — so a
//! client could not write one unwrapping function. These tests pin the single
//! shape endpoint by endpoint; `admin_2xx_bodies_all_carry_the_data_envelope`
//! is the one that would catch a *new* handler regressing to a bare object.

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
        "/api/v1/admin/engine/status",
        "/api/v1/admin/functions",
        "/api/v1/admin/connectors/circuit-breakers",
        "/api/v1/admin/backups",
    ] {
        assert_data_envelope(&get(&app, uri).await, uri);
    }

    for uri in [
        "/api/v1/admin/workflows",
        "/api/v1/admin/channels",
        "/api/v1/admin/connectors",
        "/api/v1/admin/audit-logs",
        "/api/v1/admin/traces",
        "/api/v1/admin/trace-dlq",
    ] {
        assert_paginated_envelope(&get(&app, uri).await, uri);
    }
}

#[tokio::test]
async fn engine_reload_is_enveloped() {
    let app = test_app().await;
    let resp = app
        .clone()
        .oneshot(json_request("POST", "/api/v1/admin/engine/reload", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_data_envelope(&body, "POST /engine/reload");
    assert_eq!(body["data"]["reloaded"], true);
}

#[tokio::test]
async fn workflow_validate_and_test_are_enveloped() {
    let app = test_app().await;
    let workflow = json!({
        "name": "envelope-wf",
        "channel": "envelope-ch",
        "tasks": [{
            "id": "t1",
            "name": "log",
            "function": {"name": "log", "input": {"message": "hi"}}
        }]
    });

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/validate",
            Some(workflow.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_data_envelope(&body, "POST /workflows/validate");
    assert_eq!(body["data"]["valid"], true);

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(workflow),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await;
    assert_data_envelope(&created, "POST /workflows");
    let id = created["data"]["workflow_id"].as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/admin/workflows/{id}/test"),
            Some(json!({"data": {"x": 1}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_data_envelope(&body, "POST /workflows/{id}/test");
    assert!(body["data"].get("matched").is_some());
}

/// R18: dry run and real run return the same four fields, told apart by
/// `dry_run`. The pre-1.0 dry run answered with six — `would_create` and
/// `would_fail` next to a hardcoded `imported: 0` and a `failed` that always
/// equalled `would_fail`.
#[tokio::test]
async fn import_dry_run_and_real_run_share_one_shape() {
    let app = test_app().await;
    let items = json!([{
        "name": "import-envelope-wf",
        "tasks": [{
            "id": "t1",
            "name": "log",
            "function": {"name": "log", "input": {"message": "hi"}}
        }]
    }]);

    let mut shapes = Vec::new();
    for uri in [
        "/api/v1/admin/workflows/import?dry_run=true",
        "/api/v1/admin/workflows/import",
    ] {
        let resp = app
            .clone()
            .oneshot(json_request("POST", uri, Some(items.clone())))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "POST {uri}");
        let body = body_json(resp).await;
        assert_data_envelope(&body, uri);
        let mut keys: Vec<String> = body["data"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        keys.sort();
        shapes.push((uri, body, keys));
    }

    let (dry_uri, dry, dry_keys) = &shapes[0];
    let (real_uri, real, real_keys) = &shapes[1];
    assert_eq!(
        dry_keys, real_keys,
        "{dry_uri} and {real_uri} must return the same field set"
    );
    assert_eq!(
        dry_keys,
        &["dry_run", "errors", "failed", "imported"],
        "unexpected import field set"
    );

    // The dry run reports its projection in `imported`, not a hardcoded 0.
    assert_eq!(dry["data"]["dry_run"], true);
    assert_eq!(dry["data"]["imported"], 1);
    assert_eq!(dry["data"]["failed"], 0);
    assert_eq!(real["data"]["dry_run"], false);
    assert_eq!(real["data"]["imported"], 1);
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
            "/api/v1/data/envelope-trace/async",
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
            &format!("/api/v1/admin/traces/{trace_id}?token={token}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_data_envelope(&body, "GET /traces/{id}");
    assert_eq!(body["data"]["id"], trace_id);
}
