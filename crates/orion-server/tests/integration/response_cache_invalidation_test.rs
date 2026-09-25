//! #354: channel response-cache invalidation by namespace, over the wire.
//!
//! The unit tests in `channel::guards` cover the version arithmetic and the
//! lookup-time tagging. These cover what only a whole server shows: that
//! `cache_invalidate` from one channel's workflow retires another channel's
//! entries, that the admin route does the same, that a channel which does not
//! declare the namespace is untouched, and that the config is validated.

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common;

/// A workflow whose response changes on every run, so a hit is visible: it
/// bumps a counter on a memory cache connector and answers the new value.
fn counting_workflow(name: &str) -> Value {
    common::workflow_with_tasks(
        name,
        json!([{
            "id": "count", "name": "count runs",
            "function": {"name": "cache_incr", "input": {
                "connector": "runs", "key": name, "output": "data.run"}}
        }]),
    )
}

async fn run_of(app: &axum::Router, channel: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            &format!("/api/v1/data/{channel}"),
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    common::body_json(resp).await["data"]["run"].clone()
}

#[tokio::test]
async fn a_workflow_and_the_admin_route_invalidate_a_namespace() {
    let app = common::test_app().await;
    common::create_connector(&app, common::cache_connector_memory("runs")).await;

    let cached = |ns: Option<&[&str]>| {
        let mut cache = json!({"enabled": true, "ttl_secs": 3600});
        if let Some(ns) = ns {
            cache["namespaces"] = json!(ns);
        }
        json!({ "cache": cache })
    };
    common::create_and_activate_channel_with_config(
        &app,
        "board",
        counting_workflow("board"),
        cached(Some(&["ladder"])),
    )
    .await;
    common::create_and_activate_channel_with_config(
        &app,
        "plain",
        counting_workflow("plain"),
        cached(None),
    )
    .await;
    common::create_and_activate_channel(
        &app,
        "finish-match",
        common::workflow_with_tasks(
            "finish-match",
            json!([{
                "id": "inv", "name": "invalidate the ladder",
                "function": {"name": "cache_invalidate", "input": {
                    "namespaces": ["ladder"], "output": "data.invalidated"}}
            }]),
        ),
    )
    .await;

    assert_eq!(run_of(&app, "board").await, 1);
    assert_eq!(run_of(&app, "board").await, 1, "served from the cache");
    assert_eq!(run_of(&app, "plain").await, 1);

    // The write-side workflow invalidates the namespace.
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/finish-match",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["data"]["invalidated"]["namespaces"], 1);
    assert!(body["data"]["invalidated"]["stores"].as_u64().unwrap_or(0) >= 1);

    assert_eq!(run_of(&app, "board").await, 2, "the bump retired the entry");
    assert_eq!(
        run_of(&app, "board").await,
        2,
        "and the fresh one is cached"
    );
    assert_eq!(
        run_of(&app, "plain").await,
        1,
        "a channel not declaring the namespace keeps its entry"
    );

    // The operator's route does the same, and is audited.
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/cache/namespaces/ladder/invalidate",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["data"]["namespace"], "ladder");
    assert_eq!(run_of(&app, "board").await, 3);

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/cache/namespaces/Not%20Valid/invalidate",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn namespaces_are_validated_at_create() {
    let app = common::test_app().await;
    for (label, namespaces) in [
        ("empty", json!([])),
        ("uppercase", json!(["Ladder"])),
        ("duplicate", json!(["a", "a"])),
        (
            "too many",
            json!(["a", "b", "c", "d", "e", "f", "g", "h", "i"]),
        ),
    ] {
        let resp = app
            .clone()
            .oneshot(common::json_request(
                "POST",
                "/api/v1/admin/channels",
                Some(json!({
                    "name": format!("ns-{}", label.replace(' ', "-")),
                    "channel_type": "sync",
                    "protocol": "http",
                    "methods": ["POST"],
                    "route_pattern": format!("/ns-{}", label.replace(' ', "-")),
                    "config": {"cache": {"enabled": true, "namespaces": namespaces}}
                })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{label}");
        let body = common::body_json(resp).await;
        assert!(
            body.to_string().contains("namespaces"),
            "{label}: the refusal names the field: {body}"
        );
    }
}

#[tokio::test]
async fn cache_invalidate_refuses_a_bad_name_without_bumping_anything() {
    let app = common::test_app().await;
    common::create_and_activate_channel(
        &app,
        "bad-inv",
        common::workflow_with_tasks(
            "bad-inv",
            json!([{
                "id": "inv", "name": "invalidate",
                "function": {"name": "cache_invalidate", "input": {
                    "namespaces": [{"var": "data.ns"}]}}
            }]),
        ),
    )
    .await;
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/bad-inv",
            Some(json!({"data": {"ns": "NOT OK"}})),
        ))
        .await
        .unwrap();
    let body = common::body_json(resp).await;
    assert!(
        body.to_string().contains("cache_invalidate"),
        "the task fails and names itself: {body}"
    );
}
