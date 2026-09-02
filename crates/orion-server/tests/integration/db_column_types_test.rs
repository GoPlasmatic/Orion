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
