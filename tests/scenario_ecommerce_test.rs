mod common;

use axum::http::StatusCode;
use common::{body_json, json_request};
use serde_json::json;
use tower::ServiceExt;

// ============================================================
// Test 1: Full e-commerce order processing pipeline
// ============================================================

/// Simulates a realistic order processing flow: parse the incoming order,
/// create the orders table, compute the order total, call an external payment
/// API, persist the order to the database, and cache the order status.
#[tokio::test]
async fn test_order_processing_pipeline() {
    // Start a mock payment server
    let mock_app = axum::Router::new().route(
        "/charge",
        axum::routing::post(|| async {
            axum::Json(json!({"payment_id": "pay-123", "status": "success"}))
        }),
    );
    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = mock_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(mock_listener, mock_app).await.unwrap();
    });

    let app = common::test_app().await;

    // Create HTTP connector for the mock payment API
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "payment-api",
                "name": "payment-api",
                "connector_type": "http",
                "config": {
                    "type": "http",
                    "url": format!("http://{}", mock_addr),
                    "retry": { "max_retries": 0, "retry_delay_ms": 10 },
                    "allow_private_urls": true
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Create SQLite DB connector for order storage
    common::create_connector(
        &app,
        common::db_connector_sqlite(
            "orders-db",
            "sqlite:file:test_order_processing?mode=memory&cache=shared",
        ),
    )
    .await;

    // Create in-memory cache connector for order status caching
    common::create_connector(&app, common::cache_connector_memory("order-cache")).await;

    // Build the 6-task order processing workflow
    let workflow = common::workflow_with_tasks(
        "Order Processing Pipeline",
        json!([
            {
                "id": "t1",
                "name": "Parse order payload",
                "function": {
                    "name": "parse_json",
                    "input": { "source": "payload", "target": "input" }
                }
            },
            {
                "id": "t2",
                "name": "Create orders table",
                "function": {
                    "name": "db_write",
                    "input": {
                        "connector": "orders-db",
                        "query": "CREATE TABLE IF NOT EXISTS orders (id TEXT PRIMARY KEY, total REAL, payment_id TEXT, status TEXT)",
                        "output": "data.create_result"
                    }
                }
            },
            {
                "id": "t3",
                "name": "Compute order total",
                "function": {
                    "name": "map",
                    "input": {
                        "mappings": [{
                            "path": "data.total",
                            "logic": { "*": [{ "var": "data.input.quantity" }, { "var": "data.input.price" }] }
                        }]
                    }
                }
            },
            {
                "id": "t4",
                "name": "Charge payment",
                "function": {
                    "name": "http_call",
                    "input": {
                        "connector": "payment-api",
                        "method": "POST",
                        "path": "/charge",
                        "body": { "amount": 89.97, "currency": "USD" },
                        "response_path": "data.payment",
                        "timeout_ms": 5000
                    }
                }
            },
            {
                "id": "t5",
                "name": "Insert order record",
                "function": {
                    "name": "db_write",
                    "input": {
                        "connector": "orders-db",
                        "query": "INSERT INTO orders (id, total, payment_id, status) VALUES ('ord-001', 89.97, 'pay-123', 'confirmed')",
                        "output": "data.insert_result"
                    }
                }
            },
            {
                "id": "t6",
                "name": "Cache order status",
                "function": {
                    "name": "cache_write",
                    "input": {
                        "connector": "order-cache",
                        "key": "order:ord-001",
                        "value": "confirmed",
                        "ttl_secs": 3600
                    }
                }
            }
        ]),
    );

    common::create_and_activate_channel(&app, "process-order", workflow).await;

    // Send an order request
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/process-order",
            Some(json!({"data": {"order_id": "ord-001", "quantity": 3, "price": 29.99}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");

    // Verify payment response was captured
    assert_eq!(
        body["data"]["payment"]["status"], "success",
        "Expected payment status 'success', got {:?}",
        body["data"]["payment"]["status"]
    );
    assert_eq!(
        body["data"]["payment"]["payment_id"], "pay-123",
        "Expected payment_id 'pay-123', got {:?}",
        body["data"]["payment"]["payment_id"]
    );

    // Verify the computed total is approximately 89.97 (3 * 29.99)
    let total = body["data"]["total"]
        .as_f64()
        .expect("data.total should be a number");
    assert!(
        (total - 89.97).abs() < 0.01,
        "Expected data.total ~89.97, got {}",
        total
    );

    // Verify the DB insert reported 1 row affected
    assert_eq!(
        body["data"]["insert_result"]["rows_affected"], 1,
        "Expected 1 row inserted, got {:?}",
        body["data"]["insert_result"]["rows_affected"]
    );
}

// ============================================================
// Test 2: Input validation rejects invalid orders
// ============================================================

/// Verifies that channel-level validation_logic enforces business rules at the
/// boundary: order_id must be present and quantity must be greater than zero.
#[tokio::test]
async fn test_order_validation_rejects_invalid() {
    let app = common::test_app().await;

    let workflow = common::workflow_with_tasks(
        "Validated Order Workflow",
        json!([
            {
                "id": "t1",
                "name": "Parse payload",
                "function": {
                    "name": "parse_json",
                    "input": { "source": "payload", "target": "input" }
                }
            },
            {
                "id": "t2",
                "name": "Log order",
                "function": {
                    "name": "log",
                    "input": { "message": "Order received" }
                }
            }
        ]),
    );

    let channel_config = json!({
        "validation_logic": {
            "and": [
                { "!!": [{ "var": "data.order_id" }] },
                { ">": [{ "var": "data.quantity" }, 0] }
            ]
        }
    });

    common::create_and_activate_channel_with_config(
        &app,
        "validate-order",
        workflow,
        channel_config,
    )
    .await;

    // Valid input: order_id present and quantity > 0
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/validate-order",
            Some(json!({"data": {"order_id": "o-1", "quantity": 5}})),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "Valid order should be accepted"
    );

    // Invalid input: missing order_id
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/validate-order",
            Some(json!({"data": {"quantity": 5}})),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "Missing order_id should be rejected"
    );
    let body = body_json(resp).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("validation failed"),
        "Expected validation failure message, got {:?}",
        body["error"]["message"]
    );

    // Invalid input: quantity is 0
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/validate-order",
            Some(json!({"data": {"order_id": "o-1", "quantity": 0}})),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "Zero quantity should be rejected"
    );
    let body = body_json(resp).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("validation failed"),
        "Expected validation failure message, got {:?}",
        body["error"]["message"]
    );
}

// ============================================================
// Test 3: Write order then read it back via a separate channel
// ============================================================

/// Simulates a two-channel scenario: one channel writes an order to the
/// database, and a second channel reads it back. Both channels share the
/// same DB connector so they operate on the same SQLite in-memory database.
#[tokio::test]
async fn test_order_lookup_via_db_read() {
    let app = common::test_app().await;

    // Create a shared SQLite DB connector used by both channels
    common::create_connector(
        &app,
        common::db_connector_sqlite(
            "lookup-db",
            "sqlite:file:test_order_lookup?mode=memory&cache=shared",
        ),
    )
    .await;

    // Channel 1: order-writer — creates the table and inserts a row
    let writer_workflow = common::workflow_with_tasks(
        "Order Writer Workflow",
        json!([
            {
                "id": "t1",
                "name": "Create orders table",
                "function": {
                    "name": "db_write",
                    "input": {
                        "connector": "lookup-db",
                        "query": "CREATE TABLE IF NOT EXISTS orders (id TEXT PRIMARY KEY, customer TEXT, total REAL, status TEXT)",
                        "output": "data.create_result"
                    }
                }
            },
            {
                "id": "t2",
                "name": "Insert order",
                "function": {
                    "name": "db_write",
                    "input": {
                        "connector": "lookup-db",
                        "query": "INSERT INTO orders (id, customer, total, status) VALUES ('ord-001', 'Alice', 149.95, 'confirmed')",
                        "output": "data.insert_result"
                    }
                }
            }
        ]),
    );

    common::create_and_activate_channel(&app, "order-writer", writer_workflow).await;

    // Channel 2: order-reader — reads the order back by fixed ID
    let reader_workflow = common::workflow_with_tasks(
        "Order Reader Workflow",
        json!([
            {
                "id": "t1",
                "name": "Read order",
                "function": {
                    "name": "db_read",
                    "input": {
                        "connector": "lookup-db",
                        "query": "SELECT id, customer, total, status FROM orders WHERE id = 'ord-001'",
                        "output": "data.order"
                    }
                }
            }
        ]),
    );

    common::create_and_activate_channel(&app, "order-reader", reader_workflow).await;

    // Step 1: Write the order
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/order-writer",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(
        body["data"]["insert_result"]["rows_affected"], 1,
        "Expected 1 row inserted"
    );

    // Step 2: Read the order back via a separate channel
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/order-reader",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");

    // Step 3: Verify the read data matches what was inserted
    let rows = body["data"]["order"]
        .as_array()
        .expect("data.order should be an array");
    assert_eq!(rows.len(), 1, "Expected exactly 1 order row");
    assert_eq!(rows[0]["id"], "ord-001");
    assert_eq!(rows[0]["customer"], "Alice");
    assert_eq!(rows[0]["status"], "confirmed");

    let total = rows[0]["total"].as_f64().expect("total should be a number");
    assert!(
        (total - 149.95).abs() < 0.01,
        "Expected total ~149.95, got {}",
        total
    );
}
