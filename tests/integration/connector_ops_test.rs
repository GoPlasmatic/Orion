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
use crate::common::dsl::{ddl, dq, dw, is_gate_rejection, post};

use axum::http::StatusCode;
use serde_json::{Value, json};

/// A sqlite db connector with explicit operation gates over a shared in-memory DB.
fn gated_connector(name: &str, mem: &str, ops: Value) -> Value {
    json!({
        "id": name, "name": name, "connector_type": "db",
        "config": {
            "type": "db",
            "connection_string": format!("sqlite:file:{mem}?mode=memory&cache=shared"),
            "operations": ops
        }
    })
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

#[tokio::test]
async fn gate_rejection_does_not_name_the_connector_to_the_caller() {
    // The data plane is anonymous. A gate message like "operation 'read' is
    // disabled on connector 'prod-billing-db'" hands out connector inventory
    // for free, so it is logged and kept on the trace but redacted from the
    // response (proposal G3).
    let app = app_with_gated(
        "ops_leak",
        "ops-leak-admin",
        "ops-leak-secret",
        json!({ "read": false }),
    )
    .await;

    let (status, body) = run_task(
        &app,
        "ch-ops-leak",
        dq("ops-leak-secret", "t_q", json!({ "source": "users" })),
    )
    .await;

    assert!(
        is_gate_rejection(status, &body),
        "the gate must still reject: status={status} body={body}"
    );
    let rendered = body.to_string();
    assert!(
        !rendered.contains("ops-leak-secret"),
        "the connector name must not reach an anonymous caller: {rendered}"
    );
    assert!(
        !rendered.contains("disabled on connector"),
        "the gate detail must not reach an anonymous caller: {rendered}"
    );
}

// ============================================================
// F22e: the same gate mechanism on the other three connector types.
//
// `OperationGates` covered db and es only, so a cache connector could not be
// made read-only and an http connector had no method allow-list — the gate an
// operator reaches for first, since `http_call` is the one handler that can
// mutate an upstream nobody else in the deployment controls. Each gate is
// exercised both open and closed: a closed gate that rejects proves nothing
// if the open one rejects too.
// ============================================================

/// A cache connector (memory backend) with explicit operation gates.
fn gated_cache(name: &str, ops: Value) -> Value {
    json!({
        "id": name, "name": name, "connector_type": "cache",
        "config": { "type": "cache", "backend": "memory", "operations": ops }
    })
}

fn cache_write_task(conn: &str) -> Value {
    json!({
        "id": "t_cw", "name": "t_cw",
        "function": { "name": "cache_write", "input": {
            "connector": conn, "key": "k", "value": "v"
        } }
    })
}

fn cache_read_task(conn: &str) -> Value {
    json!({
        "id": "t_cr", "name": "t_cr",
        "function": { "name": "cache_read", "input": {
            "connector": conn, "key": "k", "output": "data.cached"
        } }
    })
}

#[tokio::test]
async fn test_cache_connector_can_be_made_read_only() {
    let app = common::test_app().await;
    common::create_connector(&app, gated_cache("cache-ro", json!({ "write": false }))).await;

    let (status, body) = run_task(&app, "ch-cache-ro-w", cache_write_task("cache-ro")).await;
    assert!(
        is_gate_rejection(status, &body),
        "disabled write must gate cache_write: status={status} body={body}"
    );

    // The read side of the same connector is untouched.
    let (status, body) = run_task(&app, "ch-cache-ro-r", cache_read_task("cache-ro")).await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert_eq!(body["status"], "ok", "read-only connector: {body}");
    assert!(body["data"]["cached"].is_null(), "body = {body}");
}

#[tokio::test]
async fn test_cache_connector_can_be_made_write_only() {
    let app = common::test_app().await;
    common::create_connector(&app, gated_cache("cache-wo", json!({ "read": false }))).await;

    let (status, body) = run_task(&app, "ch-cache-wo-r", cache_read_task("cache-wo")).await;
    assert!(
        is_gate_rejection(status, &body),
        "disabled read must gate cache_read: status={status} body={body}"
    );

    let (status, body) = run_task(&app, "ch-cache-wo-w", cache_write_task("cache-wo")).await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert_eq!(body["status"], "ok", "write-only connector: {body}");
}

/// Un-gated cache connectors keep working — the default is all-allowed, so
/// nothing authored before the gates existed changes behaviour.
#[tokio::test]
async fn test_ungated_cache_connector_is_fully_open() {
    let app = common::test_app().await;
    common::create_connector(&app, gated_cache("cache-open", json!({}))).await;

    for (channel, task) in [
        ("ch-cache-open-w", cache_write_task("cache-open")),
        ("ch-cache-open-r", cache_read_task("cache-open")),
    ] {
        let (status, body) = run_task(&app, channel, task).await;
        assert_eq!(status, StatusCode::OK, "body = {body}");
        assert_eq!(body["status"], "ok", "{channel}: {body}");
    }
}

fn gated_kafka(name: &str, ops: Value) -> Value {
    json!({
        "id": name, "name": name, "connector_type": "kafka",
        "config": {
            "type": "kafka", "brokers": ["localhost:9092"], "topic": "t",
            "operations": ops
        }
    })
}

fn publish_task(conn: &str) -> Value {
    json!({
        "id": "t_pk", "name": "t_pk",
        "function": { "name": "publish_kafka", "input": { "connector": conn, "topic": "t" } }
    })
}

/// The gate is checked before producer availability, so the refusal is the
/// same whether or not the deployment has Kafka enabled — and the open case
/// is distinguishable, because it gets past the gate and fails on the
/// disabled producer instead.
#[tokio::test]
async fn test_kafka_connector_can_be_made_publish_proof() {
    let app = common::test_app().await;
    common::create_connector(&app, gated_kafka("kafka-ro", json!({ "publish": false }))).await;
    common::create_connector(&app, gated_kafka("kafka-open", json!({}))).await;

    let (status, body) = run_task(&app, "ch-kafka-ro", publish_task("kafka-ro")).await;
    assert!(
        is_gate_rejection(status, &body),
        "disabled publish must gate publish_kafka: status={status} body={body}"
    );

    let (status, body) = run_task(&app, "ch-kafka-open", publish_task("kafka-open")).await;
    assert!(
        !is_gate_rejection(status, &body),
        "an open gate must let the call through to the producer, which is \
         disabled in this test app: status={status} body={body}"
    );
}

/// Mock upstream answering both methods, so the only thing that can refuse a
/// POST is the connector's allow-list.
async fn start_echo_server() -> std::net::SocketAddr {
    let mock = axum::Router::new().route(
        "/ping",
        axum::routing::get(|| async { axum::Json(json!({"ok": true})) })
            .post(|| async { axum::Json(json!({"ok": true})) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, mock).await.unwrap();
    });
    addr
}

fn http_task(conn: &str, method: &str) -> Value {
    json!({
        "id": "t_http", "name": "t_http",
        "function": { "name": "http_call", "input": {
            "connector": conn, "method": method, "path": "/ping",
            "output": "data.result", "timeout_ms": 5000
        } }
    })
}

#[tokio::test]
async fn test_http_connector_method_allow_list() {
    let addr = start_echo_server().await;
    let app = common::test_app().await;
    common::create_connector(
        &app,
        json!({
            "id": "http-ro", "name": "http-ro", "connector_type": "http",
            "config": {
                "type": "http",
                "url": format!("http://{addr}"),
                "retry": {"max_retries": 0, "retry_delay_ms": 10},
                "allow_private_urls": true,
                "operations": { "methods": ["GET"] }
            }
        }),
    )
    .await;

    // The allowed method reaches the upstream.
    let (status, body) = run_task(&app, "ch-http-get", http_task("http-ro", "GET")).await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert_eq!(body["status"], "ok", "GET must be allowed: {body}");
    assert_eq!(body["data"]["result"]["ok"], true, "body = {body}");

    // Everything else is refused at the connector, not at the upstream — the
    // upstream answers POST /ping perfectly happily.
    let (status, body) = run_task(&app, "ch-http-post", http_task("http-ro", "POST")).await;
    assert!(
        is_gate_rejection(status, &body),
        "POST must be refused by the allow-list: status={status} body={body}"
    );
}

/// An http connector with no allow-list keeps issuing every method, which is
/// what every connector authored before F22e means.
#[tokio::test]
async fn test_http_connector_without_an_allow_list_allows_every_method() {
    let addr = start_echo_server().await;
    let app = common::test_app().await;
    common::create_http_connector(&app, "http-open", addr).await;

    for (channel, method) in [("ch-http-open-g", "GET"), ("ch-http-open-p", "POST")] {
        let (status, body) = run_task(&app, channel, http_task("http-open", method)).await;
        assert_eq!(status, StatusCode::OK, "body = {body}");
        assert_eq!(body["status"], "ok", "{method} must be allowed: {body}");
    }
}

/// A method the allow-list names must be one `http_call` can issue: a typo
/// like `GTE` would otherwise persist and refuse every call at request time.
#[tokio::test]
async fn test_http_method_allow_list_rejects_an_unknown_method_at_the_door() {
    use tower::ServiceExt;

    let app = common::test_app().await;
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "http-typo", "name": "http-typo", "connector_type": "http",
                "config": {
                    "type": "http", "url": "https://example.com",
                    "operations": { "methods": ["GTE"] }
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = common::body_json(resp).await;
    assert!(
        body.to_string().contains("GTE"),
        "the error must name the offending method: {body}"
    );
}

/// F22e: a gate is only a control if a misspelled key is refused.
///
/// The gate structs are `#[serde(default)]` and connector configs deliberately
/// do not `deny_unknown_fields` (a 0.3.x row carrying a removed field must
/// still load), so `{"operations": {"writes": false}}` deserializes into a
/// *fully open* gate — accepted with a 201, stored, and gating nothing. The
/// keys are checked at the same door as the HTTP method values.
#[tokio::test]
async fn test_unknown_operation_gate_key_is_refused_for_every_type() {
    use tower::ServiceExt;

    let cases = [
        (
            "cache",
            json!({ "type": "cache", "backend": "memory", "operations": { "writes": false } }),
            "writes",
        ),
        (
            "kafka",
            json!({
                "type": "kafka", "brokers": ["b:9092"], "topic": "t",
                "operations": { "publlish": false }
            }),
            "publlish",
        ),
        (
            "http",
            json!({
                "type": "http", "url": "https://example.com",
                "operations": { "method": ["GET"] }
            }),
            "method",
        ),
        (
            "db",
            json!({
                "type": "db", "connection_string": "sqlite::memory:",
                "operations": { "raw_writes": false }
            }),
            "raw_writes",
        ),
        (
            "es",
            json!({ "type": "es", "url": "http://localhost:9200", "operations": { "publish": false } }),
            "publish",
        ),
    ];

    let app = common::test_app().await;
    for (connector_type, config, typo) in cases {
        let resp = app
            .clone()
            .oneshot(common::json_request(
                "POST",
                "/api/v1/admin/connectors",
                Some(json!({
                    "id": format!("gate-typo-{connector_type}"),
                    "name": format!("gate-typo-{connector_type}"),
                    "connector_type": connector_type,
                    "config": config
                })),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "a {connector_type} connector with a misspelled gate must not be created"
        );
        let body = common::body_json(resp).await;
        assert!(
            body.to_string().contains(typo),
            "the error must name the offending key: {body}"
        );
    }
}

/// …and the real keys still pass, for every type, so the check is a spelling
/// gate and not a blanket refusal of `operations`.
#[tokio::test]
async fn test_every_real_operation_gate_key_is_accepted() {
    let app = common::test_app().await;
    for (connector_type, config) in [
        (
            "cache",
            json!({
                "type": "cache", "backend": "memory",
                "operations": { "read": true, "write": false }
            }),
        ),
        (
            "kafka",
            json!({
                "type": "kafka", "brokers": ["b:9092"], "topic": "t",
                "operations": { "publish": false }
            }),
        ),
        (
            "http",
            json!({
                "type": "http", "url": "https://example.com",
                "operations": { "methods": ["GET"] }
            }),
        ),
        (
            "db",
            json!({
                "type": "db", "connection_string": "sqlite::memory:",
                "operations": {
                    "read": true, "insert": false, "update": false,
                    "delete": false, "upsert": false, "raw_write": false
                }
            }),
        ),
        (
            "es",
            json!({
                "type": "es", "url": "http://localhost:9200",
                "operations": { "read": true, "insert": false }
            }),
        ),
    ] {
        common::create_connector(
            &app,
            json!({
                "id": format!("gate-ok-{connector_type}"),
                "name": format!("gate-ok-{connector_type}"),
                "connector_type": connector_type,
                "config": config
            }),
        )
        .await;
    }
}
