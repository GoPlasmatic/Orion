//! Integration tests for the configurable trace-storage modes.
//!
//! Covers: sync (baseline), off (no persistence), async + batch (eventually
//! consistent persistence), the async-endpoint `trace_id: null` contract under
//! off mode, the `errors_only` filter, and per-channel override beating the
//! global default.

use crate::common;

use crate::common::{body_json, json_request};
use axum::http::StatusCode;
use orion::config::{AppConfig, TraceStorageMode, TracingStorageConfig};
use serde_json::json;
use tower::ServiceExt;

fn cfg_with_storage(mode: TraceStorageMode) -> AppConfig {
    let mut c = AppConfig::default();
    c.tracing.storage = TracingStorageConfig {
        mode,
        // tiny batch interval keeps tests fast
        batch_flush_interval_ms: 20,
        batch_size: 16,
        ..TracingStorageConfig::default()
    };
    c
}

async fn list_total(app: &axum::Router) -> u64 {
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/data/traces?limit=1", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    body["total"].as_u64().unwrap_or(0)
}

async fn submit_sync(app: &axum::Router, channel: &str) -> StatusCode {
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/data/{}", channel),
            Some(json!({"data": {"x": 1}})),
        ))
        .await
        .unwrap();
    resp.status()
}

// -----------------------------------------------------------------------
// Sync mode (baseline, default)
// -----------------------------------------------------------------------

#[tokio::test]
async fn sync_mode_persists_traces_inline() {
    let app = common::test_app().await; // default = sync
    let (_, _) =
        common::create_and_activate_channel(&app, "ch_sync", common::simple_log_workflow("Log"))
            .await;

    assert_eq!(submit_sync(&app, "ch_sync").await, StatusCode::OK);
    // Sync mode: trace is committed before the response returns, so it must
    // already be visible to the list endpoint.
    assert_eq!(list_total(&app).await, 1);
}

// -----------------------------------------------------------------------
// Off mode
// -----------------------------------------------------------------------

#[tokio::test]
async fn off_mode_skips_persistence() {
    let app = common::test_app_with_config(cfg_with_storage(TraceStorageMode::Off)).await;
    let (_, _) =
        common::create_and_activate_channel(&app, "ch_off", common::simple_log_workflow("Log"))
            .await;

    let before = list_total(&app).await;
    assert_eq!(submit_sync(&app, "ch_off").await, StatusCode::OK);
    assert_eq!(submit_sync(&app, "ch_off").await, StatusCode::OK);
    assert_eq!(submit_sync(&app, "ch_off").await, StatusCode::OK);
    assert_eq!(
        list_total(&app).await,
        before,
        "no rows should be persisted"
    );
}

// -----------------------------------------------------------------------
// Async + batch modes — persistence is eventual
// -----------------------------------------------------------------------

async fn assert_eventually_persisted(app: &axum::Router, expected_at_least: u64) {
    for _ in 0..50 {
        if list_total(app).await >= expected_at_least {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!(
        "trace was not persisted within 2.5s (saw {})",
        list_total(app).await
    );
}

#[tokio::test]
async fn async_mode_persists_eventually() {
    let app = common::test_app_with_config(cfg_with_storage(TraceStorageMode::Async)).await;
    let (_, _) =
        common::create_and_activate_channel(&app, "ch_async", common::simple_log_workflow("Log"))
            .await;
    let before = list_total(&app).await;
    assert_eq!(submit_sync(&app, "ch_async").await, StatusCode::OK);
    assert_eventually_persisted(&app, before + 1).await;
}

#[tokio::test]
async fn batch_mode_persists_eventually() {
    let app = common::test_app_with_config(cfg_with_storage(TraceStorageMode::Batch)).await;
    let (_, _) =
        common::create_and_activate_channel(&app, "ch_batch", common::simple_log_workflow("Log"))
            .await;
    let before = list_total(&app).await;
    for _ in 0..5 {
        assert_eq!(submit_sync(&app, "ch_batch").await, StatusCode::OK);
    }
    assert_eventually_persisted(&app, before + 5).await;
}

// -----------------------------------------------------------------------
// POST /{channel}/async behaviour under `off` mode
// -----------------------------------------------------------------------

#[tokio::test]
async fn async_endpoint_off_mode_returns_null_trace_id_with_warning() {
    let app = common::test_app_with_config(cfg_with_storage(TraceStorageMode::Off)).await;
    let (_, _) = common::create_and_activate_channel(
        &app,
        "ch_async_off",
        common::simple_log_workflow("Log"),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/ch_async_off/async",
            Some(json!({"data": {"x": 1}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let warning = resp
        .headers()
        .get("warning")
        .map(|v| v.to_str().unwrap_or("").to_string())
        .unwrap_or_default();
    assert!(
        warning.contains("Trace persistence disabled"),
        "expected Warning header, got '{}'",
        warning
    );
    let body = body_json(resp).await;
    assert!(body["trace_id"].is_null(), "body was {:?}", body);
}

// -----------------------------------------------------------------------
// `errors_only` filter
// -----------------------------------------------------------------------

#[tokio::test]
async fn errors_only_filter_drops_successful_sync_traces() {
    let mut c = AppConfig::default();
    c.tracing.storage = TracingStorageConfig {
        mode: TraceStorageMode::Sync,
        errors_only: true,
        ..TracingStorageConfig::default()
    };
    let app = common::test_app_with_config(c).await;
    let (_, _) =
        common::create_and_activate_channel(&app, "ch_errs", common::simple_log_workflow("Log"))
            .await;

    let before = list_total(&app).await;
    assert_eq!(submit_sync(&app, "ch_errs").await, StatusCode::OK);
    // A successful trace should be dropped under errors_only.
    assert_eq!(
        list_total(&app).await,
        before,
        "errors_only must drop success traces"
    );
}

// -----------------------------------------------------------------------
// Per-channel override
// -----------------------------------------------------------------------

#[tokio::test]
async fn channel_override_persists_when_global_is_off() {
    let app = common::test_app_with_config(cfg_with_storage(TraceStorageMode::Off)).await;

    // Create + activate workflow.
    let wf = common::create_and_activate_workflow(&app, common::simple_log_workflow("Log")).await;

    // Create channel with explicit `tracing.mode = "sync"` override.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "ch_override",
                "channel_type": "sync",
                "protocol": "http",
                "methods": ["POST"],
                "route_pattern": "/ch_override",
                "workflow_id": wf,
                "config": { "tracing": { "mode": "sync" } },
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let ch_id = body_json(resp).await["data"]["channel_id"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{}/status", ch_id),
            Some(json!({"status":"active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let before = list_total(&app).await;
    assert_eq!(submit_sync(&app, "ch_override").await, StatusCode::OK);
    assert_eq!(
        list_total(&app).await,
        before + 1,
        "channel override should beat global Off"
    );
}

// -----------------------------------------------------------------------
// traces.channel_id is populated (multi-instance-ha 0.5)
// -----------------------------------------------------------------------

/// Look up a channel's stable ID by name via the admin API.
async fn admin_channel_id(app: &axum::Router, name: &str) -> String {
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/channels", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .and_then(|c| c["channel_id"].as_str())
        .unwrap_or_else(|| panic!("channel '{name}' not found in admin list"))
        .to_string()
}

#[tokio::test]
async fn sync_trace_records_channel_id() {
    let app = common::test_app().await;
    common::create_and_activate_channel(&app, "ch_cid_sync", common::simple_log_workflow("Log"))
        .await;
    let expected = admin_channel_id(&app, "ch_cid_sync").await;

    assert_eq!(submit_sync(&app, "ch_cid_sync").await, StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/data/traces?channel=ch_cid_sync",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["data"][0]["channel_id"].as_str(),
        Some(expected.as_str())
    );
}

#[tokio::test]
async fn async_trace_records_channel_id() {
    let app = common::test_app().await;
    common::create_and_activate_channel(&app, "ch_cid_async", common::simple_log_workflow("Log"))
        .await;
    let expected = admin_channel_id(&app, "ch_cid_async").await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/ch_cid_async/async",
            Some(json!({"data": {"x": 1}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let submit = body_json(resp).await;
    let trace_id = submit["trace_id"].as_str().unwrap().to_string();
    let token = submit["trace_token"].as_str().unwrap().to_string();

    // The pending row is inserted before the 202, so channel_id is already set.
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/data/traces/{trace_id}?token={token}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["channel_id"].as_str(), Some(expected.as_str()));
}
