//! O4: the `/api/v1/admin/trace-dlq` operator surface — list, get, requeue,
//! purge. Before this the DLQ had no read path at all, so entries accumulated
//! with zero visibility and no manual replay.

use axum::http::StatusCode;
use tower::ServiceExt;

use crate::common;

use orion::config::AppConfig;
use orion::server::state::AppState;

const PAYLOAD: &str = r#"{"order":"A-1"}"#;

/// Two entries: one still retrying, one that has given up.
async fn seed(state: &AppState) -> (String, String) {
    let pending = state
        .trace_dlq_repo
        .enqueue("trace-pending", "orders", PAYLOAD, "{}", "boom", 0, 3)
        .await
        .expect("enqueue pending");
    let exhausted = state
        .trace_dlq_repo
        .enqueue("trace-exhausted", "payments", PAYLOAD, "{}", "boom", 3, 3)
        .await
        .expect("enqueue exhausted");
    (pending.id, exhausted.id)
}

async fn app_with_entries() -> (axum::Router, String, String) {
    let state = common::test_state_with_config(AppConfig::default()).await;
    let (pending, exhausted) = seed(&state).await;
    (orion::server::build_router(state), pending, exhausted)
}

#[tokio::test]
async fn list_paginates_and_omits_payloads() {
    let (app, _pending, _exhausted) = app_with_entries().await;

    let resp = app
        .oneshot(common::json_request("GET", "/api/v1/admin/trace-dlq", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;

    assert_eq!(body["total"], 2);
    assert_eq!(body["limit"], 50);
    let rows = body["data"].as_array().expect("data array");
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert!(row["channel"].is_string());
        assert!(row["retry_count"].is_number());
        assert!(
            row.get("payload_json").is_none(),
            "the list view must not carry payloads: {row}"
        );
        assert!(row.get("metadata_json").is_none());
    }
}

#[tokio::test]
async fn list_filters_by_exhaustion_and_channel() {
    let (app, _pending, exhausted) = app_with_entries().await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "GET",
            "/api/v1/admin/trace-dlq?exhausted=true",
            None,
        ))
        .await
        .unwrap();
    let body = common::body_json(resp).await;
    assert_eq!(body["total"], 1);
    assert_eq!(body["data"][0]["id"], exhausted);

    let resp = app
        .oneshot(common::json_request(
            "GET",
            "/api/v1/admin/trace-dlq?channel=orders",
            None,
        ))
        .await
        .unwrap();
    let body = common::body_json(resp).await;
    assert_eq!(body["total"], 1);
    assert_eq!(body["data"][0]["channel"], "orders");
}

#[tokio::test]
async fn get_by_id_returns_the_payload() {
    let (app, pending, _exhausted) = app_with_entries().await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "GET",
            &format!("/api/v1/admin/trace-dlq/{pending}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["data"]["payload_json"], PAYLOAD);
    assert_eq!(body["data"]["error_message"], "boom");

    let resp = app
        .oneshot(common::json_request(
            "GET",
            "/api/v1/admin/trace-dlq/no-such-entry",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn requeue_resets_an_exhausted_entry() {
    let (app, _pending, exhausted) = app_with_entries().await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            &format!("/api/v1/admin/trace-dlq/{exhausted}/requeue"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["data"]["retry_count"], 0);

    // Nothing is exhausted any more.
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "GET",
            "/api/v1/admin/trace-dlq?exhausted=true",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(common::body_json(resp).await["total"], 0);

    let resp = app
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/trace-dlq/no-such-entry/requeue",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn purge_removes_only_exhausted_entries() {
    let (app, pending, _exhausted) = app_with_entries().await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/trace-dlq/purge",
            Some(serde_json::json!({"older_than_hours": 0})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(common::body_json(resp).await["purged"], 1);

    let resp = app
        .oneshot(common::json_request("GET", "/api/v1/admin/trace-dlq", None))
        .await
        .unwrap();
    let body = common::body_json(resp).await;
    assert_eq!(body["total"], 1);
    assert_eq!(
        body["data"][0]["id"], pending,
        "an entry with retries left must survive a purge"
    );
}

/// Purging is destructive; an omitted age must be an error, not "everything".
#[tokio::test]
async fn purge_requires_an_explicit_age() {
    let (app, _pending, _exhausted) = app_with_entries().await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/trace-dlq/purge",
            Some(serde_json::json!({})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let resp = app
        .oneshot(common::json_request("GET", "/api/v1/admin/trace-dlq", None))
        .await
        .unwrap();
    assert_eq!(common::body_json(resp).await["total"], 2);
}

/// A purge far in the past must spare recent entries — the age bound is real,
/// not decorative.
#[tokio::test]
async fn purge_respects_the_age_bound() {
    let (app, _pending, _exhausted) = app_with_entries().await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/trace-dlq/purge",
            Some(serde_json::json!({"older_than_hours": 24})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(common::body_json(resp).await["purged"], 0);

    let resp = app
        .oneshot(common::json_request("GET", "/api/v1/admin/trace-dlq", None))
        .await
        .unwrap();
    assert_eq!(common::body_json(resp).await["total"], 2);
}
