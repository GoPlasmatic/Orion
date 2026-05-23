//! B6 (Tier-2 ergonomics): bulk import for channels/connectors + ?dry_run=true.
//!
//! Verifies:
//!   - POST /api/v1/admin/channels/import imports an array of channels and
//!     returns {imported, failed, errors[]}
//!   - POST /api/v1/admin/connectors/import same shape for connectors
//!   - ?dry_run=true on workflows/channels/connectors imports validates
//!     without writing and reports would_create / would_fail

mod common;

use axum::http::StatusCode;
use common::{body_json, json_request, test_app};
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn channels_import_creates_each_item() {
    let app = test_app().await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels/import",
            Some(json!([
                {
                    "name": "ch-imp-1",
                    "channel_type": "sync",
                    "protocol": "rest",
                    "methods": ["POST"],
                    "route_pattern": "/ch1"
                },
                {
                    "name": "ch-imp-2",
                    "channel_type": "sync",
                    "protocol": "rest",
                    "methods": ["POST"],
                    "route_pattern": "/ch2"
                }
            ])),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["imported"], 2);
    assert_eq!(body["failed"], 0);
}

#[tokio::test]
async fn channels_import_dry_run_does_not_persist() {
    let app = test_app().await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels/import?dry_run=true",
            Some(json!([
                {
                    "name": "ch-dry-1",
                    "channel_type": "sync",
                    "protocol": "rest",
                    "methods": ["POST"],
                    "route_pattern": "/dry1"
                },
                {
                    // Will fail validation (REST without route_pattern).
                    "name": "ch-dry-broken",
                    "channel_type": "sync",
                    "protocol": "rest",
                    "methods": ["POST"]
                }
            ])),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["dry_run"], true);
    assert_eq!(body["would_create"], 1);
    assert_eq!(body["would_fail"], 1);
    assert_eq!(body["imported"], 0);

    // Confirm the would-be channel was NOT actually persisted.
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/channels", None))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let data = body["data"].as_array().unwrap();
    assert!(
        data.iter().all(|ch| ch["name"] != "ch-dry-1"),
        "dry-run must not persist any rows, got {data:?}"
    );
}

#[tokio::test]
async fn connectors_import_creates_each_item() {
    let app = test_app().await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors/import",
            Some(json!([
                { "name": "imp-a", "connector_type": "http", "config": {"url": "https://a.example.com"} },
                { "name": "imp-b", "connector_type": "http", "config": {"url": "https://b.example.com"} }
            ])),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["imported"], 2);
}

#[tokio::test]
async fn connectors_import_dry_run_reports_validation_outcome() {
    let app = test_app().await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors/import?dry_run=true",
            Some(json!([
                { "name": "good", "connector_type": "http", "config": {"url": "https://good.example"} },
                { "name": "bad-url-scheme", "connector_type": "http", "config": {"url": "ftp://bad.example"} }
            ])),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["dry_run"], true);
    assert_eq!(body["would_create"], 1);
    assert_eq!(body["would_fail"], 1);
}

#[tokio::test]
async fn workflows_import_dry_run_does_not_persist() {
    // Existing /workflows/import endpoint: B6 added ?dry_run=true to it.
    let app = test_app().await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/import?dry_run=true",
            Some(json!([
                {
                    "name": "wf-dry",
                    "tasks": [{"id":"t1","name":"log","function":{"name":"log","input":{"message":"x"}}}]
                }
            ])),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["dry_run"], true);
    assert_eq!(body["would_create"], 1);
    assert_eq!(body["imported"], 0);
    // No row persisted.
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/workflows", None))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert!(
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|w| w["name"] != "wf-dry")
    );
}
