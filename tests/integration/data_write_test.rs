//! End-to-end tests for the `data_write` handler (portable INSERT / UPDATE /
//! DELETE / upsert). Each test drives an in-memory SQLite connector: raw
//! `db_write` creates the table, `data_write` mutates it, and a `data_query`
//! reads the result back — proving the sea-query write → `AnyPool` path and the
//! safety guards end-to-end.

use crate::common;
use crate::common::dsl::{ddl, dq, dw, is_rejection, post};

use axum::http::StatusCode;
use orion::config::{AppConfig, WriteConfig};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn sqlite_app(conn: &str, mem: &str) -> axum::Router {
    let app = common::test_app().await;
    common::create_connector(
        &app,
        common::db_connector_sqlite(conn, &format!("sqlite:file:{mem}?mode=memory&cache=shared")),
    )
    .await;
    app
}

async fn run(app: &axum::Router, channel: &str, tasks: Vec<Value>) -> Value {
    common::create_and_activate_channel(
        app,
        channel,
        common::workflow_with_tasks("dw", json!(tasks)),
    )
    .await;
    let (status, body) = post(app, channel, json!({ "data": {} })).await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert_eq!(body["status"], "ok", "body = {body}");
    body
}

#[tokio::test]
async fn test_insert_then_read_back() {
    let conn = "dw-ins";
    let app = sqlite_app(conn, "dw_ins").await;

    let body = run(
        &app,
        "ch-dw-ins",
        vec![
            ddl(conn, "t_ddl", "CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY, name TEXT, age INTEGER)"),
            dw(conn, "t_w", json!({
                "op": "insert", "target": "users",
                "values": [ { "id": "u1", "name": "Alice", "age": 30 }, { "id": "u2", "name": "Bob", "age": 20 } ]
            })),
            dq(conn, "t_r", json!({ "source": "users", "sort": [{ "id": "asc" }] })),
        ],
    )
    .await;

    assert_eq!(body["data"]["w"]["rows_affected"], 2, "body = {body}");
    let rows = body["data"]["result"].as_array().expect("array");
    assert_eq!(rows.len(), 2, "body = {body}");
    assert_eq!(rows[0]["id"], "u1");
    assert_eq!(rows[0]["name"], "Alice");
    assert_eq!(rows[1]["id"], "u2");
}

#[tokio::test]
async fn test_update_with_filter() {
    let conn = "dw-upd";
    let app = sqlite_app(conn, "dw_upd").await;

    let body = run(
        &app,
        "ch-dw-upd",
        vec![
            ddl(conn, "t_ddl", "CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY, status TEXT)"),
            dw(conn, "t_s1", json!({ "op": "insert", "target": "users", "values": { "id": "u1", "status": "active" } })),
            dw(conn, "t_s2", json!({ "op": "insert", "target": "users", "values": { "id": "u2", "status": "active" } })),
            dw(conn, "t_w", json!({
                "op": "update", "target": "users",
                "set": { "status": "inactive" },
                "filter": { "==": [{ "field": "id" }, "u1"] }
            })),
            dq(conn, "t_r", json!({ "source": "users", "sort": [{ "id": "asc" }] })),
        ],
    )
    .await;

    assert_eq!(body["data"]["w"]["rows_affected"], 1, "body = {body}");
    let rows = body["data"]["result"].as_array().expect("array");
    assert_eq!(rows[0]["status"], "inactive"); // u1 updated
    assert_eq!(rows[1]["status"], "active"); // u2 untouched
}

#[tokio::test]
async fn test_update_param_from_message() {
    let conn = "dw-param";
    let app = sqlite_app(conn, "dw_param").await;

    // Bring the request payload into `data.req`, then update with params folded in.
    let tasks = vec![
        json!({ "id": "t_parse", "name": "parse",
            "function": { "name": "parse_json", "input": { "source": "payload", "target": "req" } } }),
        ddl(
            conn,
            "t_ddl",
            "CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY, status TEXT)",
        ),
        dw(
            conn,
            "t_s1",
            json!({ "op": "insert", "target": "users", "values": { "id": "u1", "status": "active" } }),
        ),
        dw(
            conn,
            "t_w",
            json!({
                "op": "update", "target": "users",
                "set": { "status": { "param": "s" } },
                "filter": { "==": [{ "field": "id" }, { "param": "id" }] },
                "params": { "s": { "var": "data.req.new_status" }, "id": { "var": "data.req.who" } }
            }),
        ),
        dq(conn, "t_r", json!({ "source": "users" })),
    ];
    common::create_and_activate_channel(
        &app,
        "ch-dw-param",
        common::workflow_with_tasks("dw", json!(tasks)),
    )
    .await;

    let (status, body) = post(
        &app,
        "ch-dw-param",
        json!({ "data": { "new_status": "banned", "who": "u1" } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    let rows = body["data"]["result"].as_array().expect("array");
    assert_eq!(rows[0]["status"], "banned", "body = {body}");
}

#[tokio::test]
async fn test_delete_with_filter() {
    let conn = "dw-del";
    let app = sqlite_app(conn, "dw_del").await;

    let body = run(
        &app,
        "ch-dw-del",
        vec![
            ddl(
                conn,
                "t_ddl",
                "CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY)",
            ),
            dw(
                conn,
                "t_s1",
                json!({ "op": "insert", "target": "users", "values": { "id": "u1" } }),
            ),
            dw(
                conn,
                "t_s2",
                json!({ "op": "insert", "target": "users", "values": { "id": "u2" } }),
            ),
            dw(
                conn,
                "t_w",
                json!({
                    "op": "delete", "target": "users",
                    "filter": { "==": [{ "field": "id" }, "u1"] }
                }),
            ),
            dq(conn, "t_r", json!({ "source": "users" })),
        ],
    )
    .await;

    assert_eq!(body["data"]["w"]["rows_affected"], 1, "body = {body}");
    let rows = body["data"]["result"].as_array().expect("array");
    assert_eq!(rows.len(), 1, "body = {body}");
    assert_eq!(rows[0]["id"], "u2");
}

#[tokio::test]
async fn test_upsert_insert_then_update() {
    let conn = "dw-ups";
    let app = sqlite_app(conn, "dw_ups").await;

    let body = run(
        &app,
        "ch-dw-ups",
        vec![
            ddl(
                conn,
                "t_ddl",
                "CREATE TABLE IF NOT EXISTS users (email TEXT PRIMARY KEY, name TEXT)",
            ),
            // First upsert inserts the row.
            dw(
                conn,
                "t_w1",
                json!({
                    "op": "upsert", "target": "users",
                    "values": { "email": "a@x.io", "name": "Ada" },
                    "on_conflict": { "target": ["email"], "action": "update" }
                }),
            ),
            // Second upsert on the same key updates the name.
            dw(
                conn,
                "t_w2",
                json!({
                    "op": "upsert", "target": "users",
                    "values": { "email": "a@x.io", "name": "Ada Lovelace" },
                    "on_conflict": { "target": ["email"], "action": "update" }
                }),
            ),
            dq(conn, "t_r", json!({ "source": "users" })),
        ],
    )
    .await;

    let rows = body["data"]["result"].as_array().expect("array");
    assert_eq!(rows.len(), 1, "upsert must not duplicate: body = {body}");
    assert_eq!(rows[0]["name"], "Ada Lovelace");
}

#[tokio::test]
async fn test_insert_returning() {
    let conn = "dw-ret";
    let app = sqlite_app(conn, "dw_ret").await;

    let body = run(
        &app,
        "ch-dw-ret",
        vec![
            ddl(conn, "t_ddl", "CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT)"),
            dw(conn, "t_w", json!({
                "op": "insert", "target": "items",
                "values": { "name": "Widget" },
                "returning": ["id", "name"]
            })),
        ],
    )
    .await;

    let ret = body["data"]["w"]["returning"]
        .as_array()
        .expect("returning array");
    assert_eq!(ret.len(), 1, "body = {body}");
    assert_eq!(ret[0]["name"], "Widget");
    assert_eq!(ret[0]["id"], 1);
}

#[tokio::test]
async fn test_unfiltered_update_rejected() {
    let conn = "dw-unf";
    let app = sqlite_app(conn, "dw_unf").await;

    // An update with no filter and no `all` acknowledgement must be rejected.
    let tasks = vec![
        ddl(
            conn,
            "t_ddl",
            "CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY, status TEXT)",
        ),
        dw(
            conn,
            "t_w",
            json!({ "op": "update", "target": "users", "set": { "status": "x" } }),
        ),
    ];
    common::create_and_activate_channel(
        &app,
        "ch-dw-unf",
        common::workflow_with_tasks("dw", json!(tasks)),
    )
    .await;

    let (status, body) = post(&app, "ch-dw-unf", json!({ "data": {} })).await;
    assert!(
        is_rejection(status, &body),
        "unfiltered update must be rejected, got status={status} body={body}"
    );
}

#[tokio::test]
async fn test_vacuous_filter_does_not_bypass_the_unfiltered_guard() {
    // A filter that matches every row must be treated as no filter at all.
    // Before proposal W1 the guard keyed on the presence of the `filter` key,
    // so any of these deleted the whole table while skipping both the
    // `"all": true` acknowledgement and `write.allow_unfiltered`.
    for (label, vacuous) in [
        ("empty and", json!({ "and": [] })),
        ("negated empty or", json!({ "!": { "or": [] } })),
        ("nested empty and", json!({ "and": [{ "and": [] }] })),
    ] {
        let conn = "dw-vac";
        let app = sqlite_app(conn, "dw_vac").await;

        let tasks = vec![
            ddl(
                conn,
                "t_ddl",
                "CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY, status TEXT)",
            ),
            dw(
                conn,
                "t_w",
                json!({ "op": "delete", "target": "users", "filter": vacuous }),
            ),
        ];
        common::create_and_activate_channel(
            &app,
            "ch-dw-vac",
            common::workflow_with_tasks("dw", json!(tasks)),
        )
        .await;

        let (status, body) = post(&app, "ch-dw-vac", json!({ "data": {} })).await;
        assert!(
            is_rejection(status, &body),
            "a '{label}' filter restricts nothing and must be rejected as unfiltered, \
             got status={status} body={body}"
        );
    }
}

#[tokio::test]
async fn test_unfiltered_update_allowed_with_all_and_config() {
    // With write.allow_unfiltered on and `"all": true`, an unfiltered update runs.
    let config = AppConfig {
        write: WriteConfig {
            max_rows: 1000,
            allow_unfiltered: true,
        },
        ..Default::default()
    };
    let app = common::test_app_with_config(config).await;
    let conn = "dw-all";
    common::create_connector(
        &app,
        common::db_connector_sqlite(conn, "sqlite:file:dw_all?mode=memory&cache=shared"),
    )
    .await;

    let body = run(
        &app,
        "ch-dw-all",
        vec![
            ddl(conn, "t_ddl", "CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY, status TEXT)"),
            dw(conn, "t_s1", json!({ "op": "insert", "target": "users", "values": { "id": "u1", "status": "a" } })),
            dw(conn, "t_s2", json!({ "op": "insert", "target": "users", "values": { "id": "u2", "status": "a" } })),
            dw(conn, "t_w", json!({ "op": "update", "target": "users", "set": { "status": "z" }, "all": true })),
            dq(conn, "t_r", json!({ "source": "users" })),
        ],
    )
    .await;

    assert_eq!(body["data"]["w"]["rows_affected"], 2, "body = {body}");
    let rows = body["data"]["result"].as_array().expect("array");
    assert!(rows.iter().all(|r| r["status"] == "z"), "body = {body}");
}

#[tokio::test]
async fn test_bulk_insert_over_max_rows_rejected() {
    // Hard cap of 1 row per insert; a two-row bulk insert must be rejected.
    let config = AppConfig {
        write: WriteConfig {
            max_rows: 1,
            allow_unfiltered: false,
        },
        ..Default::default()
    };
    let app = common::test_app_with_config(config).await;
    let conn = "dw-cap";
    common::create_connector(
        &app,
        common::db_connector_sqlite(conn, "sqlite:file:dw_cap?mode=memory&cache=shared"),
    )
    .await;

    let tasks = vec![
        ddl(
            conn,
            "t_ddl",
            "CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY)",
        ),
        dw(
            conn,
            "t_w",
            json!({
                "op": "insert", "target": "users",
                "values": [ { "id": "u1" }, { "id": "u2" } ]
            }),
        ),
    ];
    common::create_and_activate_channel(
        &app,
        "ch-dw-cap",
        common::workflow_with_tasks("dw", json!(tasks)),
    )
    .await;

    let (status, body) = post(&app, "ch-dw-cap", json!({ "data": {} })).await;
    assert!(
        is_rejection(status, &body),
        "bulk insert over max_rows must be rejected, got status={status} body={body}"
    );
}

// ---------------------------------------------------------------------------
// W7: the mutation envelope is nested under `write`
// ---------------------------------------------------------------------------

/// Every other test in this file goes through `dsl::dw`, which builds the
/// nested 1.0 shape. This one writes both shapes out by hand and asserts they
/// produce the same rows, so the compatibility path cannot rot unnoticed.
#[tokio::test]
async fn flat_and_nested_envelopes_write_the_same_rows() {
    async fn insert_via(app: &axum::Router, channel: &str, table: &str, input: Value) -> Value {
        common::create_and_activate_channel(
            app,
            channel,
            common::workflow_with_tasks(
                "dw",
                json!([
                    ddl(
                        "dw-shapes",
                        "ddl",
                        &format!("CREATE TABLE IF NOT EXISTS {table} (id INTEGER, name TEXT)")
                    ),
                    json!({
                        "id": "w", "name": "w",
                        "function": { "name": "data_write", "input": input }
                    }),
                    dq("dw-shapes", "read", json!({ "source": table })),
                ]),
            ),
        )
        .await;
        let (status, body) = post(app, channel, json!({ "data": {} })).await;
        assert_eq!(status, StatusCode::OK, "body = {body}");
        body["data"]["result"].clone()
    }

    let app = sqlite_app("dw-shapes", "dw_shapes").await;

    // 1.0: envelope under `write`, handler keys alongside it.
    let nested = insert_via(
        &app,
        "ch-dw-nested",
        "shapes_nested",
        json!({
            "connector": "dw-shapes",
            "output": "data.w",
            "write": { "op": "insert", "target": "shapes_nested", "values": { "id": 1, "name": "a" } }
        }),
    )
    .await;

    // Pre-1.0: envelope flat, sharing the namespace with the handler keys.
    let flat = insert_via(
        &app,
        "ch-dw-flat",
        "shapes_flat",
        json!({
            "connector": "dw-shapes",
            "output": "data.w",
            "op": "insert",
            "target": "shapes_flat",
            "values": { "id": 1, "name": "a" }
        }),
    )
    .await;

    assert_eq!(
        nested,
        json!([{ "id": 1, "name": "a" }]),
        "nested = {nested}"
    );
    assert_eq!(flat, nested, "the deprecated flat form must agree: {flat}");
}

/// `write` wins when a task carries both, so a half-migrated task cannot
/// silently execute the stale envelope.
#[tokio::test]
async fn nested_envelope_wins_when_both_shapes_are_present() {
    let app = sqlite_app("dw-both", "dw_both").await;
    common::create_and_activate_channel(
        &app,
        "ch-dw-both",
        common::workflow_with_tasks(
            "dw",
            json!([
                ddl(
                    "dw-both",
                    "ddl",
                    "CREATE TABLE IF NOT EXISTS both_t (id INTEGER, name TEXT)"
                ),
                json!({
                    "id": "w", "name": "w",
                    "function": { "name": "data_write", "input": {
                        "connector": "dw-both",
                        "output": "data.w",
                        // Stale flat keys left behind by a partial migration.
                        "op": "insert",
                        "target": "both_t",
                        "values": { "id": 99, "name": "stale" },
                        // The authoritative envelope.
                        "write": { "op": "insert", "target": "both_t", "values": { "id": 1, "name": "fresh" } }
                    }}
                }),
                dq("dw-both", "read", json!({ "source": "both_t" })),
            ]),
        ),
    )
    .await;

    let (status, body) = post(&app, "ch-dw-both", json!({ "data": {} })).await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert_eq!(
        body["data"]["result"],
        json!([{ "id": 1, "name": "fresh" }]),
        "body = {body}"
    );
}

/// A task with neither shape is rejected at create time, naming `write`.
#[tokio::test]
async fn a_data_write_without_an_envelope_is_rejected_at_create() {
    let app = common::test_app().await;
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(common::workflow_with_tasks(
                "dw",
                json!([{
                    "id": "w", "name": "w",
                    "function": { "name": "data_write", "input": { "connector": "c" } }
                }]),
            )),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = common::body_json(resp).await;
    let details = body["error"]["details"].as_array().expect("field details");
    assert!(
        details.iter().any(|d| d["path"]
            .as_str()
            .is_some_and(|f| f.ends_with("function.input.write"))),
        "expected a field error on `write`, got {details:?}"
    );
}

/// A malformed nested envelope reports paths *inside* `write`, which is the
/// point of having one JSON value that is the envelope.
#[tokio::test]
async fn envelope_errors_are_reported_under_the_write_path() {
    let app = common::test_app().await;
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(common::workflow_with_tasks(
                "dw",
                json!([{
                    "id": "w", "name": "w",
                    "function": { "name": "data_write", "input": {
                        "connector": "c",
                        "write": { "op": "insert" }   // `target` missing
                    }}
                }]),
            )),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = common::body_json(resp).await;
    let details = body["error"]["details"].as_array().expect("field details");
    assert!(
        details.iter().any(|d| d["path"]
            .as_str()
            .is_some_and(|f| f.ends_with("function.input.write.target"))),
        "expected a field error on `write.target`, got {details:?}"
    );
}
