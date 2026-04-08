mod common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// MySQL integration tests
//
// These tests require a running MySQL instance on localhost:3306.
// Start it with: docker compose -f docker-compose.test.yml up -d mysql
// Run with:      cargo test --test mysql_test -- --ignored
// ---------------------------------------------------------------------------

/// Helper: create a MySQL DB connector via the admin API.
fn mysql_connector(name: &str) -> serde_json::Value {
    json!({
        "id": name,
        "name": name,
        "connector_type": "db",
        "config": {
            "type": "db",
            "connection_string": "mysql://root:test@localhost:3306/orion_test",
            "driver": "mysql",
            "max_connections": 2,
            "query_timeout_ms": 5000
        }
    })
}

/// Create a table, insert a row, and read it back via db_read.
#[tokio::test]
#[ignore]
async fn test_mysql_db_write_and_read() {
    let app = common::test_app().await;

    common::create_connector(&app, mysql_connector("mysql-rw")).await;

    common::create_and_activate_channel(
        &app,
        "mysql-rw-ch",
        common::workflow_with_tasks(
            "MysqlWriteRead",
            json!([
                {
                    "id": "t1",
                    "name": "Drop table if exists",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "mysql-rw",
                            "query": "DROP TABLE IF EXISTS mysql_test_items",
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
                            "connector": "mysql-rw",
                            "query": "CREATE TABLE mysql_test_items (id VARCHAR(255) PRIMARY KEY, name VARCHAR(255), quantity INT)",
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
                            "connector": "mysql-rw",
                            "query": "INSERT INTO mysql_test_items (id, name, quantity) VALUES ('item-1', 'Widget', 10)",
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
                            "connector": "mysql-rw",
                            "query": "SELECT id, name, quantity FROM mysql_test_items",
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
            "/api/v1/data/mysql-rw-ch",
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

/// INSERT and SELECT with parameterized queries using MySQL ? placeholders.
#[tokio::test]
#[ignore]
async fn test_mysql_parameterized_queries() {
    let app = common::test_app().await;

    common::create_connector(&app, mysql_connector("mysql-params")).await;

    common::create_and_activate_channel(
        &app,
        "mysql-params-ch",
        common::workflow_with_tasks(
            "MysqlParameterized",
            json!([
                {
                    "id": "t1",
                    "name": "Drop table if exists",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "mysql-params",
                            "query": "DROP TABLE IF EXISTS mysql_param_items",
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
                            "connector": "mysql-params",
                            "query": "CREATE TABLE mysql_param_items (id VARCHAR(255) PRIMARY KEY, name VARCHAR(255), quantity INT)",
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
                            "connector": "mysql-params",
                            "query": "INSERT INTO mysql_param_items (id, name, quantity) VALUES (?, ?, ?)",
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
                            "connector": "mysql-params",
                            "query": "SELECT id, name, quantity FROM mysql_param_items WHERE id = ?",
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
            "/api/v1/data/mysql-params-ch",
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
