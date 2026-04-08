mod common;

use axum::http::StatusCode;
use common::{body_json, json_request};
use serde_json::json;
use tower::ServiceExt;

// ============================================================
// Test 1: parse_json -> map -> log pipeline
// ============================================================

/// Verifies a three-task pipeline: parse_json parses the payload into data.input,
/// map computes data.total = data.input.amount * 1.1, and log emits a message.
/// Sends {"data": {"amount": 100}} and asserts data.total is approximately 110.
#[tokio::test]
async fn test_parse_map_log_pipeline() {
    let app = common::test_app().await;

    let workflow = common::workflow_with_tasks(
        "Parse Map Log Pipeline",
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
                "name": "Compute total",
                "function": {
                    "name": "map",
                    "input": {
                        "mappings": [{
                            "path": "data.total",
                            "logic": { "*": [{ "var": "data.input.amount" }, 1.1] }
                        }]
                    }
                }
            },
            {
                "id": "t3",
                "name": "Log result",
                "function": {
                    "name": "log",
                    "input": { "message": "Pipeline completed" }
                }
            }
        ]),
    );

    common::create_and_activate_channel(&app, "parse-map-log", workflow).await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/parse-map-log",
            Some(json!({"data": {"amount": 100}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");

    // data.total should be approximately 110.0 (100 * 1.1)
    let total = body["data"]["total"]
        .as_f64()
        .expect("data.total should be a number");
    assert!(
        (total - 110.0).abs() < 0.01,
        "Expected data.total ~110.0, got {}",
        total
    );
}

// ============================================================
// Test 2: filter halts pipeline
// ============================================================

/// Verifies that the filter function halts the pipeline when the condition is not met.
/// - priority=3 -> filter rejects (condition: priority > 5), map should NOT run
/// - priority=10 -> filter passes, map sets data.processed = true
#[tokio::test]
async fn test_filter_halts_pipeline() {
    let app = common::test_app().await;

    let workflow = common::workflow_with_tasks(
        "Filter Pipeline",
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
                "name": "Priority gate",
                "function": {
                    "name": "filter",
                    "input": {
                        "condition": { ">": [{ "var": "data.input.priority" }, 5] },
                        "on_reject": "halt"
                    }
                }
            },
            {
                "id": "t3",
                "name": "Mark processed",
                "function": {
                    "name": "map",
                    "input": {
                        "mappings": [{
                            "path": "data.processed",
                            "logic": true
                        }]
                    }
                }
            }
        ]),
    );

    common::create_and_activate_channel(&app, "filter-ch", workflow).await;

    // Case 1: priority=3 -> filter halts, data.processed should be absent
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/filter-ch",
            Some(json!({"data": {"priority": 3}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert!(
        body["data"]["processed"].is_null(),
        "Expected data.processed to be absent when filter halts, got {:?}",
        body["data"]["processed"]
    );

    // Case 2: priority=10 -> filter passes, data.processed should be true
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/filter-ch",
            Some(json!({"data": {"priority": 10}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(
        body["data"]["processed"], true,
        "Expected data.processed == true when filter passes"
    );
}

// ============================================================
// Test 3: multi-task data flow with cache write/read
// ============================================================

/// Verifies data flows across tasks through the cache: cache_write stores a known value
/// under "user:1", and cache_read retrieves it into data.cached_name. Then a map task
/// copies data.cached_name into data.greeting via JSONLogic.
#[tokio::test]
async fn test_multi_task_data_flow() {
    let app = common::test_app().await;

    // Create an in-memory cache connector (config must include "type": "cache" for registry deserialization)
    let connector_id = common::create_connector(
        &app,
        json!({
            "id": "test-cache",
            "name": "test-cache",
            "connector_type": "cache",
            "config": {
                "type": "cache",
                "backend": "memory"
            }
        }),
    )
    .await;
    assert!(!connector_id.is_empty());

    let workflow = common::workflow_with_tasks(
        "Cache Data Flow",
        json!([
            {
                "id": "t1",
                "name": "Write to cache",
                "function": {
                    "name": "cache_write",
                    "input": {
                        "connector": "test-cache",
                        "key": "user:1",
                        "value": "Alice"
                    }
                }
            },
            {
                "id": "t2",
                "name": "Read from cache",
                "function": {
                    "name": "cache_read",
                    "input": {
                        "connector": "test-cache",
                        "key": "user:1",
                        "output": "data.cached_name"
                    }
                }
            },
            {
                "id": "t3",
                "name": "Build greeting from cached name",
                "function": {
                    "name": "map",
                    "input": {
                        "mappings": [{
                            "path": "data.greeting",
                            "logic": { "cat": ["Hello, ", { "var": "data.cached_name" }] }
                        }]
                    }
                }
            }
        ]),
    );

    common::create_and_activate_channel(&app, "cache-flow", workflow).await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/cache-flow",
            Some(json!({"data": {"name": "Alice"}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(
        body["data"]["cached_name"], "Alice",
        "Expected data.cached_name == 'Alice', got {:?}",
        body["data"]["cached_name"]
    );
    assert_eq!(
        body["data"]["greeting"], "Hello, Alice",
        "Expected data.greeting == 'Hello, Alice', got {:?}",
        body["data"]["greeting"]
    );
}

// ============================================================
// Test 4: conditional per-task execution
// ============================================================

/// Verifies that task-level conditions control which tasks run:
/// - Task A (no condition): always sets data.step_a = true
/// - Task B (condition: type == "premium"): sets data.step_b = true only for premium
/// - Task C (no condition): always sets data.step_c = true
#[tokio::test]
async fn test_conditional_per_task_execution() {
    let app = common::test_app().await;

    let workflow = common::workflow_with_tasks(
        "Conditional Tasks",
        json!([
            {
                "id": "t0",
                "name": "Parse payload",
                "function": {
                    "name": "parse_json",
                    "input": { "source": "payload", "target": "input" }
                }
            },
            {
                "id": "t1",
                "name": "Step A - always",
                "function": {
                    "name": "map",
                    "input": {
                        "mappings": [{
                            "path": "data.step_a",
                            "logic": true
                        }]
                    }
                }
            },
            {
                "id": "t2",
                "name": "Step B - premium only",
                "condition": { "==": [{ "var": "data.input.type" }, "premium"] },
                "function": {
                    "name": "map",
                    "input": {
                        "mappings": [{
                            "path": "data.step_b",
                            "logic": true
                        }]
                    }
                }
            },
            {
                "id": "t3",
                "name": "Step C - always",
                "function": {
                    "name": "map",
                    "input": {
                        "mappings": [{
                            "path": "data.step_c",
                            "logic": true
                        }]
                    }
                }
            }
        ]),
    );

    common::create_and_activate_channel(&app, "cond-tasks", workflow).await;

    // Case 1: type = "basic" -> step_a=true, step_b absent, step_c=true
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/cond-tasks",
            Some(json!({"data": {"type": "basic"}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(
        body["data"]["step_a"], true,
        "step_a should be true for basic"
    );
    assert!(
        body["data"]["step_b"].is_null(),
        "step_b should be absent for basic, got {:?}",
        body["data"]["step_b"]
    );
    assert_eq!(
        body["data"]["step_c"], true,
        "step_c should be true for basic"
    );

    // Case 2: type = "premium" -> all three steps true
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/cond-tasks",
            Some(json!({"data": {"type": "premium"}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(
        body["data"]["step_a"], true,
        "step_a should be true for premium"
    );
    assert_eq!(
        body["data"]["step_b"], true,
        "step_b should be true for premium"
    );
    assert_eq!(
        body["data"]["step_c"], true,
        "step_c should be true for premium"
    );
}

// ============================================================
// Test 5: continue_on_error
// ============================================================

/// Verifies that a task with continue_on_error: true allows the pipeline to proceed
/// even when the task fails. Task 1 references a non-existent db connector and fails,
/// but task 2 (log) still runs. The response should have status "ok" with errors recorded.
#[tokio::test]
async fn test_continue_on_error() {
    let app = common::test_app().await;

    let workflow = json!({
        "name": "Continue On Error Pipeline",
        "condition": true,
        "continue_on_error": true,
        "tasks": [
            {
                "id": "t1",
                "name": "Failing DB read",
                "continue_on_error": true,
                "function": {
                    "name": "db_read",
                    "input": {
                        "connector": "ghost-db",
                        "query": "SELECT 1",
                        "output": "data.db_result"
                    }
                }
            },
            {
                "id": "t2",
                "name": "Log success",
                "function": {
                    "name": "log",
                    "input": { "message": "Pipeline continued after error" }
                }
            }
        ]
    });

    common::create_and_activate_channel(&app, "continue-err", workflow).await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/continue-err",
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;

    // Pipeline should still complete with status "ok" because continue_on_error is true
    assert_eq!(body["status"], "ok");

    // There should be errors recorded from the failed db_read task
    let errors = body["errors"]
        .as_array()
        .expect("errors should be an array");
    assert!(
        !errors.is_empty(),
        "Expected at least one error from the failed db_read task"
    );
}

// ============================================================
// Test 6: http_call -> map pipeline with mock server
// ============================================================

/// Verifies an end-to-end pipeline: parse_json extracts the payload, http_call fetches
/// data from a mock server, and map transforms the response. The mock server returns
/// {"user_id": "u-1", "name": "Bob"} and the map extracts data.api_response.name
/// into data.user_name.
#[tokio::test]
async fn test_http_call_then_map_pipeline() {
    // Start a mock HTTP server
    let mock_app = axum::Router::new().route(
        "/api/user",
        axum::routing::post(|| async { axum::Json(json!({"user_id": "u-1", "name": "Bob"})) }),
    );
    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = mock_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(mock_listener, mock_app).await.unwrap();
    });

    let app = common::test_app().await;

    // Create HTTP connector pointing to the mock server
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "mock-api",
                "name": "mock-api",
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

    let workflow = common::workflow_with_tasks(
        "HTTP Call Then Map",
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
                "name": "Call mock API",
                "function": {
                    "name": "http_call",
                    "input": {
                        "connector": "mock-api",
                        "method": "POST",
                        "path": "/api/user",
                        "body": { "request": true },
                        "response_path": "data.api_response",
                        "timeout_ms": 5000
                    }
                }
            },
            {
                "id": "t3",
                "name": "Extract user name",
                "function": {
                    "name": "map",
                    "input": {
                        "mappings": [{
                            "path": "data.user_name",
                            "logic": { "var": "data.api_response.name" }
                        }]
                    }
                }
            }
        ]),
    );

    common::create_and_activate_channel(&app, "http-map-ch", workflow).await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/http-map-ch",
            Some(json!({"data": {"request_id": "r-1"}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");

    // The mock server returned {"user_id": "u-1", "name": "Bob"}
    // http_call stored it at data.api_response, map extracted name into data.user_name
    assert_eq!(
        body["data"]["api_response"]["user_id"], "u-1",
        "Expected api_response.user_id == 'u-1'"
    );
    assert_eq!(
        body["data"]["api_response"]["name"], "Bob",
        "Expected api_response.name == 'Bob'"
    );
    assert_eq!(
        body["data"]["user_name"], "Bob",
        "Expected data.user_name == 'Bob' after map extraction"
    );
}
