//! Integration tests for the configurable trace-storage modes.
//!
//! Covers: sync (baseline), off (no persistence), async + batch (eventually
//! consistent persistence), the async-endpoint `trace_id: null` contract under
//! off mode, the `errors_only` filter, and per-channel override beating the
//! global default.

use crate::common;

use crate::common::{body_json, json_request};
use axum::http::StatusCode;
use orion::config::{AppConfig, TraceStorageConfig, TraceStorageMode};
use serde_json::json;
use tower::ServiceExt;

fn cfg_with_storage(mode: TraceStorageMode) -> AppConfig {
    AppConfig {
        trace_storage: TraceStorageConfig {
            mode,
            // tiny batch interval keeps tests fast
            batch_flush_interval_ms: 20,
            batch_size: 16,
            ..TraceStorageConfig::default()
        },
        ..AppConfig::default()
    }
}

async fn list_total(app: &axum::Router) -> u64 {
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v2/admin/traces?limit=1&include_total=true",
            None,
        ))
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
            &format!("/api/v2/data/{}", channel),
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
    let app = common::test_app_with_config(cfg_with_storage(TraceStorageMode::Sync)).await;
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

/// R11: `mode = off` does not turn the async path into a receipt whose
/// documented follow-up is structurally impossible (a null `trace_id`, a
/// `Warning: 299`, the work enqueued anyway).
///
/// Appending `/async` *is* the request for a result to be fetched later, so
/// the row is written regardless of `mode`. The result must actually arrive:
/// dropping it while the row exists would leave the trace at `pending` forever,
/// which is the same dead end wearing a different hat.
#[tokio::test]
async fn an_async_submission_is_pollable_even_when_trace_storage_is_off() {
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
            "/api/v2/data/ch_async_off/async",
            Some(json!({"data": {"x": 1}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert!(
        resp.headers().get("warning").is_none(),
        "there is nothing conditional left to warn about"
    );
    let body = body_json(resp).await;
    let trace_id = body["trace_id"]
        .as_str()
        .unwrap_or_else(|| panic!("trace_id must be present, got {body}"))
        .to_string();
    let token = body["trace_token"]
        .as_str()
        .unwrap_or_else(|| panic!("trace_token must be present, got {body}"))
        .to_string();

    let final_trace = common::poll_trace_until_done(&app, &trace_id, 40, Some(&token)).await;
    assert_eq!(final_trace["status"], "completed", "{final_trace}");
}

/// The sync path is untouched: `off` still means no row at all, because the
/// caller already has the answer in the response.
#[tokio::test]
async fn off_mode_still_persists_nothing_on_the_sync_path() {
    let app = common::test_app_with_config(cfg_with_storage(TraceStorageMode::Off)).await;
    let (_, _) = common::create_and_activate_channel(
        &app,
        "ch_sync_off",
        common::simple_log_workflow("Log"),
    )
    .await;
    let before = list_total(&app).await;
    assert_eq!(submit_sync(&app, "ch_sync_off").await, StatusCode::OK);
    assert_eq!(
        list_total(&app).await,
        before,
        "off mode must write no rows"
    );
}

// -----------------------------------------------------------------------
// `errors_only` filter
// -----------------------------------------------------------------------

#[tokio::test]
async fn errors_only_filter_drops_successful_sync_traces() {
    let c = AppConfig {
        trace_storage: TraceStorageConfig {
            mode: TraceStorageMode::Sync,
            errors_only: true,
            ..TraceStorageConfig::default()
        },
        ..AppConfig::default()
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
// N22: sampling is per-trace, decided once
// -----------------------------------------------------------------------

/// N22: a sampled-out sync trace produces no rows at all — the draw happens
/// once, at the single point the trace's persistence is decided.
#[tokio::test]
async fn sampled_out_sync_trace_writes_no_rows() {
    let c = AppConfig {
        trace_storage: TraceStorageConfig {
            mode: TraceStorageMode::Sync,
            sample_rate: 0.0,
            ..TraceStorageConfig::default()
        },
        ..AppConfig::default()
    };
    let app = common::test_app_with_config(c).await;
    let (_, _) =
        common::create_and_activate_channel(&app, "ch_sampled", common::simple_log_workflow("Log"))
            .await;

    let before = list_total(&app).await;
    assert_eq!(submit_sync(&app, "ch_sampled").await, StatusCode::OK);
    assert_eq!(
        list_total(&app).await,
        before,
        "a sampled-out trace must leave no rows"
    );
}

/// N22: async submissions are never sampled out. The 202's `trace_id` is a
/// receipt for a fetchable result, so `for_async_submission` pins the sample
/// rate to 1.0 — sampling one out would keep the status row but drop the
/// result, leaving the caller a `completed` trace with nothing in it.
#[tokio::test]
async fn async_submission_is_never_sampled_out() {
    let app = common::test_app().await;
    let (_, _) = common::create_and_activate_channel_with_config(
        &app,
        "ch_async_sampled",
        common::simple_log_workflow("Log"),
        json!({ "tracing": { "sample_rate": 0.0 } }),
    )
    .await;

    let (trace_id, token) = common::submit_async(
        &app,
        "/api/v2/data/ch_async_sampled/async",
        json!({"data": {"x": 1}}),
    )
    .await;
    let body = common::poll_trace_until_done(&app, &trace_id, 40, Some(&token)).await;
    assert_eq!(body["status"], "completed", "{body}");
    assert!(
        body.get("message").is_some(),
        "an async trace's result must never be sampled away: {body}"
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
    common::deploy_channel(
        &app,
        json!({
            "name": "ch_override",
            "channel_type": "sync",
            "protocol": "http",
            "methods": ["POST"],
            "route_pattern": "/ch_override",
            "workflow_id": wf,
            "config": { "tracing": { "mode": "sync" } },
        }),
    )
    .await;

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

#[tokio::test]
async fn sync_trace_records_channel_id() {
    let app = common::test_app().await;
    // A channel's stable id is the one its deployment gives it.
    let (expected, _) = common::create_and_activate_channel_full(
        &app,
        "ch_cid_sync",
        common::simple_log_workflow("Log"),
        json!({}),
    )
    .await;

    assert_eq!(submit_sync(&app, "ch_cid_sync").await, StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v2/admin/traces?channel=ch_cid_sync",
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
    // A channel's stable id is the one its deployment gives it.
    let (expected, _) = common::create_and_activate_channel_full(
        &app,
        "ch_cid_async",
        common::simple_log_workflow("Log"),
        json!({}),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/ch_cid_async/async",
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
            &format!("/api/v2/admin/traces/{trace_id}?token={token}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["channel_id"].as_str(), Some(expected.as_str()));
}

// ---------------------------------------------------------------------------
// D6: retention deletes are chunked
// ---------------------------------------------------------------------------

/// The retention DELETE runs in bounded chunks rather than one unbounded
/// statement per tick, and the loop must still drain everything — a chunked
/// delete that stops after the first chunk would silently leave the table
/// growing.
///
/// 2 500 rows against a 1 000-row chunk forces three statements plus the
/// short final one, so this exercises the loop rather than the single-chunk
/// fast path.
#[tokio::test]
async fn retention_delete_drains_more_rows_than_one_chunk() {
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let repo = state.repos.traces.clone();

    // Rows older than the retention window, plus a few inside it.
    let old = chrono::Utc::now().naive_utc() - chrono::Duration::hours(200);
    let recent = chrono::Utc::now().naive_utc();
    for (n, created) in [(2_500, old), (7, recent)] {
        for i in 0..n {
            insert_trace(&state.db_pool, &format!("{created}-{i}"), created).await;
        }
    }

    let before = count_traces(&state.db_pool).await;
    assert_eq!(before, 2_507, "fixture did not land");

    let deleted = repo.delete_older_than(72).await.expect("cleanup");
    assert_eq!(
        deleted, 2_500,
        "every expired row must go, not just one chunk"
    );
    assert_eq!(
        count_traces(&state.db_pool).await,
        7,
        "rows inside the retention window must survive"
    );

    // A second pass has nothing to do and must not error or loop.
    assert_eq!(repo.delete_older_than(72).await.expect("cleanup"), 0);
}

async fn insert_trace(pool: &orion::storage::DbPool, id: &str, created: chrono::NaiveDateTime) {
    let sql = "INSERT INTO traces (id, channel, mode, status, created_at, updated_at) \
               VALUES (?, 'ch', 'sync', 'completed', ?, ?)";
    pool.execute_query(
        sql,
        sea_query_sqlx::SqlxValues(sea_query::Values(vec![
            id.into(),
            created.into(),
            created.into(),
        ])),
    )
    .await
    .expect("insert trace");
}

async fn count_traces(pool: &orion::storage::DbPool) -> i64 {
    let (n,): (i64,) = pool
        .fetch_one_as::<(i64,)>(
            "SELECT COUNT(*) FROM traces",
            sea_query_sqlx::SqlxValues(sea_query::Values(vec![])),
        )
        .await
        .expect("count");
    n
}

/// The stored `(input_json, result_json)` of the one trace on `channel`,
/// once a queued worker has written it.
async fn stored_payloads(pool: &orion::storage::DbPool, channel: &str) -> (String, String) {
    for _ in 0..50 {
        let row = pool
            .fetch_optional_as::<(Option<String>, Option<String>)>(
                "SELECT input_json, result_json FROM traces WHERE channel = ?",
                sea_query_sqlx::SqlxValues(sea_query::Values(vec![channel.into()])),
            )
            .await
            .expect("read trace");
        if let Some((Some(input), Some(result))) = row {
            return (input, result);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("no trace for {channel} within 2.5s");
}

/// The queued modes take a sync trace's payloads by move rather than copying
/// them. Whichever way the full-detail envelope travels — kept as the HTTP
/// body on a clean run, given to the trace when the run carried errors — the
/// row must hold the request body and that envelope.
#[tokio::test]
async fn a_queued_sync_trace_stores_the_input_and_the_full_detail_envelope() {
    for mode in [TraceStorageMode::Async, TraceStorageMode::Batch] {
        let state = common::test_state_with_config(cfg_with_storage(mode)).await;
        let pool = state.db_pool.clone();
        let app = orion::server::build_router(state);

        // Clean: the envelope is the body, and the trace stores the same bytes.
        common::create_and_activate_channel(&app, "ch_q_ok", common::simple_log_workflow("Log"))
            .await;
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v2/data/ch_q_ok",
                Some(json!({"data": {"marker": "in-ok"}})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let (input, result) = stored_payloads(&pool, "ch_q_ok").await;
        assert!(input.contains("in-ok"), "{mode:?}: {input}");
        let stored: serde_json::Value = serde_json::from_str(&result).expect("result is JSON");
        assert_eq!(stored, body, "{mode:?}: a clean trace stores the body");

        // Errors: the body is the sanitized envelope; the trace keeps the
        // full-detail one, which carries the request_id-free original.
        let addr = common::start_failing_server().await;
        common::create_http_connector(&app, "q-fail", addr).await;
        common::create_and_activate_channel(
            &app,
            "ch_q_err",
            common::workflow_with_tasks(
                "Fail",
                json!([{
                    "id": "t1",
                    "name": "Call failing endpoint",
                    "on_failure": "continue",
                    "function": {
                        "name": "http_call",
                        "input": {
                            "connector": "q-fail",
                            "method": "POST",
                            "path": "/fail",
                            "output": "data.result",
                            "timeout_ms": 5000
                        }
                    }
                }]),
            ),
        )
        .await;
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v2/data/ch_q_err",
                Some(json!({"data": {"marker": "in-err"}})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert!(body["request_id"].is_string(), "{mode:?}: {body}");
        let (input, result) = stored_payloads(&pool, "ch_q_err").await;
        assert!(input.contains("in-err"), "{mode:?}: {input}");
        let stored: serde_json::Value = serde_json::from_str(&result).expect("result is JSON");
        assert!(stored.get("request_id").is_none(), "{mode:?}: {stored}");
        assert_eq!(stored["id"], body["id"], "{mode:?}");
        assert_eq!(
            stored["errors"][0]["code"], body["errors"][0]["code"],
            "{mode:?}: {stored}"
        );
        assert!(
            stored["errors"][0].get("path").is_some(),
            "{mode:?}: the stored errors are the full records: {stored}"
        );
    }
}
