//! A constraint violation is its own kind of failure (#297).
//!
//! A schema rule the author deliberately expressed as a unique index, a
//! foreign key or a CHECK used to arrive at the workflow as `FUNCTION_ERROR`
//! with status `500` — the same record a deadlock, a dropped column and a type
//! mismatch produce. So an endpoint whose whole job is to answer `409` when a
//! submission is already in flight had to answer `500` instead, and no
//! spelling of a condition could tell the two apart.
//!
//! These tests pin both halves of the fix: the code a workflow can branch on,
//! and the status the edge sends when nothing catches it. The driver's own
//! text is asserted *absent* throughout — it names tables, columns, index
//! names and often the conflicting value, and it must never leave `detail`.

use crate::common;
use crate::common::backends::{Backend, BackendHarness};
use crate::common::dsl::{ddl, dq, dw, post};

use axum::http::StatusCode;
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

/// Run `tasks` on a fresh channel and return `(status, body)` without
/// asserting either — every test here is about a failure.
async fn run(app: &axum::Router, channel: &str, tasks: Vec<Value>) -> (StatusCode, Value) {
    common::create_and_activate_channel(
        app,
        channel,
        common::workflow_with_tasks("integrity", json!(tasks)),
    )
    .await;
    post(app, channel, json!({ "data": {} })).await;
    post(app, channel, json!({ "data": {} })).await
}

/// The task that would have needed `continue_on_error` to be observable, plus
/// a `map` that copies the first error record into the response. Without the
/// flag a failed task ends the run and the record is never read back.
fn classify(id: &str) -> Value {
    json!({
        "id": id, "name": id,
        "condition": { "!!": { "var": "metadata._orion_errors.0.code" } },
        "function": { "name": "map", "input": { "mappings": [
            { "path": "data.failure_code",
              "logic": { "var": "metadata._orion_errors.0.code" } },
            { "path": "data.failure_status",
              "logic": { "var": "metadata._orion_errors.0.status" } }
        ] } }
    })
}

/// Mark a task `continue_on_error` so the run survives it.
fn tolerant(mut task: Value) -> Value {
    task["continue_on_error"] = json!(true);
    task
}

// ---------------------------------------------------------------------------
// The code a workflow branches on
// ---------------------------------------------------------------------------

/// The case the issue was filed for: a second insert against a unique index.
///
/// `integrity_unique` rather than `FUNCTION_ERROR` is the whole point — it is
/// what lets the workflow shape a `409` instead of surfacing a `500`.
#[tokio::test]
async fn a_unique_violation_reaches_the_workflow_as_its_own_code() {
    let conn = "ig-uniq";
    let app = sqlite_app(conn, "ig_uniq").await;

    let (status, body) = run(
        &app,
        "ch-ig-uniq",
        vec![
            ddl(
                conn,
                "t_ddl",
                "CREATE TABLE IF NOT EXISTS models (id TEXT PRIMARY KEY, owner TEXT)",
            ),
            tolerant(dw(
                conn,
                "t_w",
                json!({ "op": "insert", "target": "models",
                        "values": { "id": "m1", "owner": "alice" } }),
            )),
            classify("t_c"),
        ],
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert_eq!(
        body["data"]["failure_code"], "integrity_unique",
        "a unique violation must be distinguishable from any other query \
         failure: body = {body}"
    );
}

/// A dangling reference is not a duplicate, and a workflow that answers both
/// with one `409` body is answering the wrong question for one of them.
///
/// SQLite enforces foreign keys only with the pragma on, so the DDL turns it
/// on for this connection — which is also the shape a real deployment uses.
#[tokio::test]
async fn a_foreign_key_violation_is_told_apart_from_a_duplicate() {
    let conn = "ig-fk";
    let app = sqlite_app(conn, "ig_fk").await;

    let (status, body) = run(
        &app,
        "ch-ig-fk",
        vec![
            ddl(conn, "t_pragma", "PRAGMA foreign_keys = ON"),
            ddl(
                conn,
                "t_ddl_p",
                "CREATE TABLE IF NOT EXISTS owners (id TEXT PRIMARY KEY)",
            ),
            ddl(
                conn,
                "t_ddl_c",
                "CREATE TABLE IF NOT EXISTS models (id TEXT PRIMARY KEY, \
                 owner TEXT REFERENCES owners(id))",
            ),
            tolerant(dw(
                conn,
                "t_w",
                json!({ "op": "insert", "target": "models",
                        "values": { "id": "m9", "owner": "nobody" } }),
            )),
            classify("t_c"),
        ],
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert_eq!(
        body["data"]["failure_code"], "integrity_foreign_key",
        "body = {body}"
    );
}

/// A failed CHECK is a value the caller sent wrong, so it gets its own code
/// too — an endpoint may well answer it `400` while answering a duplicate
/// `409`.
#[tokio::test]
async fn a_check_violation_has_its_own_code() {
    let conn = "ig-chk";
    let app = sqlite_app(conn, "ig_chk").await;

    let (status, body) = run(
        &app,
        "ch-ig-chk",
        vec![
            ddl(
                conn,
                "t_ddl",
                "CREATE TABLE IF NOT EXISTS scores (id TEXT PRIMARY KEY, \
                 v INTEGER CHECK (v >= 0))",
            ),
            tolerant(dw(
                conn,
                "t_w",
                json!({ "op": "insert", "target": "scores",
                        "values": { "id": "s1", "v": -5 } }),
            )),
            classify("t_c"),
        ],
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert_eq!(
        body["data"]["failure_code"], "integrity_check",
        "body = {body}"
    );
}

/// A missing required value. `sqlx` reports it as its own kind on every
/// driver, and the three predicates the issue proposed would have missed it.
#[tokio::test]
async fn a_not_null_violation_has_its_own_code() {
    let conn = "ig-nn";
    let app = sqlite_app(conn, "ig_nn").await;

    let (status, body) = run(
        &app,
        "ch-ig-nn",
        vec![
            ddl(
                conn,
                "t_ddl",
                "CREATE TABLE IF NOT EXISTS people (id TEXT PRIMARY KEY, name TEXT NOT NULL)",
            ),
            tolerant(dw(
                conn,
                "t_w",
                json!({ "op": "insert", "target": "people",
                        "values": { "id": "p1", "name": null } }),
            )),
            classify("t_c"),
        ],
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert_eq!(
        body["data"]["failure_code"], "integrity_not_null",
        "body = {body}"
    );
}

/// Everything that is *not* a declared-constraint refusal stays exactly where
/// it was. The catch-all is the larger half of the behaviour and a change that
/// widened it would be hard to notice.
#[tokio::test]
async fn an_ordinary_query_failure_is_still_a_backend_failure() {
    let conn = "ig-plain";
    let app = sqlite_app(conn, "ig_plain").await;

    let (status, body) = run(
        &app,
        "ch-ig-plain",
        vec![
            ddl(
                conn,
                "t_ddl",
                "CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY)",
            ),
            tolerant(dq(conn, "t_r", json!({ "source": "no_such_table_at_all" }))),
            classify("t_c"),
        ],
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert_eq!(
        body["data"]["failure_code"], "FUNCTION_ERROR",
        "a missing table is not an integrity violation: body = {body}"
    );
}

// ---------------------------------------------------------------------------
// What the caller sees
// ---------------------------------------------------------------------------

/// The driver's text names the table, the index and often the value that
/// conflicted. It must stay in the operator-only `detail`.
///
/// Deliberately run on the **uncaught** path. That is the one where the
/// assertion is load-bearing: `engine_error_response` hands a `Service`
/// error's `Display` straight to the caller with no sanitising step, so a
/// later "improvement" that passed the driver message through as the
/// caller-facing half would surface here and nowhere else. On the
/// `continue_on_error` path `sanitize_errors` replaces every message anyway,
/// so the same assertion there would pass whatever this code did.
#[tokio::test]
async fn the_driver_text_never_reaches_the_caller() {
    let conn = "ig-leak";
    let app = sqlite_app(conn, "ig_leak").await;

    let (status, body) = run(
        &app,
        "ch-ig-leak",
        vec![
            ddl(
                conn,
                "t_ddl",
                "CREATE TABLE IF NOT EXISTS secret_ledger (id TEXT PRIMARY KEY)",
            ),
            dw(
                conn,
                "t_w",
                json!({ "op": "insert", "target": "secret_ledger",
                        "values": { "id": "row-42" } }),
            ),
        ],
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT, "body = {body}");
    assert_eq!(
        body["error"]["message"], "The request conflicts with an existing record",
        "the caller gets the generic sentence, not the driver's: body = {body}"
    );

    let rendered = body.to_string();
    for leaked in ["secret_ledger", "UNIQUE constraint", "row-42", "db_write"] {
        assert!(
            !rendered.contains(leaked),
            "the response body leaked {leaked:?} from the driver message: {rendered}"
        );
    }
}

/// With nothing catching it, the run ends and the edge answers for itself.
/// A duplicate is a conflict of state, not a server fault.
#[tokio::test]
async fn an_uncaught_unique_violation_answers_409() {
    let conn = "ig-409";
    let app = sqlite_app(conn, "ig_409").await;

    let (status, body) = run(
        &app,
        "ch-ig-409",
        vec![
            ddl(
                conn,
                "t_ddl",
                "CREATE TABLE IF NOT EXISTS orders (id TEXT PRIMARY KEY)",
            ),
            dw(
                conn,
                "t_w",
                json!({ "op": "insert", "target": "orders", "values": { "id": "o1" } }),
            ),
        ],
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT, "body = {body}");
    assert_eq!(body["error"]["code"], "CONFLICT", "body = {body}");
}

/// The other half of the status split: a value the schema refuses is the same
/// shape as a failed field rule, so it gets the same answer.
#[tokio::test]
async fn an_uncaught_check_violation_answers_400() {
    let conn = "ig-400";
    let app = sqlite_app(conn, "ig_400").await;

    let (status, body) = run(
        &app,
        "ch-ig-400",
        vec![
            ddl(
                conn,
                "t_ddl",
                "CREATE TABLE IF NOT EXISTS gauges (id TEXT PRIMARY KEY, \
                 v INTEGER CHECK (v >= 0))",
            ),
            dw(
                conn,
                "t_w",
                json!({ "op": "insert", "target": "gauges",
                        "values": { "id": "g1", "v": -1 } }),
            ),
        ],
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "body = {body}");
    assert_eq!(body["error"]["code"], "VALIDATION_ERROR", "body = {body}");
}

// ---------------------------------------------------------------------------
// The breaker
// ---------------------------------------------------------------------------

/// No code makes this true — `guarded_handler` counts a failure only when the
/// error declares itself retryable, and an integrity error declares `false`.
/// It is asserted precisely *because* nothing enforces it: a later change that
/// made the class retryable would take a healthy connector offline because
/// callers kept posting duplicates, and the only symptom would be a `503`.
#[tokio::test]
async fn repeated_violations_do_not_trip_the_circuit_breaker() {
    let conn = "ig-cb";
    let app = sqlite_app(conn, "ig_cb").await;

    common::create_and_activate_channel(
        &app,
        "ch-ig-cb",
        common::workflow_with_tasks(
            "integrity",
            json!([
                ddl(
                    conn,
                    "t_ddl",
                    "CREATE TABLE IF NOT EXISTS dupes (id TEXT PRIMARY KEY)"
                ),
                dw(
                    conn,
                    "t_w",
                    json!({ "op": "insert", "target": "dupes", "values": { "id": "d1" } })
                ),
            ]),
        ),
    )
    .await;

    // The first insert succeeds; every one after it violates the primary key.
    // Well past any plausible breaker threshold.
    let mut statuses = Vec::new();
    for _ in 0..12 {
        let (status, _) = post(&app, "ch-ig-cb", json!({ "data": {} })).await;
        statuses.push(status);
    }

    assert!(
        statuses[1..].iter().all(|s| *s == StatusCode::CONFLICT),
        "a stream of duplicates must keep answering 409, never 503: {statuses:?}"
    );
}

// ---------------------------------------------------------------------------
// Cross-backend agreement
// ---------------------------------------------------------------------------

/// The same duplicate insert on every SQL backend, asserting the same code.
///
/// This is the case the rest of the file cannot make: the classification comes
/// from `sqlx`'s per-driver `ErrorKind` mapping, and `data_write` executes
/// against `sqlx::Any`. That the concrete driver's error — and so its
/// classification — survives the Any driver's type erasure is an inference
/// from sqlx's source, not a documented guarantee, so it is asserted per
/// backend rather than trusted.
///
/// Scoped to a unique violation deliberately. It is the case the issue was
/// filed for, and it is the one constraint whose enforcement needs no pragma
/// (SQLite foreign keys), no storage engine (MySQL InnoDB) and no `sql_mode`
/// (MySQL strict mode) — so a failure here is a real divergence rather than a
/// container's configuration.
async fn assert_unique_violation_is_classified(backend: Backend, name: &str) {
    let harness: BackendHarness = common::backends::start(backend, name).await;
    let app = common::test_app().await;
    common::create_connector(&app, harness.connector_json()).await;
    let conn = harness.connector_name.as_str();

    // MySQL cannot index a `TEXT` primary key without a prefix length.
    let id_type = if matches!(backend, Backend::Mysql) {
        "VARCHAR(64)"
    } else {
        "TEXT"
    };

    let channel = format!("ch-ig-x-{}", backend.label());
    common::create_and_activate_channel(
        &app,
        &channel,
        common::workflow_with_tasks(
            "integrity",
            json!([
                ddl(
                    conn,
                    "t_ddl",
                    &format!("CREATE TABLE IF NOT EXISTS ig_dupes (id {id_type} PRIMARY KEY)")
                ),
                tolerant(dw(
                    conn,
                    "t_w",
                    json!({ "op": "insert", "target": "ig_dupes", "values": { "id": "x1" } })
                )),
                classify("t_c"),
            ]),
        ),
    )
    .await;

    let (_, first) = post(&app, &channel, json!({ "data": {} })).await;
    assert!(
        first["data"]["failure_code"].is_null(),
        "the first insert must succeed on {}: {first}",
        backend.label()
    );

    let (status, body) = post(&app, &channel, json!({ "data": {} })).await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert_eq!(
        body["data"]["failure_code"],
        "integrity_unique",
        "{} did not classify a duplicate key: body = {body}",
        backend.label()
    );
}

#[tokio::test]
async fn a_unique_violation_is_classified_on_sqlite() {
    assert_unique_violation_is_classified(Backend::Sqlite, "ig-x-sqlite").await;
}

#[tokio::test]
#[ignore]
async fn a_unique_violation_is_classified_on_postgres() {
    assert_unique_violation_is_classified(Backend::Postgres, "ig-x-postgres").await;
}

#[tokio::test]
#[ignore]
async fn a_unique_violation_is_classified_on_mysql() {
    assert_unique_violation_is_classified(Backend::Mysql, "ig-x-mysql").await;
}
