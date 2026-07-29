//! The dialect's schema requirement and write-outcome contract (F24, F28).
//!
//! F24: through 0.x the dialect defaulted to identity mode, so a `data_query`
//! or `data_write` that declared no `schema` reached every table the
//! connector's database user could see — read *and* write. The safe mode
//! existed but was opt-in per task, so one forgotten `schema` key reopened
//! everything. These tests pin the flipped default, the error that tells an
//! author what to add, and the two connector-level guards an operator can put
//! on top of it.
//!
//! F28: a `data_write` result carries a `status`, and SQL's is never `partial`
//! because the statement runs in a transaction.

use crate::common;
use crate::common::dsl::{ddl, dq, dq_no_schema, dq_schema, dw, dw_no_schema, is_rejection, post};

use axum::http::StatusCode;
use serde_json::{Value, json};

/// A sqlite db connector over a shared in-memory DB, with optional `dialect`
/// guards.
fn connector(name: &str, mem: &str, dialect: Value) -> Value {
    json!({
        "id": name, "name": name, "connector_type": "db",
        "config": {
            "type": "db",
            "connection_string": format!("sqlite:file:{mem}?mode=memory&cache=shared"),
            "dialect": dialect
        }
    })
}

/// App with one unguarded "admin" connector (used for DDL and seeding) and one
/// connector carrying `dialect` guards, both over the same in-memory DB.
async fn app_with(mem: &str, admin: &str, guarded: &str, dialect: Value) -> axum::Router {
    let app = common::test_app().await;
    common::create_connector(&app, connector(admin, mem, json!({}))).await;
    common::create_connector(&app, connector(guarded, mem, dialect)).await;
    seed(&app, admin, mem).await;
    app
}

async fn seed(app: &axum::Router, admin: &str, mem: &str) {
    let tasks = vec![
        ddl(
            admin,
            "t_ddl",
            "CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY, name TEXT)",
        ),
        ddl(
            admin,
            "t_ddl2",
            "CREATE TABLE IF NOT EXISTS secrets (id TEXT PRIMARY KEY, token TEXT)",
        ),
        dw(
            admin,
            "t_seed",
            json!({ "op": "insert", "target": "users", "values": { "id": "u1", "name": "Alice" } }),
        ),
        dw(
            admin,
            "t_seed2",
            json!({ "op": "insert", "target": "secrets", "values": { "id": "s1", "token": "hunter2" } }),
        ),
    ];
    let channel = format!("ch-{mem}-setup");
    common::create_and_activate_channel(
        app,
        &channel,
        common::workflow_with_tasks("setup", json!(tasks)),
    )
    .await;
    let (status, body) = post(app, &channel, json!({ "data": {} })).await;
    assert_eq!(status, StatusCode::OK, "setup body = {body}");
    assert_eq!(body["status"], "ok", "setup body = {body}");
}

async fn run_task(app: &axum::Router, channel: &str, task: Value) -> (StatusCode, Value) {
    common::create_and_activate_channel(
        app,
        channel,
        common::workflow_with_tasks("d", json!([task])),
    )
    .await;
    post(app, channel, json!({ "data": {} })).await
}

// ---------------------------------------------------------------------
// F24: no schema means no access
// ---------------------------------------------------------------------

/// The break, on the read path. This exact task — no `schema` key — read the
/// whole table through 0.x.
#[tokio::test]
async fn a_query_without_a_schema_is_refused() {
    let app = app_with("f24_r", "f24-r-admin", "f24-r", json!({})).await;

    let (status, body) = run_task(
        &app,
        "ch-f24-r",
        dq_no_schema("f24-r", "t_q", json!({ "source": "secrets" })),
    )
    .await;

    assert!(
        is_rejection(status, &body),
        "a schema-less query must be refused: status={status} body={body}"
    );
}

/// The refusal has to be the *first* thing that fails, whatever else the query
/// mentions. Every read planner used to lower the filter and resolve
/// `fields`/`sort` before it resolved the table, so a real 0.x query — which
/// always carries a filter — died on `invalid field reference 'name'` and
/// never reached the one error that names the `schema` key to add.
#[tokio::test]
async fn a_schema_less_query_with_a_filter_is_refused_by_naming_the_schema() {
    let app = app_with("f24_rf", "f24-rf-admin", "f24-rf", json!({})).await;

    let (status, body) = run_task(
        &app,
        "ch-f24-rf",
        dq_no_schema(
            "f24-rf",
            "t_q",
            json!({
                "source": "secrets",
                "filter": { "==": [{ "field": "token" }, "hunter2"] },
                "fields": ["id", "token"],
                "sort": [{ "id": "asc" }]
            }),
        ),
    )
    .await;

    assert!(
        is_rejection(status, &body),
        "a schema-less query must be refused: status={status} body={body}"
    );
    let text = serde_json::to_string(&body).expect("body");
    assert!(
        text.contains("schema"),
        "the refusal must name the key to add, not the first field it \
         happened to resolve: {body}"
    );
    assert!(
        text.contains("unmapped") && text.contains("identity"),
        "the refusal must name the pass-through opt-out too: {body}"
    );
    assert!(
        !text.contains("hunter2"),
        "nothing may have been read: {body}"
    );
}

/// The same on the write path — the half that made this the most user-visible
/// break in the wave.
#[tokio::test]
async fn a_write_without_a_schema_is_refused() {
    let app = app_with("f24_w", "f24-w-admin", "f24-w", json!({})).await;

    let (status, body) = run_task(
        &app,
        "ch-f24-w",
        dw_no_schema(
            "f24-w",
            "t_w",
            json!({ "op": "insert", "target": "secrets", "values": { "id": "s9", "token": "x" } }),
        ),
    )
    .await;

    assert!(
        is_rejection(status, &body),
        "a schema-less write must be refused: status={status} body={body}"
    );
}

/// The hole the policy flip alone would have left: `reject` used to bound only
/// columns, and `{"source": "..."}` selects every column without resolving a
/// single field — so no field check ever ran and the table resolved anyway.
#[tokio::test]
async fn a_declared_schema_does_not_reach_an_undeclared_table() {
    let app = app_with("f24_e", "f24-e-admin", "f24-e", json!({})).await;
    let schema = json!({ "entities": { "users": { "columns": { "id": {}, "name": {} } } } });

    // The declared entity works.
    let (status, body) = run_task(
        &app,
        "ch-f24-e-ok",
        dq_schema("f24-e", "t_q", json!({ "source": "users" }), schema.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert_eq!(body["data"]["result"][0]["id"], "u1", "body = {body}");

    // A table the same schema does not declare does not, even with no fields named.
    let (status, body) = run_task(
        &app,
        "ch-f24-e-no",
        dq_schema("f24-e", "t_q", json!({ "source": "secrets" }), schema),
    )
    .await;
    assert!(
        is_rejection(status, &body),
        "an undeclared entity must be refused even when no field is named: \
         status={status} body={body}"
    );
}

/// The column half of the same hole. `queryable: false` meant "you may not
/// *name* this column": a query that named no fields at all rendered
/// `SELECT *` and returned it anyway, because nothing resolved a field. A
/// field-less read now projects the declared queryable columns.
#[tokio::test]
async fn a_field_less_query_does_not_return_a_non_queryable_column() {
    let app = app_with("f24_c", "f24-c-admin", "f24-c", json!({})).await;

    let (status, body) = run_task(
        &app,
        "ch-f24-c",
        dq_schema(
            "f24-c",
            "t_q",
            json!({ "source": "users" }),
            json!({ "entities": { "users": { "columns": {
                "id": {},
                "name": { "queryable": false }
            }}}}),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert_eq!(
        body["data"]["result"],
        json!([{ "id": "u1" }]),
        "a wildcard read must project only the queryable declared columns: {body}"
    );
}

/// An entity that declares columns and marks every one non-queryable must not
/// fall back to the `SELECT *` it was declared to prevent.
#[tokio::test]
async fn an_entity_with_no_queryable_column_is_refused_not_widened() {
    let app = app_with("f24_cn", "f24-cn-admin", "f24-cn", json!({})).await;

    let (status, body) = run_task(
        &app,
        "ch-f24-cn",
        dq_schema(
            "f24-cn",
            "t_q",
            json!({ "source": "secrets" }),
            json!({ "entities": { "secrets": { "columns": {
                "id": { "queryable": false },
                "token": { "queryable": false }
            }}}}),
        ),
    )
    .await;

    assert!(
        is_rejection(status, &body),
        "nothing is readable, so the read must be refused: status={status} body={body}"
    );
    assert!(
        !serde_json::to_string(&body)
            .expect("body")
            .contains("hunter2"),
        "and certainly nothing may be returned: {body}"
    );
}

/// The documented migration for a previously-identity-mode task: one explicit
/// opt-in line keeps it working.
#[tokio::test]
async fn identity_mode_still_works_when_asked_for_explicitly() {
    let app = app_with("f24_i", "f24-i-admin", "f24-i", json!({})).await;

    let (status, body) = run_task(
        &app,
        "ch-f24-i",
        dq_schema(
            "f24-i",
            "t_q",
            json!({ "source": "users" }),
            json!({ "unmapped": "identity" }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert_eq!(body["data"]["result"][0]["name"], "Alice", "body = {body}");
}

// ---------------------------------------------------------------------
// F24: the connector-level guards
// ---------------------------------------------------------------------

/// `require_schema` closes the per-task opt-out: the author can still *write*
/// `"unmapped": "identity"`, but this connector will not honour it.
#[tokio::test]
async fn require_schema_refuses_an_explicit_identity_opt_in() {
    let app = app_with(
        "f24_rs",
        "f24-rs-admin",
        "f24-rs",
        json!({ "require_schema": true }),
    )
    .await;

    let (status, body) = run_task(
        &app,
        "ch-f24-rs-no",
        dq_schema(
            "f24-rs",
            "t_q",
            json!({ "source": "users" }),
            json!({ "unmapped": "identity" }),
        ),
    )
    .await;
    assert!(
        is_rejection(status, &body),
        "require_schema must refuse identity mode: status={status} body={body}"
    );

    // A real schema is accepted by the same connector.
    let (status, body) = run_task(
        &app,
        "ch-f24-rs-ok",
        dq_schema(
            "f24-rs",
            "t_q",
            json!({ "source": "users" }),
            json!({ "entities": { "users": { "columns": { "id": {}, "name": {} } } } }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert_eq!(body["data"]["result"][0]["id"], "u1", "body = {body}");
}

/// `allowed_entities` binds the *physical* name, so a rename in the task's own
/// schema cannot step around it — the schema is authored per task, the
/// allowlist by the connector's owner.
#[tokio::test]
async fn allowed_entities_cannot_be_escaped_by_renaming() {
    let app = app_with(
        "f24_ae",
        "f24-ae-admin",
        "f24-ae",
        json!({ "allowed_entities": ["users"] }),
    )
    .await;

    // Honest use of the permitted table.
    let (status, body) = run_task(
        &app,
        "ch-f24-ae-ok",
        dq_schema(
            "f24-ae",
            "t_q",
            json!({ "source": "users" }),
            json!({ "entities": { "users": { "columns": { "id": {} } } } }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body = {body}");

    // A logical `users` renamed onto the physical `secrets` must not pass.
    let (status, body) = run_task(
        &app,
        "ch-f24-ae-no",
        dq_schema(
            "f24-ae",
            "t_q",
            json!({ "source": "users" }),
            json!({ "entities": { "users": {
                "physical": "secrets", "columns": { "id": {} }
            } } }),
        ),
    )
    .await;
    assert!(
        is_rejection(status, &body),
        "a rename onto a table outside allowed_entities must be refused: \
         status={status} body={body}"
    );
}

/// The guards are per connector, so an unguarded one over the same database is
/// unaffected — this is connector configuration, not a global switch.
#[tokio::test]
async fn the_guards_are_scoped_to_their_own_connector() {
    let app = app_with(
        "f24_sc",
        "f24-sc-admin",
        "f24-sc",
        json!({ "require_schema": true }),
    )
    .await;

    let (status, body) = run_task(
        &app,
        "ch-f24-sc",
        dq_schema(
            "f24-sc-admin",
            "t_q",
            json!({ "source": "users" }),
            json!({ "unmapped": "identity" }),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the unguarded connector must be unaffected: body = {body}"
    );
}

// ---------------------------------------------------------------------
// F28: the write result carries a status, and SQL is atomic
// ---------------------------------------------------------------------

/// Every `data_write` result carries `status`, so one check works across the
/// three backends' very different failure models.
#[tokio::test]
async fn a_sql_write_reports_an_ok_status() {
    let app = app_with("f28_s", "f28-s-admin", "f28-s", json!({})).await;

    let (status, body) = run_task(
        &app,
        "ch-f28-s",
        dw(
            "f28-s-admin",
            "t_w",
            json!({
                "op": "insert", "target": "users",
                "values": [{ "id": "b1", "name": "X" }, { "id": "b2", "name": "Y" }]
            }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert_eq!(body["data"]["w"]["status"], "ok", "body = {body}");
    assert_eq!(body["data"]["w"]["rows_affected"], 2, "body = {body}");
}

/// SQL is the atomic member of the three write models. A bulk insert whose
/// later row violates a constraint must leave *nothing* behind — no prefix, as
/// Mongo would, and no arbitrary subset, as Elasticsearch would.
#[tokio::test]
async fn a_failed_sql_bulk_insert_leaves_no_rows() {
    let app = app_with("f28_a", "f28-a-admin", "f28-a", json!({})).await;
    let conn = "f28-a-admin";

    // Second row duplicates the seeded primary key `u1`.
    let (status, body) = run_task(
        &app,
        "ch-f28-a-w",
        dw(
            conn,
            "t_w",
            json!({
                "op": "insert", "target": "users",
                "values": [
                    { "id": "n1", "name": "New" },
                    { "id": "u1", "name": "Clash" }
                ]
            }),
        ),
    )
    .await;
    assert!(
        is_rejection(status, &body),
        "a constraint violation must fail the write: status={status} body={body}"
    );

    // The good first row must not have survived.
    let (status, body) = run_task(
        &app,
        "ch-f28-a-r",
        dq(conn, "t_q", json!({ "source": "users" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    let rows = body["data"]["result"].as_array().expect("rows");
    assert!(
        !rows.iter().any(|r| r["id"] == "n1"),
        "SQL bulk insert must be all-or-nothing; 'n1' survived a failed batch: {body}"
    );
    assert_eq!(rows.len(), 1, "only the seeded row should remain: {body}");
}
