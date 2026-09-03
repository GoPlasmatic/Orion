//! Column-type fidelity for `db_read` (proposal F9).
//!
//! Two defects, one after the other, and this file is the record of both.
//!
//! First, the decoder probed `String` → `i64` → `f64` → `bool` and fell through
//! to `Value::Null`, so any value it did not recognise came back as null and was
//! indistinguishable from a genuine SQL NULL (F9). Matching the value's own type
//! info fixed that — it errors rather than inventing a null.
//!
//! Then the *ceiling* on what could be matched turned out to be nine types
//! (#309): connector queries ran on `sqlx::Any`, whose `AnyTypeInfoKind` cannot
//! spell `uuid`, `jsonb`, `numeric`, an array or an enum, so a `SELECT *` over
//! any ordinary schema failed — and only when a row existed. Decoding on the
//! real driver removed it.
//!
//! `data_query` and `data_write`'s `returning` share the decoder, so these
//! assertions cover all three read paths.

use crate::common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

/// SQLite exercises the BLOB path (previously null) alongside the scalar kinds,
/// and pins down that a real NULL still reads back as null.
#[tokio::test]
async fn sqlite_column_kinds_round_trip() {
    let app = common::test_app().await;

    common::create_connector(
        &app,
        common::db_connector_sqlite(
            "types-db",
            "sqlite:file:db_column_types_sqlite?mode=memory&cache=shared",
        ),
    )
    .await;

    common::create_and_activate_channel(
        &app,
        "types-ch",
        common::workflow_with_tasks(
            "ColumnKinds",
            json!([
                {
                    "id": "t1",
                    "name": "Create table",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "types-db",
                            "query": "CREATE TABLE IF NOT EXISTS typed_rows (id INTEGER PRIMARY KEY, ratio REAL, label TEXT, payload BLOB, missing TEXT)",
                            "output": "data.created"
                        }
                    }
                },
                {
                    "id": "t2",
                    "name": "Insert one row of each kind",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "types-db",
                            "query": "INSERT INTO typed_rows (id, ratio, label, payload, missing) VALUES (1, 0.5, 'hello', X'00ff10', NULL)",
                            "output": "data.inserted"
                        }
                    }
                },
                {
                    "id": "t3",
                    "name": "Read it back",
                    "function": {
                        "name": "db_read",
                        "input": {
                            "connector": "types-db",
                            "query": "SELECT id, ratio, label, payload, missing FROM typed_rows WHERE id = 1",
                            "output": "data.rows"
                        }
                    }
                }
            ]),
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/types-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .expect("request");

    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    let row = &body["data"]["rows"][0];

    assert_eq!(row["id"], 1, "got {body}");
    assert_eq!(row["ratio"], 0.5, "got {body}");
    assert_eq!(row["label"], "hello", "got {body}");
    // Non-UTF-8 bytes are hex-encoded rather than dropped to null.
    assert_eq!(row["payload"], "00ff10", "got {body}");
    // A genuine SQL NULL is still null — the one case that may be.
    assert!(row["missing"].is_null(), "got {body}");
}

/// A BLOB holding UTF-8 comes back as readable text. This is the common case on
/// MySQL, whose protocol reports TEXT/JSON columns as BLOB.
#[tokio::test]
async fn utf8_blob_reads_back_as_text() {
    let app = common::test_app().await;

    common::create_connector(
        &app,
        common::db_connector_sqlite(
            "blob-db",
            "sqlite:file:db_column_types_blob?mode=memory&cache=shared",
        ),
    )
    .await;

    common::create_and_activate_channel(
        &app,
        "blob-ch",
        common::workflow_with_tasks(
            "Utf8Blob",
            json!([
                {
                    "id": "t1",
                    "name": "Create table",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "blob-db",
                            "query": "CREATE TABLE IF NOT EXISTS blob_rows (id INTEGER PRIMARY KEY, doc BLOB)",
                            "output": "data.created"
                        }
                    }
                },
                {
                    "id": "t2",
                    "name": "Insert a UTF-8 blob",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "blob-db",
                            "query": "INSERT INTO blob_rows (id, doc) VALUES (1, CAST('{\"a\":1}' AS BLOB))",
                            "output": "data.inserted"
                        }
                    }
                },
                {
                    "id": "t3",
                    "name": "Read it back",
                    "function": {
                        "name": "db_read",
                        "input": {
                            "connector": "blob-db",
                            "query": "SELECT id, doc FROM blob_rows WHERE id = 1",
                            "output": "data.rows"
                        }
                    }
                }
            ]),
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/blob-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .expect("request");

    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["data"]["rows"][0]["doc"], "{\"a\":1}", "got {body}");
}

/// PostgreSQL `real` (float4) maps to `AnyTypeInfoKind::Real`, which no probe in
/// the old cascade accepted — the column read back as null. SQLite has no
/// distinct 32-bit float, so this assertion needs a container.
///
/// Requires Docker; run with
/// `cargo test --test integration -- --ignored db_column_types`.
#[tokio::test]
#[ignore]
async fn postgres_real_and_bytea_are_not_null() {
    use crate::common::backends::Backend;

    let app = common::test_app().await;
    let h = common::backends::start(Backend::Postgres, "pg-types").await;
    common::create_connector(&app, h.connector_json()).await;

    common::create_and_activate_channel(
        &app,
        "pg-types-ch",
        common::workflow_with_tasks(
            "PgColumnKinds",
            json!([
                {
                    "id": "t1",
                    "name": "Drop",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "pg-types",
                            "query": "DROP TABLE IF EXISTS pg_typed_rows",
                            "output": "data.dropped"
                        }
                    }
                },
                {
                    "id": "t2",
                    "name": "Create",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "pg-types",
                            "query": "CREATE TABLE pg_typed_rows (id INTEGER PRIMARY KEY, ratio REAL, weight DOUBLE PRECISION, raw BYTEA, missing TEXT)",
                            "output": "data.created"
                        }
                    }
                },
                {
                    "id": "t3",
                    "name": "Insert",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "pg-types",
                            "query": "INSERT INTO pg_typed_rows (id, ratio, weight, raw, missing) VALUES (1, 0.5, 2.25, '\\x00ff10'::bytea, NULL)",
                            "output": "data.inserted"
                        }
                    }
                },
                {
                    "id": "t4",
                    "name": "Read",
                    "function": {
                        "name": "db_read",
                        "input": {
                            "connector": "pg-types",
                            "query": "SELECT id, ratio, weight, raw, missing FROM pg_typed_rows WHERE id = 1",
                            "output": "data.rows"
                        }
                    }
                }
            ]),
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/pg-types-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .expect("request");

    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    let row = &body["data"]["rows"][0];

    assert_eq!(row["id"], 1, "got {body}");
    assert_eq!(row["ratio"], 0.5, "REAL must not read back as null: {body}");
    assert_eq!(row["weight"], 2.25, "got {body}");
    assert_eq!(
        row["raw"], "00ff10",
        "BYTEA must not read back as null: {body}"
    );
    assert!(row["missing"].is_null(), "got {body}");
}

// ---------------------------------------------------------------------------
// The type ceiling (#309)
// ---------------------------------------------------------------------------

/// Build a channel that creates a table, inserts one row, and reads it back.
fn round_trip_workflow(
    name: &str,
    connector: &str,
    ddl: &[&str],
    select: &str,
) -> serde_json::Value {
    let mut tasks: Vec<serde_json::Value> = ddl
        .iter()
        .enumerate()
        .map(|(i, sql)| {
            json!({
                "id": format!("ddl{i}"),
                "name": format!("Statement {i}"),
                "function": {
                    "name": "db_write",
                    "input": {
                        "connector": connector,
                        "query": sql,
                        "output": format!("data.ddl{i}")
                    }
                }
            })
        })
        .collect();
    tasks.push(json!({
        "id": "read",
        "name": "Read",
        "function": {
            "name": "db_read",
            "input": { "connector": connector, "query": select, "output": "data.rows" }
        }
    }));
    common::workflow_with_tasks(name, json!(tasks))
}

/// The headline of #309: nine decodable Postgres types became the vocabulary an
/// ordinary schema actually uses.
///
/// Every column here failed the task with a `500` before native decoding — and
/// only once a row existed, so the identical query returned `200 []` against an
/// empty table. `uuid` and `timestamptz` are the ones that make this
/// unavoidable: they are ordinary primary-key and audit-column types, so almost
/// any real `SELECT *` hit it.
///
/// Requires Docker; run with
/// `cargo test --test integration -- --ignored db_column_types`.
#[tokio::test]
#[ignore]
async fn postgres_decodes_the_types_an_ordinary_schema_uses() {
    use crate::common::backends::Backend;

    let app = common::test_app().await;
    let h = common::backends::start(Backend::Postgres, "pg-types").await;
    common::create_connector(&app, h.connector_json()).await;

    common::create_and_activate_channel(
        &app,
        "pg-types-ch",
        round_trip_workflow(
            "PgTypes",
            "pg-types",
            &[
                "DROP TABLE IF EXISTS pg_types",
                "DROP TYPE IF EXISTS user_role",
                "CREATE TYPE user_role AS ENUM ('admin', 'member')",
                "CREATE TABLE pg_types (
                   id          uuid PRIMARY KEY,
                   doc         jsonb NOT NULL,
                   plain       json NOT NULL,
                   amount      numeric(12,2) NOT NULL,
                   created     timestamptz NOT NULL,
                   naive       timestamp NOT NULL,
                   day         date NOT NULL,
                   tags        text[] NOT NULL,
                   scores      int4[] NOT NULL,
                   role        user_role NOT NULL,
                   big         bigint NOT NULL
                 )",
                "INSERT INTO pg_types VALUES (
                   '11111111-2222-3333-4444-555555555555',
                   '{\"a\": 1}'::jsonb,
                   '{\"b\": [2]}'::json,
                   1234.56,
                   '2026-09-02T05:00:00Z',
                   '2026-09-02T05:00:00',
                   '2026-09-02',
                   ARRAY['x','y'],
                   ARRAY[1,2,3],
                   'admin',
                   9007199254740993
                 )",
            ],
            "SELECT * FROM pg_types",
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/pg-types-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert!(
        body["errors"].as_array().is_some_and(|a| a.is_empty()),
        "no column may fail to decode: {body}"
    );
    let row = &body["data"]["rows"][0];

    assert_eq!(row["id"], "11111111-2222-3333-4444-555555555555");
    // The document itself, not a string to re-parse — the whole reason a
    // workflow stores one is to read it back.
    assert_eq!(row["doc"], json!({"a": 1}));
    assert_eq!(row["plain"], json!({"b": [2]}));
    assert_eq!(row["amount"], json!(1234.56));
    assert_eq!(row["created"], "2026-09-02T05:00:00Z");
    assert_eq!(row["day"], "2026-09-02");
    assert_eq!(row["tags"], json!(["x", "y"]));
    assert_eq!(row["scores"], json!([1, 2, 3]));
    assert_eq!(row["role"], "admin");
    // `i64` is exact in JSON, so a bigint past 2^53 survives where a float
    // would not — which is why only `numeric` needs `numeric_as`.
    assert_eq!(row["big"], json!(9007199254740993i64));
}

/// `numeric_as: "string"` keeps every digit. The default rounds, which is the
/// documented trade and the reason the opt-out exists.
///
/// Requires Docker; run with
/// `cargo test --test integration -- --ignored db_column_types`.
#[tokio::test]
#[ignore]
async fn postgres_numeric_as_string_keeps_every_digit() {
    use crate::common::backends::Backend;

    let app = common::test_app().await;
    let h = common::backends::start(Backend::Postgres, "pg-numeric").await;
    common::create_connector(&app, h.connector_json()).await;

    // 25 significant digits: no f64 holds this.
    let exact = "1234567890123456789.0123456";
    common::create_and_activate_channel(
        &app,
        "pg-numeric-ch",
        common::workflow_with_tasks(
            "PgNumeric",
            json!([
                {"id": "d0", "name": "Drop", "function": {"name": "db_write", "input": {
                    "connector": "pg-numeric", "query": "DROP TABLE IF EXISTS pg_money", "output": "data.d0"}}},
                {"id": "d1", "name": "Create", "function": {"name": "db_write", "input": {
                    "connector": "pg-numeric",
                    "query": "CREATE TABLE pg_money (id int PRIMARY KEY, total numeric(40,7) NOT NULL)",
                    "output": "data.d1"}}},
                {"id": "d2", "name": "Insert", "function": {"name": "db_write", "input": {
                    "connector": "pg-numeric",
                    "query": format!("INSERT INTO pg_money VALUES (1, {exact})"),
                    "output": "data.d2"}}},
                {"id": "exact", "name": "Read exact", "function": {"name": "db_read", "input": {
                    "connector": "pg-numeric",
                    "query": "SELECT total FROM pg_money WHERE id = 1",
                    "numeric_as": "string",
                    "output": "data.exact"}}},
                {"id": "rounded", "name": "Read default", "function": {"name": "db_read", "input": {
                    "connector": "pg-numeric",
                    "query": "SELECT total FROM pg_money WHERE id = 1",
                    "output": "data.rounded"}}}
            ]),
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/pg-numeric-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;

    let exact_read = body["data"]["exact"][0]["total"]
        .as_str()
        .unwrap_or_else(|| panic!("a string was asked for: {body}"));
    assert!(
        exact_read.starts_with("1234567890123456789.0123456"),
        "every digit must survive: {exact_read}"
    );
    // The default is a number, and it is the lossy one — asserted so the
    // trade-off is a stated contract rather than an accident.
    assert!(
        body["data"]["rounded"][0]["total"].is_number(),
        "the default renders a JSON number: {body}"
    );
}

/// The *parameter* side still needs a cast, and this pins that down so the
/// asymmetry is a documented contract rather than a surprise.
///
/// Native decoding removed the ceiling on what a query can **return**. It does
/// not remove the cast on what a query **binds**, and that is a property of
/// PostgreSQL rather than of the driver layer: parameters are typed, sqlx sends
/// a JSON string as `text`, and `text = uuid` has no operator — so
/// `WHERE id = ($1)::uuid` is still how a uuid parameter is written. The same
/// holds for `numeric`, `timestamptz` and the rest of the non-text family.
///
/// Requires Docker; run with
/// `cargo test --test integration -- --ignored db_column_types`.
#[tokio::test]
#[ignore]
async fn postgres_a_non_text_parameter_is_cast_in_the_query() {
    use crate::common::backends::Backend;

    let app = common::test_app().await;
    let h = common::backends::start(Backend::Postgres, "pg-bind").await;
    common::create_connector(&app, h.connector_json()).await;

    const ID: &str = "11111111-2222-3333-4444-555555555555";
    common::create_and_activate_channel(
        &app,
        "pg-bind-ch",
        common::workflow_with_tasks(
            "PgBind",
            json!([
                {"id": "d0", "name": "Drop", "function": {"name": "db_write", "input": {
                    "connector": "pg-bind", "query": "DROP TABLE IF EXISTS pg_bind", "output": "data.d0"}}},
                {"id": "d1", "name": "Create", "function": {"name": "db_write", "input": {
                    "connector": "pg-bind",
                    "query": "CREATE TABLE pg_bind (id uuid PRIMARY KEY, label text NOT NULL)",
                    "output": "data.d1"}}},
                {"id": "d2", "name": "Insert", "function": {"name": "db_write", "input": {
                    "connector": "pg-bind",
                    "query": "INSERT INTO pg_bind VALUES (($1)::uuid, 'found')",
                    "params": [ID],
                    "output": "data.d2"}}},
                {"id": "read", "name": "Read", "function": {"name": "db_read", "input": {
                    "connector": "pg-bind",
                    "query": "SELECT label FROM pg_bind WHERE id = ($1)::uuid",
                    "params": [ID],
                    "output": "data.rows"}}}
            ]),
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/pg-bind-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .expect("request");
    let status = resp.status();
    let body = common::body_json(resp).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["errors"].as_array().is_some_and(|a| a.is_empty()),
        "a cast uuid parameter must bind: {body}"
    );
    assert_eq!(
        body["data"]["rows"][0]["label"], "found",
        "the row must be found by its uuid: {body}"
    );
}

/// The long tail is a named `400`, not a `500` with the reason in the log.
///
/// `inet` has no JSON form here and is not worth one; what the author needs is
/// to be told which column, what its type is, and what to do — which is a
/// different thing from "An internal engine error occurred".
///
/// Requires Docker; run with
/// `cargo test --test integration -- --ignored db_column_types`.
#[tokio::test]
#[ignore]
async fn postgres_a_type_with_no_json_form_names_the_column_and_the_remedy() {
    use crate::common::backends::Backend;

    let app = common::test_app().await;
    let h = common::backends::start(Backend::Postgres, "pg-exotic").await;
    common::create_connector(&app, h.connector_json()).await;

    common::create_and_activate_channel(
        &app,
        "pg-exotic-ch",
        round_trip_workflow(
            "PgExotic",
            "pg-exotic",
            &[
                "DROP TABLE IF EXISTS pg_exotic",
                "CREATE TABLE pg_exotic (id int PRIMARY KEY, source inet NOT NULL)",
                "INSERT INTO pg_exotic VALUES (1, '10.0.0.1')",
            ],
            "SELECT * FROM pg_exotic",
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/pg-exotic-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .expect("request");
    let body = common::body_json(resp).await;
    let rendered = body.to_string();

    assert!(rendered.contains("source"), "name the column: {body}");
    assert!(rendered.contains("INET"), "name the SQL type: {body}");
    assert!(
        rendered.contains("::text"),
        "say what to do about it: {body}"
    );
    assert!(
        body["data"]["rows"].is_null()
            || body["data"]["rows"].as_array().is_none_or(|a| a.is_empty()),
        "no row should have been produced: {body}"
    );
}

// ---------------------------------------------------------------------------
// MySQL (#1) — the backend this file had no coverage of at all
// ---------------------------------------------------------------------------

/// The MySQL twin of `postgres_decodes_the_types_an_ordinary_schema_uses`.
///
/// This file tested Postgres and SQLite and never MySQL, which is the reason
/// the `BOOLEAN` defect survived review: sqlx names a `Tiny` column `BOOLEAN`
/// whenever its display width is 1 — every `BOOLEAN`, `BOOL` and `TINYINT(1)`
/// column there is — and no arm matched it, so the *whole query* failed with
/// `no JSON representation for BOOLEAN is defined` the moment a row existed.
///
/// The `errors` assertion is the load-bearing one: it fails loudly if any name
/// in the MySQL table goes stale again, whatever the column happens to be.
///
/// `perms SET(...)` and `flag_u TINYINT(1) UNSIGNED` are here for their *names*
/// rather than their values. MySQL sends a `SET` as a string carrying the SET
/// flag, which sqlx's naming never consults, so it arrives spelled `CHAR`; and
/// whether an unsigned width-1 column keeps its width is a server-version
/// detail. Both must decode either way, which is what this test pins.
///
/// Requires Docker; run with
/// `cargo test --test integration -- --ignored db_column_types`.
#[tokio::test]
#[ignore]
async fn mysql_decodes_the_types_an_ordinary_schema_uses() {
    use crate::common::backends::Backend;

    let app = common::test_app().await;
    let h = common::backends::start(Backend::Mysql, "mysql-types").await;
    common::create_connector(&app, h.connector_json()).await;

    common::create_and_activate_channel(
        &app,
        "mysql-types-ch",
        round_trip_workflow(
            "MysqlTypes",
            "mysql-types",
            &[
                "DROP TABLE IF EXISTS mysql_types",
                "CREATE TABLE mysql_types (
                   flag      BOOLEAN NOT NULL,
                   flag_u    TINYINT(1) UNSIGNED NOT NULL,
                   tiny      TINYINT NOT NULL,
                   sm        SMALLINT NOT NULL,
                   yr        YEAR NOT NULL,
                   iu        INT UNSIGNED NOT NULL,
                   big       BIGINT NOT NULL,
                   amount    DECIMAL(12,2) NOT NULL,
                   dbl       DOUBLE NOT NULL,
                   vc        VARCHAR(16) NOT NULL,
                   ch        CHAR(2) NOT NULL,
                   txt       TEXT NOT NULL,
                   role      ENUM('admin','member') NOT NULL,
                   perms     SET('read','write') NOT NULL,
                   doc       JSON NOT NULL,
                   raw       BLOB NOT NULL,
                   dt        DATETIME NOT NULL,
                   d         DATE NOT NULL,
                   tm        TIME NOT NULL
                 )",
                "INSERT INTO mysql_types VALUES (
                   1, 1, 7, 300, 2026, 4294967295, 9007199254740993,
                   1234.56, 2.25, 'vee', 'IN', 'texty', 'admin', 'read,write',
                   '{\"a\": 1}', X'00ff10', '2026-09-02 05:00:00', '2026-09-02',
                   '05:30:00'
                 )",
            ],
            "SELECT * FROM mysql_types",
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/mysql-types-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert!(
        body["errors"].as_array().is_some_and(|a| a.is_empty()),
        "no column may fail to decode: {body}"
    );
    let row = &body["data"]["rows"][0];

    assert_eq!(row["flag"], json!(true), "got {body}");
    assert_eq!(
        row["tiny"],
        json!(7),
        "a plain TINYINT stays a number: {body}"
    );
    assert_eq!(row["sm"], json!(300), "got {body}");
    // `YEAR` is unsigned and sqlx keeps it out of the signed integer set, so
    // this shared an arm with SMALLINT and decoded nothing.
    assert_eq!(row["yr"], json!(2026), "got {body}");
    assert_eq!(row["iu"], json!(4_294_967_295u32), "got {body}");
    // `i64` is exact in JSON, so a bigint past 2^53 survives.
    assert_eq!(row["big"], json!(9007199254740993i64), "got {body}");
    assert_eq!(row["amount"], json!(1234.56), "got {body}");
    assert_eq!(row["dbl"], json!(2.25), "got {body}");
    assert_eq!(row["vc"], "vee", "got {body}");
    assert_eq!(row["ch"], "IN", "got {body}");
    assert_eq!(row["txt"], "texty", "got {body}");
    assert_eq!(row["role"], "admin", "got {body}");
    assert_eq!(
        row["perms"], "read,write",
        "a SET arrives spelled CHAR: {body}"
    );
    // The document itself, not a string to re-parse.
    assert_eq!(row["doc"], json!({"a": 1}), "got {body}");
    // Non-UTF-8 bytes are hex-encoded rather than dropped to null.
    assert_eq!(row["raw"], "00ff10", "got {body}");
    assert_eq!(row["dt"], "2026-09-02 05:00:00", "got {body}");
    assert_eq!(row["d"], "2026-09-02", "got {body}");
    assert_eq!(row["tm"], "05:30:00", "got {body}");
}

/// The rendering decision, pinned: a MySQL boolean is a JSON boolean.
///
/// MySQL has no boolean type — `BOOLEAN` and `BOOL` are aliases for
/// `TINYINT(1)` — but sqlx reports all three as `BOOLEAN`, so Orion can and
/// does answer `true`/`false`, agreeing with Postgres `bool`. A `TINYINT`
/// without the width is a different column and stays a number, which is what
/// keeps this from being a guess about every small integer.
///
/// Requires Docker; run with
/// `cargo test --test integration -- --ignored db_column_types`.
#[tokio::test]
#[ignore]
async fn mysql_boolean_reads_back_as_a_json_boolean() {
    use crate::common::backends::Backend;

    let app = common::test_app().await;
    let h = common::backends::start(Backend::Mysql, "mysql-bool").await;
    common::create_connector(&app, h.connector_json()).await;

    common::create_and_activate_channel(
        &app,
        "mysql-bool-ch",
        round_trip_workflow(
            "MysqlBool",
            "mysql-bool",
            &[
                "DROP TABLE IF EXISTS mysql_bool",
                "CREATE TABLE mysql_bool (
                   as_boolean BOOLEAN NOT NULL,
                   as_bool    BOOL NOT NULL,
                   as_tiny1   TINYINT(1) NOT NULL,
                   falsy      BOOLEAN NOT NULL,
                   counter    TINYINT NOT NULL
                 )",
                "INSERT INTO mysql_bool VALUES (1, 1, 1, 0, 7)",
            ],
            "SELECT * FROM mysql_bool",
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/mysql-bool-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert!(
        body["errors"].as_array().is_some_and(|a| a.is_empty()),
        "a BOOLEAN column must decode at all: {body}"
    );
    let row = &body["data"]["rows"][0];

    assert_eq!(row["as_boolean"], json!(true), "got {body}");
    assert_eq!(row["as_bool"], json!(true), "got {body}");
    assert_eq!(row["as_tiny1"], json!(true), "got {body}");
    assert_eq!(row["falsy"], json!(false), "got {body}");
    // The width is the whole signal, so a TINYINT without one is still a number.
    assert_eq!(row["counter"], json!(7), "got {body}");
}

// ---------------------------------------------------------------------------
// PostgreSQL char(n) / citext (#2)
// ---------------------------------------------------------------------------

/// `char(n)` and `citext` decode, scalar and array alike.
///
/// Both arms existed and neither could ever match. `PgTypeInfo::name()` is
/// sqlx's *display* name, and it spells `bpchar` — `char(n)` — as `CHAR`, not
/// `BPCHAR`; an extension type is named from `oid::regtype::text`, so `citext`
/// is lowercase. A `country char(2)` column therefore answered
/// `no JSON representation for CHAR is defined`, which is the message a reader
/// would least expect from a table that lists `BPCHAR` precisely to support it.
///
/// The array half is the same table written twice: `pg_array` matches the
/// element name `pg_column` reads out of `PgTypeKind::Array`, so `char(2)[]`
/// arrives as `CHAR` and `citext[]` as `citext` — and `time[]` is here because
/// the scalar table decoded `TIME` while the array table did not, an asymmetry
/// no author could predict from the docs.
///
/// Requires Docker; run with
/// `cargo test --test integration -- --ignored db_column_types`.
#[tokio::test]
#[ignore]
async fn postgres_char_and_citext_decode_scalar_and_array() {
    use crate::common::backends::Backend;

    let app = common::test_app().await;
    let h = common::backends::start(Backend::Postgres, "pg-char").await;
    common::create_connector(&app, h.connector_json()).await;

    common::create_and_activate_channel(
        &app,
        "pg-char-ch",
        round_trip_workflow(
            "PgChar",
            "pg-char",
            &[
                "CREATE EXTENSION IF NOT EXISTS citext",
                "DROP TABLE IF EXISTS pg_char",
                "CREATE TABLE pg_char (
                   id       int PRIMARY KEY,
                   country  char(2) NOT NULL,
                   label    citext NOT NULL,
                   nick     name NOT NULL,
                   codes    char(2)[] NOT NULL,
                   names    citext[] NOT NULL,
                   slot     time NOT NULL,
                   slots    time[] NOT NULL
                 )",
                "INSERT INTO pg_char VALUES (
                   1, 'IN', 'MixedCase', 'nickname',
                   ARRAY['IN','US']::char(2)[],
                   ARRAY['One','Two']::citext[],
                   '05:30:00',
                   ARRAY['05:30:00','06:45:00']::time[]
                 )",
            ],
            "SELECT * FROM pg_char",
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/pg-char-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert!(
        body["errors"].as_array().is_some_and(|a| a.is_empty()),
        "no column may fail to decode: {body}"
    );
    let row = &body["data"]["rows"][0];

    assert_eq!(row["country"], "IN", "char(n) is CHAR, not BPCHAR: {body}");
    assert_eq!(row["label"], "MixedCase", "citext is lowercase: {body}");
    assert_eq!(row["nick"], "nickname", "got {body}");
    assert_eq!(row["codes"], json!(["IN", "US"]), "got {body}");
    assert_eq!(row["names"], json!(["One", "Two"]), "got {body}");
    assert_eq!(row["slot"], "05:30:00", "got {body}");
    assert_eq!(
        row["slots"],
        json!(["05:30:00", "06:45:00"]),
        "the array table must decode every element the scalar table does: {body}"
    );
}

/// Postgres' internal one-byte `"char"` is a *different* type from `char(n)`,
/// and it now says so.
///
/// The two display names differ only by a pair of quote characters, and the
/// string arm used to list both — but sqlx's `str` compatibility accepts
/// `bpchar` and not `"char"`, so this column failed inside the driver and
/// surfaced a raw type-mismatch instead of the module's named, actionable
/// error. It belongs in the catch-all, which is what this asserts.
///
/// Requires Docker; run with
/// `cargo test --test integration -- --ignored db_column_types`.
#[tokio::test]
#[ignore]
async fn postgres_internal_char_names_the_column_and_the_remedy() {
    use crate::common::backends::Backend;

    let app = common::test_app().await;
    let h = common::backends::start(Backend::Postgres, "pg-ichar").await;
    common::create_connector(&app, h.connector_json()).await;

    common::create_and_activate_channel(
        &app,
        "pg-ichar-ch",
        round_trip_workflow(
            "PgInternalChar",
            "pg-ichar",
            &[
                "DROP TABLE IF EXISTS pg_ichar",
                "CREATE TABLE pg_ichar (id int PRIMARY KEY, kind \"char\" NOT NULL)",
                "INSERT INTO pg_ichar VALUES (1, 'r')",
            ],
            "SELECT * FROM pg_ichar",
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/pg-ichar-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .expect("request");
    let body = common::body_json(resp).await;
    let rendered = body.to_string();

    assert!(rendered.contains("kind"), "name the column: {body}");
    assert!(rendered.contains("CHAR"), "name the SQL type: {body}");
    assert!(
        rendered.contains("::text"),
        "say what to do about it: {body}"
    );
    // The assertion that separates the two paths. Both produce a `DecodeError`
    // — `DecodeError::message` appends the `::text` remedy either way — so the
    // three checks above pass even with `"CHAR"` wrongly listed in the string
    // arm. What changes is the *reason*: the catch-all says the type has no
    // JSON form here, where the string arm surfaced sqlx's raw
    // `mismatched types; Rust type ... is not compatible with SQL type` from
    // one layer down, which names Rust types the author never wrote.
    assert!(
        rendered.contains("no JSON representation"),
        "the reason must be this module's, not a driver type mismatch: {body}"
    );
    assert!(
        !rendered.contains("mismatched types"),
        "a driver mismatch means the type is in the wrong arm: {body}"
    );
}

/// A domain column decodes as the type it wraps.
///
/// Worth pinning because the mechanism is not the one the code suggests.
/// `pg_column` has a `PgTypeKind::Domain` branch that unwraps the domain and
/// re-dispatches on the base type's name, and reading the module it is natural
/// to conclude that is what makes domains work. It is not: **PostgreSQL reports
/// the base type's OID in the row description**, so `CREATE DOMAIN email AS
/// text` arrives already spelled `TEXT` and the branch is never taken. (Made
/// certain by replacing its body with an unconditional error; this test still
/// passed.)
///
/// The distinction matters because the branch could not carry the feature if it
/// ever were taken — `try_get` re-reads the value's own type info and compares
/// by OID, and no Rust type can claim an OID the database invented, so `String`
/// would refuse a domain over `text`. This test is therefore about the
/// behaviour, not the branch: if a future sqlx resolves domains differently,
/// this fails and says so.
///
/// Requires Docker; run with
/// `cargo test --test integration -- --ignored db_column_types`.
#[tokio::test]
#[ignore]
async fn postgres_domain_columns_decode_as_the_type_they_wrap() {
    use crate::common::backends::Backend;

    let app = common::test_app().await;
    let h = common::backends::start(Backend::Postgres, "pg-domain").await;
    common::create_connector(&app, h.connector_json()).await;

    common::create_and_activate_channel(
        &app,
        "pg-domain-ch",
        round_trip_workflow(
            "PgDomain",
            "pg-domain",
            &[
                "DROP TABLE IF EXISTS pg_domain",
                "DROP DOMAIN IF EXISTS email",
                "DROP DOMAIN IF EXISTS positive_int",
                "DROP DOMAIN IF EXISTS money_amount",
                "CREATE DOMAIN email AS text CHECK (VALUE LIKE '%@%')",
                "CREATE DOMAIN positive_int AS integer CHECK (VALUE > 0)",
                "CREATE DOMAIN money_amount AS numeric(12,2)",
                "CREATE TABLE pg_domain (
                   id      int PRIMARY KEY,
                   contact email NOT NULL,
                   qty     positive_int NOT NULL,
                   total   money_amount NOT NULL
                 )",
                "INSERT INTO pg_domain VALUES (1, 'a@b.test', 3, 1234.56)",
            ],
            "SELECT * FROM pg_domain",
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/pg-domain-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert!(
        body["errors"].as_array().is_some_and(|a| a.is_empty()),
        "a domain must decode as its base type: {body}"
    );
    let row = &body["data"]["rows"][0];

    assert_eq!(row["contact"], "a@b.test", "domain over text: {body}");
    assert_eq!(row["qty"], json!(3), "domain over integer: {body}");
    // Every rule the base type gets, `numeric_as` included, applies through the
    // domain — because by the time the row is read there is no domain left.
    assert_eq!(row["total"], json!(1234.56), "domain over numeric: {body}");
}
