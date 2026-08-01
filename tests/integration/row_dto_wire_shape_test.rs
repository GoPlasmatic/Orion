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
        .repos
        .trace_dlq
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

// ---------------------------------------------------------------------------
// D26 — `workflows.tags` → `tags_json`, `channels.methods` → `methods_json`
// ---------------------------------------------------------------------------

/// The two renamed columns are physical only: the wire keeps `tags` and
/// `methods` (D26).
///
/// This is the assertion the rename is worth nothing without. `tags_json` and
/// `methods_json` are storage names, chosen so `_json` means one thing across
/// the whole schema; the admin API's field names are a public contract and did
/// not move. The two halves are checked against each other in one test on
/// purpose — a response that still says `tags` proves nothing on its own if
/// the column underneath never moved, and a moved column proves nothing if it
/// took the wire name with it.
#[tokio::test]
async fn wire_names_survive_the_column_rename() {
    let state = common::test_state_with_config(AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());

    // -- workflow: `tags` on the wire --
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "D26 wire shape",
                "tags": ["production", "d26"],
                "tasks": [{"id":"t1","name":"Log",
                           "function":{"name":"log","input":{"message":"x"}}}],
            })),
        ))
        .await
        .expect("create workflow");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = common::body_json(resp).await;
    let workflow_id = created["data"]["workflow_id"]
        .as_str()
        .expect("workflow_id")
        .to_string();

    let fetched = get(&app, &format!("/api/v1/admin/workflows/{workflow_id}")).await;
    for (what, body) in [("create", &created), ("read", &fetched)] {
        assert_eq!(
            keys(&body["data"], "workflow"),
            [
                "condition",
                "continue_on_error",
                "created_at",
                "description",
                "name",
                "priority",
                "rollout_percentage",
                "status",
                "tags",
                "tasks",
                "updated_at",
                "version",
                "workflow_id",
            ],
            "the {what} response must publish `tags`, never `tags_json`"
        );
        assert_eq!(body["data"]["tags"], json!(["production", "d26"]));
        assert!(body["data"].get("tags_json").is_none());
    }

    // -- channel: `methods` on the wire --
    let channel_id = common::create_rest_channel(
        &app,
        "d26-wire-shape",
        "/d26-wire-shape",
        vec!["GET", "POST"],
        &workflow_id,
    )
    .await;

    let fetched = get(&app, &format!("/api/v1/admin/channels/{channel_id}")).await;
    assert_eq!(
        keys(&fetched["data"], "channel"),
        [
            "channel_id",
            "channel_type",
            "config",
            "consumer_group",
            "created_at",
            "description",
            "methods",
            "name",
            "priority",
            "protocol",
            "route_pattern",
            "status",
            "topic",
            "transport_config",
            "updated_at",
            "version",
            "workflow_id",
        ],
        "the channel response must publish `methods`, never `methods_json`"
    );
    assert_eq!(fetched["data"]["methods"], json!(["GET", "POST"]));
    assert!(fetched["data"].get("methods_json").is_none());

    // -- and underneath, the columns really did move --
    let orion::storage::DbPool::Sqlite(db) = &state.db_pool else {
        panic!("the integration binary pins SQLite");
    };

    for (object, gone, present) in [
        ("workflows", "tags", "tags_json"),
        ("current_workflows", "tags", "tags_json"),
        ("channels", "methods", "methods_json"),
        ("current_channels", "methods", "methods_json"),
    ] {
        let columns: Vec<(String,)> = sqlx::query_as("SELECT name FROM pragma_table_info(?)")
            .bind(object)
            .fetch_all(db)
            .await
            .unwrap_or_else(|e| panic!("introspect {object}: {e}"));
        let columns: Vec<String> = columns.into_iter().map(|(c,)| c).collect();
        assert!(
            columns.iter().any(|c| c == present),
            "{object} must expose `{present}`, has {columns:?}"
        );
        assert!(
            !columns.iter().any(|c| c == gone),
            "{object} still exposes the old `{gone}` — on Postgres and MySQL a view \
             keeps the pre-rename name unless it is dropped and recreated, which is \
             the whole point of migrations postgres/013 and mysql/011: {columns:?}"
        );
    }

    // The values landed in the renamed columns, not merely near them.
    let (tags,): (String,) =
        sqlx::query_as("SELECT tags_json FROM current_workflows WHERE workflow_id = ?")
            .bind(&workflow_id)
            .fetch_one(db)
            .await
            .expect("read tags_json");
    assert_eq!(tags, r#"["production","d26"]"#);

    let (methods,): (Option<String>,) =
        sqlx::query_as("SELECT methods_json FROM current_channels WHERE channel_id = ?")
            .bind(&channel_id)
            .fetch_one(db)
            .await
            .expect("read methods_json");
    assert_eq!(methods.as_deref(), Some(r#"["GET","POST"]"#));
}

/// Filtering by tag still reaches the renamed column (D26).
///
/// `?tag=` builds a `LIKE` over the column directly rather than going through
/// a row struct, so it is the one read path a field rename alone would not
/// have fixed — and on SQLite a `LIKE` against a column that does not exist is
/// an error, not an empty page, so this fails loudly if the predicate is left
/// pointing at `tags`.
#[tokio::test]
async fn tag_filtering_reaches_the_renamed_column() {
    let app = common::test_app().await;

    for (name, tag) in [("D26 tagged A", "d26-keep"), ("D26 tagged B", "d26-drop")] {
        let resp = app
            .clone()
            .oneshot(common::json_request(
                "POST",
                "/api/v1/admin/workflows",
                Some(json!({
                    "name": name,
                    "tags": [tag],
                    "tasks": [{"id":"t1","name":"Log",
                               "function":{"name":"log","input":{"message":"x"}}}],
                })),
            ))
            .await
            .expect("create workflow");
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    let body = get(&app, "/api/v1/admin/workflows?tag=d26-keep").await;
    let rows = body["data"].as_array().expect("data array");
    assert_eq!(rows.len(), 1, "exactly the tagged workflow: {body}");
    assert_eq!(rows[0]["tags"], json!(["d26-keep"]));
}
