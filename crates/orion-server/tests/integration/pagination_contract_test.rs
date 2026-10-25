//! D18: every list endpoint pages the same way, because they all go through
//! one helper.
//!
//! Traces, the trace DLQ and audit logs each once re-implemented
//! count-then-page, so a pagination fix had to land several times and any one
//! of them could drift. They now share `helpers::paginate`, and this test is
//! what says so: it asserts one contract against each of them.
//!
//! The contract:
//!
//! - `total` counts every row matching the filter, ignoring `limit`/`offset`;
//! - `limit` is echoed after clamping to `[1, 1000]`, and `data` is no longer
//!   than it;
//! - `offset` is echoed and skips that many rows;
//! - an omitted `limit` is 50.

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common;
use orion::config::AppConfig;
use orion::server::state::AppState;

async fn get(app: &axum::Router, uri: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(common::json_request("GET", uri, None))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK, "GET {uri}");
    common::body_json(resp).await
}

/// Assert the whole contract for one endpoint seeded with `total` rows.
///
/// `base` is the endpoint path with no query string.
async fn assert_pagination_contract(app: &axum::Router, base: &str, total: i64) {
    assert!(total >= 3, "the contract needs at least three rows to page");
    let sep = if base.contains('?') { '&' } else { '?' };

    let page = get(app, base).await;
    assert_eq!(page["total"], total, "{base}: default page total");
    assert_eq!(page["limit"], 50, "{base}: absent limit defaults to 50");
    assert_eq!(page["offset"], 0, "{base}: absent offset defaults to 0");

    let page = get(app, &format!("{base}{sep}limit=2")).await;
    assert_eq!(
        page["total"], total,
        "{base}: total must ignore limit, not count the page"
    );
    assert_eq!(page["limit"], 2);
    assert_eq!(page["data"].as_array().expect("data").len(), 2);

    let page = get(app, &format!("{base}{sep}limit=2&offset=2")).await;
    assert_eq!(page["total"], total, "{base}: total must ignore offset");
    assert_eq!(page["offset"], 2);
    assert_eq!(
        page["data"].as_array().expect("data").len() as i64,
        (total - 2).min(2),
        "{base}: offset must skip rows"
    );

    // A page past the end is empty, not an error, and still reports `total`.
    let page = get(app, &format!("{base}{sep}limit=2&offset={total}")).await;
    assert_eq!(page["total"], total);
    assert!(page["data"].as_array().expect("data").is_empty());

    // Clamping: `[1, 1000]`, echoed as clamped.
    let page = get(app, &format!("{base}{sep}limit=0")).await;
    assert_eq!(page["limit"], 1, "{base}: limit clamps up to 1");
    assert_eq!(page["data"].as_array().expect("data").len(), 1);

    let page = get(app, &format!("{base}{sep}limit=99999")).await;
    assert_eq!(page["limit"], 1000, "{base}: limit clamps down to 1000");

    let page = get(app, &format!("{base}{sep}offset=-5")).await;
    assert_eq!(page["offset"], 0, "{base}: negative offset clamps to 0");
}

// ---------------------------------------------------------------------------
// 1: audit logs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn audit_log_list_pages_the_same_way() {
    let app = common::test_app().await;
    // One audit row per definition upload — through the async audit queue,
    // so wait for all three to land before asserting the contract.
    for i in 0..3 {
        common::deploy::upload(
            &app,
            common::deploy::workflow_definition(&common::simple_log_workflow(&format!(
                "page-audit-{i}"
            ))),
        )
        .await;
    }
    common::wait_for_body(&app, "/api/v2/admin/audit-logs", |b| b["total"] == 3).await;

    assert_pagination_contract(&app, "/api/v2/admin/audit-logs", 3).await;
}

// ---------------------------------------------------------------------------
// 2: trace DLQ — the one list with a narrow projection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn trace_dlq_list_pages_the_same_way() {
    let state: AppState = common::test_state_with_config(AppConfig::default()).await;
    for i in 0..3 {
        state
            .repos
            .trace_dlq
            .enqueue(orion::storage::repositories::trace_dlq::DlqEnqueue {
                trace_id: &format!("t-{i}"),
                channel: "orders",
                payload_json: "{}",
                metadata_json: "{}",
                error_message: "boom",
                retry_count: 0,
                max_retries: 3,
            })
            .await
            .expect("enqueue");
    }
    let app = orion::server::build_router(state);

    assert_pagination_contract(&app, "/api/v2/admin/trace-dlq", 3).await;
}

// ---------------------------------------------------------------------------
// 3: traces — the other narrow projection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn trace_list_pages_the_same_way() {
    let app = common::test_app().await;
    common::create_and_activate_channel(
        &app,
        "page-traces",
        common::echo_workflow("page-traces-workflow"),
    )
    .await;

    for i in 0..3 {
        common::submit_async(
            &app,
            "/api/v2/data/page-traces/async",
            json!({"data": {"n": i}}),
        )
        .await;
    }
    common::wait_for_body(&app, "/api/v2/admin/traces?include_total=true", |b| {
        b["total"] == 3
    })
    .await;

    // Traces are the one endpoint whose `total` is opt-in (D8): the count is a
    // full scan of the filtered set on Postgres and InnoDB, and a 10M-row table
    // was paying it on every page. Asking for it restores the shared contract
    // exactly.
    assert_pagination_contract(&app, "/api/v2/admin/traces?include_total=true", 3).await;

    // And the deviation itself: no flag, no count — but the rest of the
    // envelope is unchanged, so a caller that never reads `total` sees the
    // same shape it always did.
    let page = get(&app, "/api/v2/admin/traces").await;
    assert!(
        page["total"].is_null(),
        "traces must not count unless asked: {page}"
    );
    assert_eq!(page["limit"], 50);
    assert_eq!(page["offset"], 0);
    assert_eq!(page["data"].as_array().expect("data").len(), 3);
}
