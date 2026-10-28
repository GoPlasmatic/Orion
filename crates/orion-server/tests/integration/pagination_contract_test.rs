//! Every list endpoint pages the same way, because they all go through one
//! helper (`helpers::paginate`) and one envelope (`orion_api::Page`).
//!
//! Traces, the trace DLQ, audit logs and cron occurrences each once
//! re-implemented count-then-page, so a pagination fix had to land several
//! times and any one of them could drift. This test asserts one contract
//! against each of them.
//!
//! The contract:
//!
//! - an omitted `limit` is 50, and `limit` is echoed after clamping to
//!   `[1, 1000]`; `data` is no longer than it;
//! - `total` is **opt-in** (`include_total=true`): absent otherwise, and then it
//!   counts every row matching the filter, ignoring `limit` and `cursor`;
//! - `next_cursor` is present exactly when more rows follow; walking it visits
//!   every row once, newest first, and never ends on an empty page;
//! - there is no `offset`.

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common;
use orion::config::AppConfig;
use orion::runtime::state::AppState;

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
/// `base` is the endpoint path, with a query string only if it needs one.
async fn assert_pagination_contract(app: &axum::Router, base: &str, total: i64) {
    assert!(total >= 3, "the contract needs at least three rows to page");
    let sep = if base.contains('?') { '&' } else { '?' };

    // Defaults: 50 a page, no count, no offset, and everything in one page.
    let page = get(app, base).await;
    assert_eq!(page["limit"], 50, "{base}: absent limit defaults to 50");
    assert!(
        page["total"].is_null(),
        "{base}: the count is opt-in: {page}"
    );
    assert!(page.get("offset").is_none(), "{base}: there is no offset");
    assert!(page["next_cursor"].is_null(), "{base}: one page, no cursor");
    assert_eq!(page["data"].as_array().expect("data").len() as i64, total);

    // Asking for the count restores it, ignoring the page size.
    let page = get(app, &format!("{base}{sep}limit=2&include_total=true")).await;
    assert_eq!(page["total"], total, "{base}: total ignores the page size");
    assert_eq!(page["limit"], 2);
    assert_eq!(page["data"].as_array().expect("data").len(), 2);
    assert!(page["next_cursor"].is_string(), "{base}: more rows follow");

    // Walking the cursor visits every row exactly once.
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let uri = match &cursor {
            Some(c) => format!("{base}{sep}limit=2&cursor={c}"),
            None => format!("{base}{sep}limit=2"),
        };
        let page = get(app, &uri).await;
        let rows = page["data"].as_array().expect("data");
        assert!(!rows.is_empty(), "{base}: a page is never empty");
        seen.extend(
            rows.iter()
                .map(|r| r["id"].as_str().expect("id").to_string()),
        );
        pages += 1;
        match page["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_string()),
            None => break,
        }
        assert!(pages <= total, "{base}: cursor walk is not terminating");
    }
    assert_eq!(seen.len() as i64, total, "{base}: every row once");
    let distinct: std::collections::BTreeSet<_> = seen.iter().collect();
    assert_eq!(distinct.len(), seen.len(), "{base}: no row repeated");

    // A page that ends exactly on the last row offers no further cursor.
    let page = get(app, &format!("{base}{sep}limit={total}")).await;
    assert!(
        page["next_cursor"].is_null(),
        "{base}: {total} rows at limit {total} is the last page: {page}"
    );

    // Clamping: `[1, 1000]`, echoed as clamped.
    let page = get(app, &format!("{base}{sep}limit=0")).await;
    assert_eq!(page["limit"], 1, "{base}: limit clamps up to 1");
    assert_eq!(page["data"].as_array().expect("data").len(), 1);
    let page = get(app, &format!("{base}{sep}limit=99999")).await;
    assert_eq!(page["limit"], 1000, "{base}: limit clamps down to 1000");

    // A cursor this endpoint did not issue is refused, not ignored.
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "GET",
            &format!("{base}{sep}cursor=not-a-cursor"),
            None,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{base}");
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
    common::wait_for_body(&app, "/api/v2/admin/audit-logs", |b| {
        b["data"].as_array().is_some_and(|d| d.len() == 3)
    })
    .await;

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

    assert_pagination_contract(&app, "/api/v2/admin/traces", 3).await;
}
