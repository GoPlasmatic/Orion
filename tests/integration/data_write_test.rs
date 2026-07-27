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
