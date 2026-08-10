//! Column-type fidelity for `db_read` (proposal F9).
//!
//! `rows_to_json` used to probe `String` → `i64` → `f64` → `bool` and fall
//! through to `Value::Null`, so any value it did not recognise came back as
//! null and was indistinguishable from a genuine SQL NULL. It now dispatches on
//! the value's own `AnyTypeInfoKind` and errors rather than inventing a null.
//!
//! `data_query` and `data_write`'s `returning` share `rows_to_json`, so these
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

/// A column type the `sqlx` `Any` driver cannot represent — `timestamptz`,
/// `uuid`, `jsonb`, `numeric`, arrays, enums — must surface as an error. The
/// driver rejects these while building the row, before `rows_to_json` sees
/// them, so the failure mode is a loud one either way; this test pins that down
/// so the "query succeeded, every column is null" reading can never return.
///
/// Requires Docker; run with
/// `cargo test --test integration -- --ignored db_column_types`.
#[tokio::test]
#[ignore]
async fn postgres_unsupported_column_type_errors_rather_than_returning_nulls() {
    use crate::common::backends::Backend;

    let app = common::test_app().await;
    let h = common::backends::start(Backend::Postgres, "pg-unsupported").await;
    common::create_connector(&app, h.connector_json()).await;

    common::create_and_activate_channel(
        &app,
        "pg-unsupported-ch",
        common::workflow_with_tasks(
            "PgUnsupportedKind",
            json!([
                {
                    "id": "t1",
                    "name": "Drop",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "pg-unsupported",
                            "query": "DROP TABLE IF EXISTS pg_stamped",
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
                            "connector": "pg-unsupported",
                            "query": "CREATE TABLE pg_stamped (id INTEGER PRIMARY KEY, seen TIMESTAMPTZ NOT NULL DEFAULT now())",
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
                            "connector": "pg-unsupported",
                            "query": "INSERT INTO pg_stamped (id) VALUES (1)",
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
                            "connector": "pg-unsupported",
                            "query": "SELECT id, seen FROM pg_stamped WHERE id = 1",
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
            "/api/v1/data/pg-unsupported-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .expect("request");

    let status = resp.status();
    let body = common::body_json(resp).await;

    let reported_error = body.get("error").is_some_and(|e| e.get("code").is_some())
        || body
            .get("errors")
            .and_then(|e| e.as_array())
            .is_some_and(|a| !a.is_empty())
        || status.is_server_error();
    assert!(
        reported_error,
        "an unrepresentable column type must be reported, not silently nulled: status={status} body={body}"
    );
    assert!(
        body["data"]["rows"].is_null()
            || body["data"]["rows"].as_array().is_none_or(|a| a.is_empty()),
        "no row should have been produced: {body}"
    );
}
