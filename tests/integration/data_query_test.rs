//! End-to-end tests for the `data_query` handler (Phase 1: scalar SQL, identity
//! mode). Each test seeds an in-memory SQLite connector via `db_write`, then runs
//! a portable `data_query` through the data route and asserts the returned rows —
//! proving the sea-query → `AnyPool` execution path end-to-end.

use crate::common;

use axum::http::StatusCode;
use orion::config::{AppConfig, QueryConfig};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Build a `db_write` INSERT task for the shared `users` table.
fn insert_task(conn: &str, tid: &str, id: &str, name: &str, age: i64, status: &str) -> Value {
    json!({
        "id": tid,
        "name": "insert",
        "function": {
            "name": "db_write",
            "input": {
                "connector": conn,
                "query": "INSERT INTO users (id, name, age, status) VALUES (?, ?, ?, ?)",
                "params": [id, name, age, status],
                "output": "data.ins"
            }
        }
    })
}

/// Tasks that create the `users` table and insert four fixture rows.
fn seed_tasks(conn: &str) -> Vec<Value> {
    vec![
        json!({
            "id": "t_create",
            "name": "create",
            "function": {
                "name": "db_write",
                "input": {
                    "connector": conn,
                    "query": "CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY, name TEXT, age INTEGER, status TEXT)",
                    "output": "data.create"
                }
            }
        }),
        insert_task(conn, "t_i1", "u1", "Alice", 15, "active"),
        insert_task(conn, "t_i2", "u2", "Bob", 20, "active"),
        insert_task(conn, "t_i3", "u3", "Carol", 30, "inactive"),
        insert_task(conn, "t_i4", "u4", "Dave", 40, "active"),
    ]
}

/// A `data_query` task with the given query envelope, writing to `data.result`.
fn query_task(conn: &str, query: Value) -> Value {
    json!({
        "id": "t_query",
        "name": "query",
        "function": {
            "name": "data_query",
            "input": { "connector": conn, "query": query, "output": "data.result" }
        }
    })
}

/// Seed + run a single `data_query`, returning the parsed response body.
async fn run_query(app: &axum::Router, conn: &str, channel: &str, query: Value) -> Value {
    run_query_with_body(app, conn, channel, query, json!({ "data": {} })).await
}

async fn run_query_with_body(
    app: &axum::Router,
    conn: &str,
    channel: &str,
    query: Value,
    body: Value,
) -> Value {
    let mut tasks = seed_tasks(conn);
    tasks.push(query_task(conn, query));
    common::create_and_activate_channel(
        app,
        channel,
        common::workflow_with_tasks("dq", json!(tasks)),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            &format!("/api/v1/data/{channel}"),
            Some(body),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "data route should return 200"
    );
    common::body_json(resp).await
}

#[tokio::test]
async fn test_data_query_filter_projection_sort() {
    let app = common::test_app().await;
    common::create_connector(
        &app,
        common::db_connector_sqlite(
            "dq-filter",
            "sqlite:file:dq_filter?mode=memory&cache=shared",
        ),
    )
    .await;

    let body = run_query(
        &app,
        "dq-filter",
        "ch-dq-filter",
        json!({
            "source": "users",
            "filter": { ">": [{ "field": "age" }, 18] },
            "fields": ["id", "age"],
            "sort": [{ "age": "asc" }],
            "limit": 50
        }),
    )
    .await;

    assert_eq!(body["status"], "ok");
    let rows = body["data"]["result"]
        .as_array()
        .expect("result should be an array");
    // age > 18 → Bob(20), Carol(30), Dave(40), ordered ascending.
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["id"], "u2");
    assert_eq!(rows[0]["age"], 20);
    assert_eq!(rows[2]["age"], 40);
    // Projection: only id + age were requested.
    assert!(rows[0].get("name").is_none());
    assert!(rows[0].get("status").is_none());
}

#[tokio::test]
async fn test_data_query_membership() {
    let app = common::test_app().await;
    common::create_connector(
        &app,
        common::db_connector_sqlite("dq-in", "sqlite:file:dq_in?mode=memory&cache=shared"),
    )
    .await;

    // status IN ('inactive') → only Carol(u3).
    let body = run_query(
        &app,
        "dq-in",
        "ch-dq-in",
        json!({
            "source": "users",
            "filter": { "in": [{ "field": "status" }, ["inactive"]] }
        }),
    )
    .await;

    assert_eq!(body["status"], "ok");
    let rows = body["data"]["result"].as_array().expect("array");
    assert_eq!(rows.len(), 1, "body = {body}");
    assert_eq!(rows[0]["id"], "u3");
    assert_eq!(rows[0]["name"], "Carol");
}

#[tokio::test]
async fn test_data_query_param_resolved_from_message() {
    let app = common::test_app().await;
    let conn = "dq-ctx";
    common::create_connector(
        &app,
        common::db_connector_sqlite(conn, "sqlite:file:dq_ctx?mode=memory&cache=shared"),
    )
    .await;

    // Bring the request payload into `data.req` (Orion convention), then filter
    // age > {param:min} with min = {var: data.req.threshold}.
    let mut tasks = vec![json!({
        "id": "t_parse",
        "name": "parse payload",
        "function": { "name": "parse_json", "input": { "source": "payload", "target": "req" } }
    })];
    tasks.extend(seed_tasks(conn));
    tasks.push(json!({
        "id": "t_query",
        "name": "query",
        "function": {
            "name": "data_query",
            "input": {
                "connector": conn,
                "query": {
                    "source": "users",
                    "filter": { ">": [{ "field": "age" }, { "param": "min" }] },
                    "sort": [{ "age": "asc" }]
                },
                "params": { "min": { "var": "data.req.threshold" } },
                "output": "data.result"
            }
        }
    }));
    common::create_and_activate_channel(
        &app,
        "ch-dq-ctx",
        common::workflow_with_tasks("dq", json!(tasks)),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/ch-dq-ctx",
            Some(json!({ "data": { "threshold": 25 } })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");

    // age > 25 → Carol(30), Dave(40).
    let rows = body["data"]["result"].as_array().expect("array");
    assert_eq!(rows.len(), 2, "body = {body}");
    assert_eq!(rows[0]["id"], "u3");
    assert_eq!(rows[1]["id"], "u4");
}

#[tokio::test]
async fn test_data_query_inclusive_range() {
    let app = common::test_app().await;
    common::create_connector(
        &app,
        common::db_connector_sqlite("dq-range", "sqlite:file:dq_range?mode=memory&cache=shared"),
    )
    .await;

    // Chained `<=` → inclusive BETWEEN 18 AND 35 → Bob(20), Carol(30).
    let body = run_query(
        &app,
        "dq-range",
        "ch-dq-range",
        json!({
            "source": "users",
            "filter": { "<=": [18, { "field": "age" }, 35] },
            "sort": [{ "age": "asc" }]
        }),
    )
    .await;

    assert_eq!(body["status"], "ok");
    let rows = body["data"]["result"].as_array().expect("array");
    assert_eq!(rows.len(), 2, "body = {body}");
    assert_eq!(rows[0]["age"], 20);
    assert_eq!(rows[1]["age"], 30);
}

#[tokio::test]
async fn test_data_query_empty_result() {
    let app = common::test_app().await;
    common::create_connector(
        &app,
        common::db_connector_sqlite("dq-empty", "sqlite:file:dq_empty?mode=memory&cache=shared"),
    )
    .await;

    let body = run_query(
        &app,
        "dq-empty",
        "ch-dq-empty",
        json!({ "source": "users", "filter": { ">": [{ "field": "age" }, 1000] } }),
    )
    .await;

    assert_eq!(body["status"], "ok");
    let rows = body["data"]["result"].as_array().expect("array");
    assert!(rows.is_empty(), "expected empty array, got {rows:?}");
}

#[tokio::test]
async fn test_data_query_limit_exceeds_max_rejected() {
    // Configure a hard cap of 1; a query asking for 50 must be rejected.
    let config = AppConfig {
        query: QueryConfig {
            default_limit: 1,
            max_limit: 1,
        },
        ..Default::default()
    };
    let app = common::test_app_with_config(config).await;
    let conn = "dq-cap";
    common::create_connector(
        &app,
        common::db_connector_sqlite(conn, "sqlite:file:dq_cap?mode=memory&cache=shared"),
    )
    .await;

    let mut tasks = seed_tasks(conn);
    tasks.push(query_task(conn, json!({ "source": "users", "limit": 50 })));
    common::create_and_activate_channel(
        &app,
        "ch-dq-cap",
        common::workflow_with_tasks("dq", json!(tasks)),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/ch-dq-cap",
            Some(json!({ "data": {} })),
        ))
        .await
        .unwrap();

    let status = resp.status();
    let body = common::body_json(resp).await;
    let has_errors_array = body
        .get("errors")
        .and_then(|e| e.as_array())
        .is_some_and(|a| !a.is_empty());
    let has_error_object = body.get("error").is_some_and(|e| e.get("code").is_some());
    assert!(
        has_errors_array || has_error_object || status.is_server_error(),
        "expected a rejection for limit over the cap, got status={status} body={body}"
    );
}

#[tokio::test]
async fn test_data_query_relation_some() {
    let app = common::test_app().await;
    let conn = "dq-rel";
    common::create_connector(
        &app,
        common::db_connector_sqlite(conn, "sqlite:file:dq_rel?mode=memory&cache=shared"),
    )
    .await;

    // Two entities: users and orders (orders.user_id → users.id). Only u1 has an
    // order over 100, so `some orders total > 100` must return only u1.
    let dbw = |id: &str, q: &str| {
        json!({
            "id": id, "name": id,
            "function": { "name": "db_write", "input": { "connector": conn, "query": q, "output": "data.x" } }
        })
    };
    let tasks = json!([
        dbw("t_cu", "CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY, name TEXT)"),
        dbw("t_co", "CREATE TABLE IF NOT EXISTS orders (id TEXT PRIMARY KEY, user_id TEXT, total INTEGER)"),
        dbw("t_u1", "INSERT INTO users (id, name) VALUES ('u1', 'Alice')"),
        dbw("t_u2", "INSERT INTO users (id, name) VALUES ('u2', 'Bob')"),
        dbw("t_o1", "INSERT INTO orders (id, user_id, total) VALUES ('o1', 'u1', 150)"),
        dbw("t_o2", "INSERT INTO orders (id, user_id, total) VALUES ('o2', 'u2', 50)"),
        {
            "id": "t_q", "name": "query",
            "function": { "name": "data_query", "input": {
                "connector": conn,
                "query": {
                    "source": "users",
                    "fields": ["id", "name"],
                    "sort": [{ "id": "asc" }],
                    "filter": { "some": [{ "field": "orders" }, { ">": [{ "field": "total" }, 100] }] }
                },
                "schema": { "entities": { "users": { "relations": {
                    "orders": { "to": "orders", "kind": "has_many", "local": "id", "foreign": "user_id" }
                } } } },
                "output": "data.result"
            } }
        }
    ]);
    common::create_and_activate_channel(
        &app,
        "ch-dq-rel",
        common::workflow_with_tasks("dq", tasks),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/ch-dq-rel",
            Some(json!({ "data": {} })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");
    let rows = body["data"]["result"].as_array().expect("array");
    assert_eq!(rows.len(), 1, "body = {body}");
    assert_eq!(rows[0]["id"], "u1");
    assert_eq!(rows[0]["name"], "Alice");
}
