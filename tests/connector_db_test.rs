mod common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

/// db_write with CREATE TABLE should succeed and return rows_affected.
#[tokio::test]
async fn test_db_write_create_table() {
    let app = common::test_app().await;

    common::create_connector(
        &app,
        common::db_connector_sqlite(
            "db-create-table",
            "sqlite:file:test_db_write_create_table?mode=memory&cache=shared",
        ),
    )
    .await;

    common::create_and_activate_channel(
        &app,
        "ch-create-table",
        common::workflow_with_tasks(
            "Create Table Workflow",
            json!([
                {
                    "id": "t1",
                    "name": "Create table",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "db-create-table",
                            "query": "CREATE TABLE IF NOT EXISTS test_items (id TEXT PRIMARY KEY, name TEXT, quantity INTEGER)",
                            "output": "data.create_result"
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
            "/api/v1/data/ch-create-table",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");
    // CREATE TABLE returns rows_affected (typically 0)
    assert!(body["data"]["create_result"]["rows_affected"].is_number());
}

/// Two-task workflow: CREATE TABLE then INSERT. Verify rows_affected is 1.
#[tokio::test]
async fn test_db_write_insert_row() {
    let app = common::test_app().await;

    common::create_connector(
        &app,
        common::db_connector_sqlite(
            "db-insert",
            "sqlite:file:test_db_write_insert_row?mode=memory&cache=shared",
        ),
    )
    .await;

    common::create_and_activate_channel(
        &app,
        "ch-insert-row",
        common::workflow_with_tasks(
            "Insert Row Workflow",
            json!([
                {
                    "id": "t1",
                    "name": "Create table",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "db-insert",
                            "query": "CREATE TABLE IF NOT EXISTS test_items (id TEXT PRIMARY KEY, name TEXT, quantity INTEGER)",
                            "output": "data.create_result"
                        }
                    }
                },
                {
                    "id": "t2",
                    "name": "Insert row",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "db-insert",
                            "query": "INSERT INTO test_items (id, name, quantity) VALUES ('item-1', 'Widget', 10)",
                            "output": "data.insert_result"
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
            "/api/v1/data/ch-insert-row",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["data"]["insert_result"]["rows_affected"], 1);
}

/// Three-task workflow: CREATE TABLE -> INSERT -> SELECT. Verify the read
/// returns an array containing the inserted row.
#[tokio::test]
async fn test_db_read_select_rows() {
    let app = common::test_app().await;

    common::create_connector(
        &app,
        common::db_connector_sqlite(
            "db-select",
            "sqlite:file:test_db_read_select_rows?mode=memory&cache=shared",
        ),
    )
    .await;

    common::create_and_activate_channel(
        &app,
        "ch-select-rows",
        common::workflow_with_tasks(
            "Select Rows Workflow",
            json!([
                {
                    "id": "t1",
                    "name": "Create table",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "db-select",
                            "query": "CREATE TABLE IF NOT EXISTS test_items (id TEXT PRIMARY KEY, name TEXT, quantity INTEGER)",
                            "output": "data.create_result"
                        }
                    }
                },
                {
                    "id": "t2",
                    "name": "Insert row",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "db-select",
                            "query": "INSERT INTO test_items (id, name, quantity) VALUES ('item-1', 'Widget', 10)",
                            "output": "data.insert_result"
                        }
                    }
                },
                {
                    "id": "t3",
                    "name": "Select rows",
                    "function": {
                        "name": "db_read",
                        "input": {
                            "connector": "db-select",
                            "query": "SELECT id, name, quantity FROM test_items WHERE id = ?",
                            "params": ["item-1"],
                            "output": "data.read_result"
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
            "/api/v1/data/ch-select-rows",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");

    let rows = body["data"]["read_result"]
        .as_array()
        .expect("read_result should be an array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "item-1");
    assert_eq!(rows[0]["name"], "Widget");
    assert_eq!(rows[0]["quantity"], 10);
}

/// Two-task workflow: CREATE TABLE -> SELECT on empty table. Verify the read
/// returns an empty array.
#[tokio::test]
async fn test_db_read_empty_table() {
    let app = common::test_app().await;

    common::create_connector(
        &app,
        common::db_connector_sqlite(
            "db-empty",
            "sqlite:file:test_db_read_empty_table?mode=memory&cache=shared",
        ),
    )
    .await;

    common::create_and_activate_channel(
        &app,
        "ch-empty-table",
        common::workflow_with_tasks(
            "Empty Table Workflow",
            json!([
                {
                    "id": "t1",
                    "name": "Create table",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "db-empty",
                            "query": "CREATE TABLE IF NOT EXISTS test_items (id TEXT PRIMARY KEY, name TEXT, quantity INTEGER)",
                            "output": "data.create_result"
                        }
                    }
                },
                {
                    "id": "t2",
                    "name": "Select rows",
                    "function": {
                        "name": "db_read",
                        "input": {
                            "connector": "db-empty",
                            "query": "SELECT * FROM test_items",
                            "output": "data.read_result"
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
            "/api/v1/data/ch-empty-table",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");

    let rows = body["data"]["read_result"]
        .as_array()
        .expect("read_result should be an array");
    assert!(rows.is_empty(), "expected empty array, got {:?}", rows);
}

/// INSERT with parameterized query containing a string, an integer, and then
/// verify the data is correctly stored via a follow-up SELECT.
#[tokio::test]
async fn test_db_write_parameterized_query() {
    let app = common::test_app().await;

    common::create_connector(
        &app,
        common::db_connector_sqlite(
            "db-params",
            "sqlite:file:test_db_write_parameterized_query?mode=memory&cache=shared",
        ),
    )
    .await;

    common::create_and_activate_channel(
        &app,
        "ch-params",
        common::workflow_with_tasks(
            "Parameterized Query Workflow",
            json!([
                {
                    "id": "t1",
                    "name": "Create table",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "db-params",
                            "query": "CREATE TABLE IF NOT EXISTS test_items (id TEXT PRIMARY KEY, name TEXT, quantity INTEGER)",
                            "output": "data.create_result"
                        }
                    }
                },
                {
                    "id": "t2",
                    "name": "Insert with params",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "db-params",
                            "query": "INSERT INTO test_items (id, name, quantity) VALUES (?, ?, ?)",
                            "params": ["gadget-99", "Gadget", 42],
                            "output": "data.insert_result"
                        }
                    }
                },
                {
                    "id": "t3",
                    "name": "Read back",
                    "function": {
                        "name": "db_read",
                        "input": {
                            "connector": "db-params",
                            "query": "SELECT id, name, quantity FROM test_items WHERE id = ?",
                            "params": ["gadget-99"],
                            "output": "data.read_result"
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
            "/api/v1/data/ch-params",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");

    // Verify the insert reported 1 row affected
    assert_eq!(body["data"]["insert_result"]["rows_affected"], 1);

    // Verify the read-back matches what was inserted
    let rows = body["data"]["read_result"]
        .as_array()
        .expect("read_result should be an array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "gadget-99");
    assert_eq!(rows[0]["name"], "Gadget");
    assert_eq!(rows[0]["quantity"], 42);
}

/// A workflow with invalid SQL should surface errors in the response.
#[tokio::test]
async fn test_db_invalid_sql_returns_error() {
    let app = common::test_app().await;

    common::create_connector(
        &app,
        common::db_connector_sqlite(
            "db-invalid",
            "sqlite:file:test_db_invalid_sql_returns_error?mode=memory&cache=shared",
        ),
    )
    .await;

    common::create_and_activate_channel(
        &app,
        "ch-invalid-sql",
        common::workflow_with_tasks(
            "Invalid SQL Workflow",
            json!([
                {
                    "id": "t1",
                    "name": "Bad query",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "db-invalid",
                            "query": "SELCT FORM nothing",
                            "output": "data.result"
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
            "/api/v1/data/ch-invalid-sql",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();

    let status = resp.status();
    let body = common::body_json(resp).await;
    // The workflow engine surfaces the SQL error — either as an engine error
    // (HTTP 500 with error.code) or as a task-level error in the errors array.
    let has_error_object = body.get("error").is_some_and(|e| e.get("code").is_some());
    let has_errors_array = body
        .get("errors")
        .and_then(|e| e.as_array())
        .is_some_and(|a| !a.is_empty());
    assert!(
        has_error_object || has_errors_array || status.is_server_error(),
        "expected an error response for invalid SQL, got status={} body={}",
        status,
        body
    );
}
