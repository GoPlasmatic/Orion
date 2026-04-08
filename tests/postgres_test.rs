mod common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// PostgreSQL integration tests
//
// These tests require a running PostgreSQL instance on localhost:5432.
// Start it with: docker compose -f docker-compose.test.yml up -d postgres
// Run with:      cargo test --test postgres_test -- --ignored
// ---------------------------------------------------------------------------

/// Helper: create a Postgres DB connector via the admin API.
fn pg_connector(name: &str) -> serde_json::Value {
    json!({
        "id": name,
        "name": name,
        "connector_type": "db",
        "config": {
            "type": "db",
            "connection_string": "postgres://postgres:test@localhost:5432/orion_test",
            "driver": "postgres",
            "max_connections": 2,
            "query_timeout_ms": 5000
        }
    })
}

/// Create a table, insert a row, and read it back via db_read.
#[tokio::test]
#[ignore]
async fn test_postgres_db_write_and_read() {
    let app = common::test_app().await;

    common::create_connector(&app, pg_connector("pg-rw")).await;

    common::create_and_activate_channel(
        &app,
        "pg-rw-ch",
        common::workflow_with_tasks(
            "PgWriteRead",
            json!([
                {
                    "id": "t1",
                    "name": "Drop table if exists",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "pg-rw",
                            "query": "DROP TABLE IF EXISTS pg_test_items",
                            "output": "data.drop_result"
                        }
                    }
                },
                {
                    "id": "t2",
                    "name": "Create table",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "pg-rw",
                            "query": "CREATE TABLE pg_test_items (id TEXT PRIMARY KEY, name TEXT, quantity INTEGER)",
                            "output": "data.create_result"
                        }
                    }
                },
                {
                    "id": "t3",
                    "name": "Insert row",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "pg-rw",
                            "query": "INSERT INTO pg_test_items (id, name, quantity) VALUES ('item-1', 'Widget', 10)",
                            "output": "data.insert_result"
                        }
                    }
                },
                {
                    "id": "t4",
                    "name": "Select rows",
                    "function": {
                        "name": "db_read",
                        "input": {
                            "connector": "pg-rw",
                            "query": "SELECT id, name, quantity FROM pg_test_items",
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
            "/api/v1/data/pg-rw-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["data"]["insert_result"]["rows_affected"], 1);

    let rows = body["data"]["rows"]
        .as_array()
        .expect("rows should be an array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "item-1");
    assert_eq!(rows[0]["name"], "Widget");
    assert_eq!(rows[0]["quantity"], 10);
}

/// INSERT and SELECT with parameterized queries using Postgres $1, $2 syntax.
#[tokio::test]
#[ignore]
async fn test_postgres_parameterized_queries() {
    let app = common::test_app().await;

    common::create_connector(&app, pg_connector("pg-params")).await;

    common::create_and_activate_channel(
        &app,
        "pg-params-ch",
        common::workflow_with_tasks(
            "PgParameterized",
            json!([
                {
                    "id": "t1",
                    "name": "Drop table if exists",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "pg-params",
                            "query": "DROP TABLE IF EXISTS pg_param_items",
                            "output": "data.drop_result"
                        }
                    }
                },
                {
                    "id": "t2",
                    "name": "Create table",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "pg-params",
                            "query": "CREATE TABLE pg_param_items (id TEXT PRIMARY KEY, name TEXT, quantity INTEGER)",
                            "output": "data.create_result"
                        }
                    }
                },
                {
                    "id": "t3",
                    "name": "Insert with params",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "pg-params",
                            "query": "INSERT INTO pg_param_items (id, name, quantity) VALUES ($1, $2, $3)",
                            "params": ["gadget-42", "Gadget", 42],
                            "output": "data.insert_result"
                        }
                    }
                },
                {
                    "id": "t4",
                    "name": "Select with WHERE",
                    "function": {
                        "name": "db_read",
                        "input": {
                            "connector": "pg-params",
                            "query": "SELECT id, name, quantity FROM pg_param_items WHERE id = $1",
                            "params": ["gadget-42"],
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
            "/api/v1/data/pg-params-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["data"]["insert_result"]["rows_affected"], 1);

    let rows = body["data"]["rows"]
        .as_array()
        .expect("rows should be an array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "gadget-42");
    assert_eq!(rows[0]["name"], "Gadget");
    assert_eq!(rows[0]["quantity"], 42);
}
