//! Per-connector operation gates: a db/es connector config can disable
//! individual operations (`read` / `insert` / `update` / `delete` / `upsert` /
//! `raw_write`) so a connector can be made read-only (or insert-only, …)
//! without touching workflows. Gated calls are rejected with a validation
//! error naming the operation and connector.
//!
//! Each test drives two in-memory SQLite connectors over the *same* shared DB:
//! an unrestricted "admin" connector for DDL/seeding, and a gated one that the
//! assertions target.

use crate::common;

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

/// A sqlite db connector with explicit operation gates over a shared in-memory DB.
fn gated_connector(name: &str, mem: &str, ops: Value) -> Value {
    json!({
        "id": name, "name": name, "connector_type": "db",
        "config": {
            "type": "db",
            "connection_string": format!("sqlite:file:{mem}?mode=memory&cache=shared"),
            "driver": "sqlite",
            "operations": ops
        }
    })
}

fn ddl(conn: &str, id: &str, sql: &str) -> Value {
    json!({
        "id": id, "name": id,
        "function": { "name": "db_write", "input": { "connector": conn, "query": sql, "output": "data.ddl" } }
    })
}

fn dw(conn: &str, id: &str, mut input: Value) -> Value {
    input["connector"] = json!(conn);
    input["output"] = json!("data.w");
    json!({ "id": id, "name": id, "function": { "name": "data_write", "input": input } })
}

fn dq(conn: &str, id: &str, query: Value) -> Value {
    json!({
        "id": id, "name": id,
        "function": { "name": "data_query", "input": { "connector": conn, "query": query, "output": "data.result" } }
    })
}

async fn post(app: &axum::Router, channel: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            &format!("/api/v1/data/{channel}"),
            Some(body),
        ))
        .await
        .unwrap();
    let status = resp.status();
    (status, common::body_json(resp).await)
}

/// Whether a response signals the gate rejection (mirrors data_write tests) and
/// carries the "disabled on connector" message.
fn is_gate_rejection(status: StatusCode, body: &Value) -> bool {
    let rejected = body
        .get("errors")
        .and_then(|e| e.as_array())
        .is_some_and(|a| !a.is_empty())
        || body.get("error").is_some_and(|e| e.get("code").is_some())
        || status.is_server_error();
    rejected && body.to_string().contains("disabled on connector")
}

/// App with an unrestricted admin connector (DDL + seed ran) and a gated
/// connector, both over the same shared in-memory DB.
async fn app_with_gated(mem: &str, admin: &str, gated: &str, ops: Value) -> axum::Router {
    let app = common::test_app().await;
    common::create_connector(&app, gated_connector(admin, mem, json!({}))).await;
    common::create_connector(&app, gated_connector(gated, mem, ops)).await;

    let tasks = vec![
        ddl(
            admin,
            "t_ddl",
            "CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY, name TEXT, status TEXT)",
        ),
        dw(
            admin,
            "t_seed",
            json!({
                "op": "insert", "target": "users",
                "values": { "id": "u1", "name": "Alice", "status": "active" }
            }),
        ),
    ];
    let channel = format!("ch-{admin}-setup");
    common::create_and_activate_channel(
        &app,
        &channel,
        common::workflow_with_tasks("setup", json!(tasks)),
    )
    .await;
    let (status, body) = post(&app, &channel, json!({ "data": {} })).await;
    assert_eq!(status, StatusCode::OK, "setup body = {body}");
    assert_eq!(body["status"], "ok", "setup body = {body}");
    app
}

/// Run a single task through its own channel and return the response.
async fn run_task(app: &axum::Router, channel: &str, task: Value) -> (StatusCode, Value) {
    common::create_and_activate_channel(
        app,
        channel,
        common::workflow_with_tasks("ops", json!([task])),
    )
    .await;
    post(app, channel, json!({ "data": {} })).await
}

#[tokio::test]
async fn test_disabled_update_and_delete_rejected_insert_still_allowed() {
    let app = app_with_gated(
        "ops_ud",
        "ops-ud-admin",
        "ops-ud",
        json!({ "update": false, "delete": false }),
    )
    .await;

    // update → gated.
    let (status, body) = run_task(
        &app,
        "ch-ops-ud-upd",
        dw(
            "ops-ud",
            "t_u",
            json!({
                "op": "update", "target": "users",
                "set": { "status": "inactive" },
                "filter": { "==": [{ "field": "id" }, "u1"] }
            }),
        ),
    )
    .await;
    assert!(
        is_gate_rejection(status, &body),
        "disabled update must be rejected: status={status} body={body}"
    );

    // delete → gated.
    let (status, body) = run_task(
        &app,
        "ch-ops-ud-del",
        dw(
            "ops-ud",
            "t_d",
            json!({
                "op": "delete", "target": "users",
                "filter": { "==": [{ "field": "id" }, "u1"] }
            }),
        ),
    )
    .await;
    assert!(
        is_gate_rejection(status, &body),
        "disabled delete must be rejected: status={status} body={body}"
    );

    // insert stays allowed on the same connector.
    let (status, body) = run_task(
        &app,
        "ch-ops-ud-ins",
        dw(
            "ops-ud",
            "t_i",
            json!({
                "op": "insert", "target": "users",
                "values": { "id": "u2", "name": "Bob", "status": "active" }
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert_eq!(body["status"], "ok", "insert should pass: {body}");
    assert_eq!(body["data"]["w"]["rows_affected"], 1, "body = {body}");
}

#[tokio::test]
async fn test_disabled_upsert_rejected() {
    let app = app_with_gated(
        "ops_ups",
        "ops-ups-admin",
        "ops-ups",
        json!({ "upsert": false }),
    )
    .await;

    let (status, body) = run_task(
        &app,
        "ch-ops-ups",
        dw(
            "ops-ups",
            "t_u",
            json!({
                "op": "upsert", "target": "users",
                "values": { "id": "u1", "name": "Alice2", "status": "active" },
                "on_conflict": { "target": ["id"], "action": "update" }
            }),
        ),
    )
    .await;
    assert!(
        is_gate_rejection(status, &body),
        "disabled upsert must be rejected: status={status} body={body}"
    );
}

#[tokio::test]
async fn test_disabled_raw_write_rejected() {
    let app = app_with_gated(
        "ops_raw",
        "ops-raw-admin",
        "ops-raw",
        json!({ "raw_write": false }),
    )
    .await;

    let (status, body) = run_task(
        &app,
        "ch-ops-raw",
        ddl("ops-raw", "t_raw", "DELETE FROM users"),
    )
    .await;
    assert!(
        is_gate_rejection(status, &body),
        "disabled raw_write must gate db_write: status={status} body={body}"
    );
}

#[tokio::test]
async fn test_disabled_read_rejects_data_query_and_db_read() {
    let app = app_with_gated("ops_rd", "ops-rd-admin", "ops-rd", json!({ "read": false })).await;

    // data_query → gated.
    let (status, body) = run_task(
        &app,
        "ch-ops-rd-dq",
        dq("ops-rd", "t_q", json!({ "source": "users" })),
    )
    .await;
    assert!(
        is_gate_rejection(status, &body),
        "disabled read must gate data_query: status={status} body={body}"
    );

    // db_read → gated.
    let (status, body) = run_task(
        &app,
        "ch-ops-rd-dbr",
        json!({
            "id": "t_r", "name": "t_r",
            "function": { "name": "db_read", "input": {
                "connector": "ops-rd", "query": "SELECT * FROM users", "output": "data.rows"
            } }
        }),
    )
    .await;
    assert!(
        is_gate_rejection(status, &body),
        "disabled read must gate db_read: status={status} body={body}"
    );

    // The write side of the same connector is untouched.
    let (status, body) = run_task(
        &app,
        "ch-ops-rd-ins",
        dw(
            "ops-rd",
            "t_i",
            json!({
                "op": "insert", "target": "users",
                "values": { "id": "u9", "name": "Nina", "status": "active" }
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert_eq!(body["status"], "ok", "write-only connector: {body}");
}
