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
// W6 + W7: the mutation envelope is strict, and nested under `write`
// ---------------------------------------------------------------------------

/// W6: a misspelled envelope key used to be silently ignored — `"retuning"`
/// meant no returning, a misspelled `filter` meant an unfiltered mutation.
/// It must be rejected naming the key; the legacy flat form (whose object
/// legitimately carries the handler keys alongside the envelope) still is.
#[tokio::test]
async fn unknown_write_envelope_keys_are_rejected() {
    let conn = "dw-w6";
    let app = sqlite_app(conn, "dw_w6").await;

    common::create_and_activate_channel(
        &app,
        "ch-dw-w6",
        common::workflow_with_tasks(
            "dw",
            json!([
                ddl(
                    conn,
                    "t_ddl",
                    "CREATE TABLE IF NOT EXISTS w6_t (id INTEGER, name TEXT)"
                ),
                dw(
                    conn,
                    "t_w",
                    json!({
                        "op": "insert", "target": "w6_t",
                        "values": { "id": 1, "name": "a" },
                        "retuning": ["id"]
                    })
                ),
            ]),
        ),
    )
    .await;

    let (status, body) = post(&app, "ch-dw-w6", json!({ "data": {} })).await;
    assert!(
        is_rejection(status, &body),
        "an unknown envelope key must be rejected, got status={status} body={body}"
    );
    let text = serde_json::to_string(&body).unwrap();
    assert!(
        text.contains("retuning"),
        "the rejection must name the offending key: {body}"
    );
}

/// W7: the pre-1.0 flat shape — envelope keys sharing the namespace with the
/// handler's own — is refused at create, naming `write`.
///
/// Refusing at create rather than at the task's first request is the whole
/// point: a stored `data_write` that never migrated would otherwise keep
/// loading and activating, and fail only once production traffic reached it.
#[tokio::test]
async fn the_pre_1_0_flat_envelope_is_refused_at_create() {
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
                        "connector": "dw-shapes",
                        "output": "data.w",
                        "schema": { "unmapped": "identity" },
                        // The envelope, flat — the 0.3.x spelling.
                        "op": "insert",
                        "target": "shapes_flat",
                        "values": { "id": 1, "name": "a" }
                    }}
                }]),
            )),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = common::body_json(resp).await;
    assert!(
        body.to_string().contains("write"),
        "the rejection must name the envelope key: {body}"
    );
}

/// A half-migrated task — envelope moved under `write`, stale flat keys left
/// behind — runs the `write` envelope. The stale keys are inert.
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
                        "schema": { "unmapped": "identity" },
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

// ---------------------------------------------------------------------------
// W3: `returning` is subject to the read allowlist
// ---------------------------------------------------------------------------

/// `returning` used to resolve through a helper that fell through to the raw
/// column name regardless of policy, so `"returning": ["secret"]` read back
/// any column the database user could see — including one the schema declared
/// `queryable: false`, and any column at all under `unmapped: "reject"`. The
/// doc comment justified skipping the `writable` check and silently skipped
/// the allowlist with it.
#[tokio::test]
async fn returning_cannot_read_a_non_queryable_column() {
    let conn = "dw-w3";
    let app = sqlite_app(conn, "dw_w3").await;

    let schema = json!({ "entities": { "items": {
        "physical": "w3_items",
        "columns": {
            "id":     { "queryable": true,  "writable": true },
            "name":   { "queryable": true,  "writable": true },
            // Writable but deliberately not readable back.
            "secret": { "queryable": false, "writable": true }
        }
    }}});

    common::create_and_activate_channel(
        &app,
        "ch-dw-w3",
        common::workflow_with_tasks(
            "dw",
            json!([
                ddl(
                    conn,
                    "t_ddl",
                    "CREATE TABLE IF NOT EXISTS w3_items (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, secret TEXT)"
                ),
                dw(conn, "t_w", json!({
                    "op": "insert", "target": "items",
                    "values": { "name": "Widget", "secret": "s3cr3t" },
                    "returning": ["id", "secret"],
                    "schema": schema
                })),
            ]),
        ),
    )
    .await;

    let (status, body) = post(&app, "ch-dw-w3", json!({ "data": {} })).await;
    let text = serde_json::to_string(&body).unwrap();
    assert!(
        is_rejection(status, &body) || !text.contains("s3cr3t"),
        "`returning` leaked a non-queryable column: {body}"
    );
}

/// The same gate under `unmapped: "reject"`: an undeclared column must not be
/// readable via `returning` when it would be rejected in `filter`.
#[tokio::test]
async fn returning_respects_unmapped_reject() {
    let conn = "dw-w3u";
    let app = sqlite_app(conn, "dw_w3u").await;

    let schema = json!({
        "unmapped": "reject",
        "entities": { "items": {
            "physical": "w3u_items",
            "columns": { "id": {}, "name": {} }
        }}
    });

    common::create_and_activate_channel(
        &app,
        "ch-dw-w3u",
        common::workflow_with_tasks(
            "dw",
            json!([
                ddl(
                    conn,
                    "t_ddl",
                    "CREATE TABLE IF NOT EXISTS w3u_items (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, secret TEXT)"
                ),
                dw(conn, "t_w", json!({
                    "op": "insert", "target": "items",
                    "values": { "name": "Widget" },
                    "returning": ["secret"],
                    "schema": schema
                })),
            ]),
        ),
    )
    .await;

    let (status, body) = post(&app, "ch-dw-w3u", json!({ "data": {} })).await;
    assert!(
        is_rejection(status, &body),
        "an undeclared column must be rejected under unmapped=reject: {body}"
    );
}
