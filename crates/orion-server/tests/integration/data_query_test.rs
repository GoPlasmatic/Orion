//! End-to-end tests for the `data_query` handler (Phase 1: scalar SQL, identity
//! mode). Each test seeds an in-memory SQLite connector via `db_write`, then runs
//! a portable `data_query` through the data route and asserts the returned rows —
//! proving the sea-query → `AnyPool` execution path end-to-end.

use crate::common;
use crate::common::dsl::{ddl, dq, is_rejection, post};

use axum::http::StatusCode;
use orion::config::{AppConfig, QueryConfig};
use serde_json::{Value, json};

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

/// Seed + run a single `data_query`, returning the parsed response body.
async fn run_query(app: &axum::Router, conn: &str, channel: &str, query: Value) -> Value {
    let mut tasks = seed_tasks(conn);
    tasks.push(dq(conn, "t_query", query));
    common::create_and_activate_channel(
        app,
        channel,
        common::workflow_with_tasks("dq", json!(tasks)),
    )
    .await;

    let (status, body) = post(app, channel, json!({ "data": {} })).await;
    assert_eq!(status, StatusCode::OK, "data route should return 200");
    body
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
                "schema": { "unmapped": "identity" },
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

    let (status, body) = post(&app, "ch-dq-ctx", json!({ "data": { "threshold": 25 } })).await;
    assert_eq!(status, StatusCode::OK);
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
            ..QueryConfig::default()
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
    tasks.push(dq(
        conn,
        "t_query",
        json!({ "source": "users", "limit": 50 }),
    ));
    common::create_and_activate_channel(
        &app,
        "ch-dq-cap",
        common::workflow_with_tasks("dq", json!(tasks)),
    )
    .await;

    let (status, body) = post(&app, "ch-dq-cap", json!({ "data": {} })).await;
    assert!(
        is_rejection(status, &body),
        "expected a rejection for limit over the cap, got status={status} body={body}"
    );
}

/// W19: a nested list inside an `in` haystack has no portable form; it must be
/// rejected at lowering — identically for every backend — not mistranslated.
/// SQL used to error late with a fabricated `at: filter` location while Mongo
/// and ES silently nested it.
#[tokio::test]
async fn test_data_query_nested_list_rejected() {
    let app = common::test_app().await;
    let conn = "dq-nest";
    common::create_connector(
        &app,
        common::db_connector_sqlite(conn, "sqlite:file:dq_nest?mode=memory&cache=shared"),
    )
    .await;

    let mut tasks = seed_tasks(conn);
    tasks.push(dq(
        conn,
        "t_query",
        json!({
            "source": "users",
            "filter": { "in": [{ "field": "status" }, ["active", ["nested"]]] }
        }),
    ));
    common::create_and_activate_channel(
        &app,
        "ch-dq-nest",
        common::workflow_with_tasks("dq", json!(tasks)),
    )
    .await;

    let (status, body) = post(&app, "ch-dq-nest", json!({ "data": {} })).await;
    assert!(
        is_rejection(status, &body),
        "expected a rejection for a nested list literal, got status={status} body={body}"
    );
}

/// W12: `skip` is capped like `limit` — rejected over `query.max_skip`,
/// never clamped. The cap used to exist only on Elasticsearch, so the same
/// envelope scanned arbitrarily deep on SQL.
#[tokio::test]
async fn test_data_query_skip_exceeds_max_rejected() {
    let config = AppConfig {
        query: QueryConfig {
            max_skip: 10,
            ..QueryConfig::default()
        },
        ..Default::default()
    };
    let app = common::test_app_with_config(config).await;
    let conn = "dq-skip";
    common::create_connector(
        &app,
        common::db_connector_sqlite(conn, "sqlite:file:dq_skip?mode=memory&cache=shared"),
    )
    .await;

    let mut tasks = seed_tasks(conn);
    tasks.push(dq(
        conn,
        "t_query",
        json!({ "source": "users", "skip": 50 }),
    ));
    common::create_and_activate_channel(
        &app,
        "ch-dq-skip",
        common::workflow_with_tasks("dq", json!(tasks)),
    )
    .await;

    let (status, body) = post(&app, "ch-dq-skip", json!({ "data": {} })).await;
    assert!(
        is_rejection(status, &body),
        "expected a rejection for skip over the cap, got status={status} body={body}"
    );
}

/// W6: a misspelled envelope key used to be silently ignored — `"fileds"`
/// selected every column. It must be rejected naming the key.
#[tokio::test]
async fn test_data_query_unknown_envelope_key_rejected() {
    let app = common::test_app().await;
    let conn = "dq-unk";
    common::create_connector(
        &app,
        common::db_connector_sqlite(conn, "sqlite:file:dq_unk?mode=memory&cache=shared"),
    )
    .await;

    let mut tasks = seed_tasks(conn);
    tasks.push(dq(
        conn,
        "t_query",
        json!({ "source": "users", "fileds": ["id"] }),
    ));
    common::create_and_activate_channel(
        &app,
        "ch-dq-unk",
        common::workflow_with_tasks("dq", json!(tasks)),
    )
    .await;

    let (status, body) = post(&app, "ch-dq-unk", json!({ "data": {} })).await;
    assert!(
        is_rejection(status, &body),
        "expected a rejection for the unknown key, got status={status} body={body}"
    );
    let text = serde_json::to_string(&body).unwrap();
    assert!(
        text.contains("fileds"),
        "the rejection must name the offending key: {body}"
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
    let tasks = json!([
        ddl(conn, "t_cu", "CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY, name TEXT)"),
        ddl(conn, "t_co", "CREATE TABLE IF NOT EXISTS orders (id TEXT PRIMARY KEY, user_id TEXT, total INTEGER)"),
        ddl(conn, "t_u1", "INSERT INTO users (id, name) VALUES ('u1', 'Alice')"),
        ddl(conn, "t_u2", "INSERT INTO users (id, name) VALUES ('u2', 'Bob')"),
        ddl(conn, "t_o1", "INSERT INTO orders (id, user_id, total) VALUES ('o1', 'u1', 150)"),
        ddl(conn, "t_o2", "INSERT INTO orders (id, user_id, total) VALUES ('o2', 'u2', 50)"),
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
                "schema": { "unmapped": "identity", "entities": { "users": { "relations": {
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

    let (status, body) = post(&app, "ch-dq-rel", json!({ "data": {} })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    let rows = body["data"]["result"].as_array().expect("array");
    assert_eq!(rows.len(), 1, "body = {body}");
    assert_eq!(rows[0]["id"], "u1");
    assert_eq!(rows[0]["name"], "Alice");
}

#[tokio::test]
async fn test_data_query_include_nested() {
    let app = common::test_app().await;
    let conn = "dq-inc";
    common::create_connector(
        &app,
        common::db_connector_sqlite(conn, "sqlite:file:dq_inc?mode=memory&cache=shared"),
    )
    .await;

    let tasks = json!([
        ddl(conn, "t_cu", "CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY, name TEXT)"),
        ddl(conn, "t_co", "CREATE TABLE IF NOT EXISTS orders (id TEXT PRIMARY KEY, user_id TEXT, total INTEGER)"),
        ddl(conn, "t_u1", "INSERT INTO users (id, name) VALUES ('u1', 'Alice')"),
        ddl(conn, "t_u2", "INSERT INTO users (id, name) VALUES ('u2', 'Bob')"),
        ddl(conn, "t_o1", "INSERT INTO orders (id, user_id, total) VALUES ('o1', 'u1', 150)"),
        ddl(conn, "t_o2", "INSERT INTO orders (id, user_id, total) VALUES ('o2', 'u1', 50)"),
        ddl(conn, "t_o3", "INSERT INTO orders (id, user_id, total) VALUES ('o3', 'u2', 10)"),
        {
            "id": "t_q", "name": "query",
            "function": { "name": "data_query", "input": {
                "connector": conn,
                "query": {
                    "source": "users",
                    "fields": ["id", "name"],
                    "sort": [{ "id": "asc" }],
                    "include": { "orders": {
                        "fields": ["id", "total"], "sort": [{ "id": "asc" }], "limit": 5
                    } }
                },
                "schema": { "unmapped": "identity", "entities": { "users": { "relations": {
                    "orders": { "to": "orders", "kind": "has_many", "local": "id", "foreign": "user_id" }
                } } } },
                "output": "data.result"
            } }
        }
    ]);
    common::create_and_activate_channel(
        &app,
        "ch-dq-inc",
        common::workflow_with_tasks("dq", tasks),
    )
    .await;

    let (status, body) = post(&app, "ch-dq-inc", json!({ "data": {} })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok", "body = {body}");
    let rows = body["data"]["result"].as_array().expect("array");
    assert_eq!(rows.len(), 2);

    // u1 (Alice) has two nested orders; the foreign key user_id is stripped.
    assert_eq!(rows[0]["id"], "u1");
    let alice_orders = rows[0]["orders"].as_array().expect("orders array");
    assert_eq!(alice_orders.len(), 2);
    assert!(alice_orders[0].get("user_id").is_none());
    assert!(alice_orders[0].get("total").is_some());

    // u2 (Bob) has one nested order.
    assert_eq!(rows[1]["id"], "u2");
    assert_eq!(rows[1]["orders"].as_array().expect("orders array").len(), 1);
}

/// W14: the parent key and the child foreign key come out of two different
/// columns, and a driver renders a column's value from its SQL *type*. Here
/// `users.id` is `INTEGER` and `orders.user_id` is `TEXT`, so the same join key
/// arrives as `1` on the parent side and `"1"` on the child side. Grouping on
/// the value's `serde_json` text therefore matched nothing: every child array
/// came back empty, with no error and no warning.
///
/// SQLite's comparison affinity still makes the child query itself match — a
/// `TEXT` column compared against a bound integer has TEXT affinity applied to
/// the parameter, so `WHERE user_id IN (1, 2)` finds the rows stored as `'1'`
/// and `'2'`. The rows are fetched; it was the in-memory join that dropped them.
/// Grouping now goes through `GroupKey`, which normalises integral values
/// however the driver rendered them.
#[tokio::test]
async fn test_data_query_include_joins_keys_the_driver_rendered_differently() {
    let app = common::test_app().await;
    let conn = "dq-inc-mixed";
    common::create_connector(
        &app,
        common::db_connector_sqlite(conn, "sqlite:file:dq_inc_mixed?mode=memory&cache=shared"),
    )
    .await;

    let tasks = json!([
        // The whole point: INTEGER on the parent, TEXT on the child.
        ddl(conn, "t_cu", "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT)"),
        ddl(conn, "t_co", "CREATE TABLE IF NOT EXISTS orders (id TEXT PRIMARY KEY, user_id TEXT, total INTEGER)"),
        ddl(conn, "t_u1", "INSERT INTO users (id, name) VALUES (1, 'Alice')"),
        ddl(conn, "t_u2", "INSERT INTO users (id, name) VALUES (2, 'Bob')"),
        ddl(conn, "t_o1", "INSERT INTO orders (id, user_id, total) VALUES ('o1', '1', 150)"),
        ddl(conn, "t_o2", "INSERT INTO orders (id, user_id, total) VALUES ('o2', '1', 50)"),
        ddl(conn, "t_o3", "INSERT INTO orders (id, user_id, total) VALUES ('o3', '2', 10)"),
        {
            "id": "t_q", "name": "query",
            "function": { "name": "data_query", "input": {
                "connector": conn,
                "query": {
                    "source": "users",
                    "fields": ["id", "name"],
                    "sort": [{ "id": "asc" }],
                    "include": { "orders": {
                        "fields": ["total"], "sort": [{ "id": "asc" }], "limit": 5
                    } }
                },
                "schema": { "unmapped": "identity", "entities": { "users": { "relations": {
                    "orders": { "to": "orders", "kind": "has_many", "local": "id", "foreign": "user_id" }
                } } } },
                "output": "data.result"
            } }
        }
    ]);
    common::create_and_activate_channel(
        &app,
        "ch-dq-inc-mixed",
        common::workflow_with_tasks("dq", tasks),
    )
    .await;

    let (status, body) = post(&app, "ch-dq-inc-mixed", json!({ "data": {} })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok", "body = {body}");
    let rows = body["data"]["result"].as_array().expect("array");
    assert_eq!(rows.len(), 2, "body = {body}");

    // The renderings really do differ — if they ever stop differing this test
    // has stopped testing anything.
    assert_eq!(
        rows[0]["id"],
        json!(1),
        "parent key must be a number: {body}"
    );

    let alice: Vec<i64> = rows[0]["orders"]
        .as_array()
        .expect("orders array")
        .iter()
        .map(|o| o["total"].as_i64().expect("total"))
        .collect();
    assert_eq!(alice, vec![150, 50], "body = {body}");
    let bob: Vec<i64> = rows[1]["orders"]
        .as_array()
        .expect("orders array")
        .iter()
        .map(|o| o["total"].as_i64().expect("total"))
        .collect();
    assert_eq!(bob, vec![10], "body = {body}");
}

// ---------------------------------------------------------------------------
// W2: projection and sort go through schema resolution
// ---------------------------------------------------------------------------

async fn sqlite_app(conn: &str, mem: &str) -> axum::Router {
    let app = common::test_app().await;
    common::create_connector(
        &app,
        common::db_connector_sqlite(conn, &format!("sqlite:file:{mem}?mode=memory&cache=shared")),
    )
    .await;
    app
}

/// `resolve_field` used to have exactly one call site — the filter lowerer —
/// so `fields` and `sort` reached the backends as raw logical strings. With
/// `{"secret": {"queryable": false}}`, `fields: ["secret"]` still emitted
/// `SELECT "secret"`: the allowlist documented for the dialect protected the
/// filter and nothing else.
#[tokio::test]
async fn projection_cannot_read_a_non_queryable_column() {
    let conn = "dq-w2";
    let app = sqlite_app(conn, "dq_w2").await;

    let schema = json!({ "entities": { "items": {
        "physical": "w2_items",
        "columns": {
            "id":     { "queryable": true },
            "name":   { "queryable": true },
            "secret": { "queryable": false }
        }
    }}});

    common::create_and_activate_channel(
        &app,
        "ch-dq-w2",
        common::workflow_with_tasks(
            "dq",
            json!([
                ddl(
                    conn,
                    "t_ddl",
                    "CREATE TABLE IF NOT EXISTS w2_items (id INTEGER, name TEXT, secret TEXT)"
                ),
                ddl(
                    conn,
                    "t_seed",
                    "INSERT INTO w2_items VALUES (1, 'Widget', 's3cr3t')"
                ),
                json!({
                    "id": "q", "name": "q",
                    "function": { "name": "data_query", "input": {
                        "connector": conn,
                        "schema": schema,
                        "query": { "source": "items", "fields": ["id", "secret"] },
                        "output": "data.result"
                    }}
                }),
            ]),
        ),
    )
    .await;

    let (status, body) = post(&app, "ch-dq-w2", json!({ "data": {} })).await;
    let text = serde_json::to_string(&body).unwrap();
    assert!(
        is_rejection(status, &body) || !text.contains("s3cr3t"),
        "`fields` leaked a non-queryable column: {body}"
    );
}

/// The same gate on `sort`: ordering by a column reveals information about it
/// and must not sidestep the allowlist either.
#[tokio::test]
async fn sort_cannot_name_a_non_queryable_column() {
    let conn = "dq-w2s";
    let app = sqlite_app(conn, "dq_w2s").await;

    let schema = json!({ "entities": { "items": {
        "physical": "w2s_items",
        "columns": { "id": { "queryable": true }, "secret": { "queryable": false } }
    }}});

    common::create_and_activate_channel(
        &app,
        "ch-dq-w2s",
        common::workflow_with_tasks(
            "dq",
            json!([
                ddl(
                    conn,
                    "t_ddl",
                    "CREATE TABLE IF NOT EXISTS w2s_items (id INTEGER, secret TEXT)"
                ),
                json!({
                    "id": "q", "name": "q",
                    "function": { "name": "data_query", "input": {
                        "connector": conn,
                        "schema": schema,
                        "query": { "source": "items", "sort": [{ "secret": "asc" }] },
                        "output": "data.result"
                    }}
                }),
            ]),
        ),
    )
    .await;

    let (status, body) = post(&app, "ch-dq-w2s", json!({ "data": {} })).await;
    assert!(
        is_rejection(status, &body),
        "`sort` must respect the allowlist: {body}"
    );
}

/// A column rename used to apply to the filter and silently not to the
/// projection, so `fields: ["email"]` selected a column that does not exist.
#[tokio::test]
async fn projection_and_sort_honour_a_column_rename() {
    let conn = "dq-w2r";
    let app = sqlite_app(conn, "dq_w2r").await;

    let schema = json!({ "entities": { "users": {
        "physical": "w2r_users",
        "columns": {
            "id":    { "name": "user_pk" },
            "email": { "name": "email_addr" }
        }
    }}});

    common::create_and_activate_channel(
        &app,
        "ch-dq-w2r",
        common::workflow_with_tasks(
            "dq",
            json!([
                ddl(
                    conn,
                    "t_ddl",
                    "CREATE TABLE IF NOT EXISTS w2r_users (user_pk INTEGER, email_addr TEXT)"
                ),
                ddl(
                    conn,
                    "t_seed",
                    "INSERT INTO w2r_users VALUES (1, 'ada@x.io'), (2, 'grace@x.io')"
                ),
                json!({
                    "id": "q", "name": "q",
                    "function": { "name": "data_query", "input": {
                        "connector": conn,
                        "schema": schema,
                        "query": {
                            "source": "users",
                            "fields": ["email"],
                            "sort": [{ "email": "asc" }]
                        },
                        "output": "data.result"
                    }}
                }),
            ]),
        ),
    )
    .await;

    let (status, body) = post(&app, "ch-dq-w2r", json!({ "data": {} })).await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert_eq!(
        body["data"]["result"],
        json!([{ "email_addr": "ada@x.io" }, { "email_addr": "grace@x.io" }]),
        "the rename must reach projection and sort, not just the filter: {body}"
    );
}
