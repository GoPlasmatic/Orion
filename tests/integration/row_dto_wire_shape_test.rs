//! D27 / D28: the row → DTO split must not move a single byte on the wire.
//!
//! Four endpoints used to serialize a storage row struct directly — connectors
//! (masked), audit logs, and the two DLQ reads. Each now returns a DTO from
//! `storage::models::dto`, reached through a `From`. These tests pin the exact
//! top-level key set each response carries, so a field dropped, added or
//! renamed during the conversion fails here rather than in a client.
//!
//! The trace listing is pinned for the opposite reason: it must *keep* not
//! carrying four columns. Its query used to be `SELECT *`, which read every
//! caller's request body, the full engine message and one `access_token_hash`
//! — a credential verifier — out of the database for every row on every page.

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common;
use orion::config::AppConfig;
use orion::server::state::AppState;

/// Sorted top-level keys of a JSON object, for exact-set assertions.
#[track_caller]
fn keys(value: &Value, what: &str) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .unwrap_or_else(|| panic!("{what}: not a JSON object: {value}"))
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

async fn get(app: &axum::Router, uri: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(common::json_request("GET", uri, None))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK, "GET {uri}");
    common::body_json(resp).await
}

// ---------------------------------------------------------------------------
// Connectors — `ConnectorResponse`, built only by `mask_connector`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn connector_response_keeps_every_field_the_row_published() {
    let app = common::test_app().await;
    let id = common::create_connector(&app, common::db_connector("wire-shape-conn")).await;

    let body = get(&app, &format!("/api/v1/admin/connectors/{id}")).await;
    assert_eq!(
        keys(&body["data"], "connector"),
        [
            "config_json",
            "connector_type",
            "created_at",
            "enabled",
            "id",
            "name",
            "updated_at"
        ]
    );
    assert_eq!(body["data"]["id"], id);
    // Timestamps stay naive-UTC strings, not RFC 3339 with an offset.
    let created = body["data"]["created_at"].as_str().expect("created_at");
    assert!(
        !created.ends_with('Z') && !created.contains('+'),
        "created_at changed representation: {created}"
    );

    // The list adds exactly the three registry fields on top of the same row.
    let list = get(&app, "/api/v1/admin/connectors").await;
    let row = &list["data"][0];
    assert_eq!(
        keys(row, "connector list row"),
        [
            "config_json",
            "connector_type",
            "created_at",
            "enabled",
            "id",
            "load_status",
            "name",
            "updated_at"
        ]
    );
}

// ---------------------------------------------------------------------------
// Audit logs — `AuditLogEntryResponse`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn audit_log_response_keeps_every_field_the_row_published() {
    let app = common::test_app().await;
    // Any admin mutation writes one audit row.
    common::create_connector(&app, common::db_connector("wire-shape-audit")).await;

    let body = get(&app, "/api/v1/admin/audit-logs").await;
    let row = &body["data"][0];
    assert_eq!(
        keys(row, "audit log row"),
        [
            "action",
            "created_at",
            "details",
            "id",
            "principal",
            "resource_id",
            "resource_type"
        ]
    );
    // `details` was a plain `Option<String>` on the row — absent means a
    // present `null`, not a missing key.
    assert!(row.get("details").is_some());
}

// ---------------------------------------------------------------------------
// Trace DLQ — `TraceDlqEntryResponse` and `TraceDlqSummaryResponse`
// ---------------------------------------------------------------------------

async fn app_with_dlq_entry() -> (axum::Router, String) {
    let state: AppState = common::test_state_with_config(AppConfig::default()).await;
    let entry = state
        .trace_dlq_repo
        .enqueue(
            "trace-1",
            "orders",
            r#"{"order":"A-1"}"#,
            "{}",
            "boom",
            0,
            3,
        )
        .await
        .expect("enqueue");
    (orion::server::build_router(state), entry.id)
}

#[tokio::test]
async fn trace_dlq_responses_keep_every_field_the_rows_published() {
    let (app, id) = app_with_dlq_entry().await;

    let detail = get(&app, &format!("/api/v1/admin/trace-dlq/{id}")).await;
    assert_eq!(
        keys(&detail["data"], "dlq entry"),
        [
            "channel",
            "created_at",
            "error_message",
            "id",
            "max_retries",
            "metadata_json",
            "next_retry_at",
            "payload_json",
            "retry_count",
            "trace_id",
            "updated_at"
        ]
    );

    let list = get(&app, "/api/v1/admin/trace-dlq").await;
    assert_eq!(
        keys(&list["data"][0], "dlq summary"),
        [
            "channel",
            "created_at",
            "error_message",
            "id",
            "max_retries",
            "next_retry_at",
            "retry_count",
            "trace_id",
            "updated_at"
        ]
    );
}

// ---------------------------------------------------------------------------
// Traces — D27: the listing reads a narrow projection, not `SELECT *`
// ---------------------------------------------------------------------------

/// The four columns a trace listing must never read or return: three payload
/// columns and the capability-token hash.
const WITHHELD: [&str; 4] = [
    "input_json",
    "result_json",
    "task_trace_json",
    "access_token_hash",
];

#[tokio::test]
async fn trace_listing_carries_neither_payloads_nor_the_token_hash() {
    let app = common::test_app().await;
    common::create_and_activate_channel(
        &app,
        "d27-trace-list",
        common::echo_workflow("d27-trace-list-workflow"),
    )
    .await;

    // An async submission is what writes an `access_token_hash` (R12).
    common::submit_async(
        &app,
        "/api/v1/data/d27-trace-list/async",
        json!({"data": {"secret_field": "top-secret-payload"}}),
    )
    .await;

    let body = common::wait_for_body(&app, "/api/v1/admin/traces?limit=10", |b| {
        b["data"].as_array().is_some_and(|rows| !rows.is_empty())
    })
    .await;

    let rows = body["data"].as_array().expect("data array");
    for row in rows {
        assert_eq!(
            keys(row, "trace list row"),
            [
                "channel",
                "channel_id",
                "completed_at",
                "created_at",
                "duration_ms",
                "error_message",
                "id",
                "mode",
                "started_at",
                "status",
                "updated_at"
            ]
        );
        for withheld in WITHHELD {
            assert!(
                row.get(withheld).is_none(),
                "trace list row leaked `{withheld}`: {row}"
            );
        }
    }
    // Belt and braces: the submitted payload must not appear anywhere in the
    // listing, under any key.
    let serialized = body.to_string();
    assert!(
        !serialized.contains("top-secret-payload"),
        "the trace listing carried the submitted payload: {serialized}"
    );
}
