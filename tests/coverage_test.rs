mod common;

use axum::http::StatusCode;
use common::{body_json, json_request};
use serde_json::json;
use tower::ServiceExt;

// ============================================================
// Circuit Breaker Admin Endpoints
// ============================================================

#[tokio::test]
async fn test_circuit_breaker_list_endpoint() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/connectors/circuit-breakers",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["enabled"], false);
    assert!(body.get("breakers").is_some());
}

#[tokio::test]
async fn test_circuit_breaker_reset_not_found() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors/circuit-breakers/nonexistent-key",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not found")
    );
}

// ============================================================
// Engine Status
// ============================================================

#[tokio::test]
async fn test_engine_status_with_loaded_rules() {
    let app = common::test_app().await;

    // Create and activate a rule
    common::create_and_activate_rule(
        &app,
        json!({
            "name": "Status Check Rule",
            "channel": "status-ch",
            "condition": true,
            "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
        }),
    )
    .await;

    // Check engine status
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/engine/status", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["rules_count"].as_i64().unwrap() >= 1);
    assert!(body["active_rules"].as_i64().unwrap() >= 1);
    assert!(body.get("channels").is_some());
    assert!(body.get("version").is_some());
    assert!(body.get("uptime_seconds").is_some());
    let channels = body["channels"].as_array().unwrap();
    assert!(channels.iter().any(|c| c == "status-ch"));
}

// ============================================================
// Version History with Pagination
// ============================================================

#[tokio::test]
async fn test_version_list_with_pagination_params() {
    let app = common::test_app().await;

    // Create a rule (draft v1)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "rule_id": "ver-page-test",
                "name": "Version Pagination",
                "channel": "test",
                "condition": true,
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "v1"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Activate v1
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/rules/ver-page-test/status",
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Create v2 draft
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/ver-page-test/versions",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Activate v2
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/rules/ver-page-test/status",
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Create v3 draft
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/ver-page-test/versions",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // List versions with limit=1 offset=0
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/rules/ver-page-test/versions?limit=1&offset=0",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["limit"], 1);
    assert_eq!(body["offset"], 0);
    assert!(body["total"].as_i64().unwrap() >= 2);
    assert_eq!(body["data"].as_array().unwrap().len(), 1);

    // List versions with offset=1
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/rules/ver-page-test/versions?limit=10&offset=1",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["offset"], 1);
    assert!(!body["data"].as_array().unwrap().is_empty());
}

// ============================================================
// Export with Filters
// ============================================================

#[tokio::test]
async fn test_export_rules_with_channel_filter() {
    let app = common::test_app().await;

    // Create rules in different channels (as drafts, export should still work)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "name": "Export Rule A",
                "channel": "export-alpha",
                "condition": true,
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "a"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "name": "Export Rule B",
                "channel": "export-beta",
                "condition": true,
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "b"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Export with channel filter
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/rules/export?channel=export-alpha",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let data = body["data"].as_array().unwrap();
    assert!(!data.is_empty());
    for rule in data {
        assert_eq!(rule["channel"], "export-alpha");
    }
}

// ============================================================
// Connector Update with type + config validation
// ============================================================

#[tokio::test]
async fn test_update_connector_with_type_and_config() {
    let app = common::test_app().await;

    // Create connector
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "type-cfg-conn",
                "name": "type-config-test",
                "connector_type": "http",
                "config": {
                    "type": "http",
                    "url": "https://example.com/api"
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Update with both connector_type and config
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/connectors/type-cfg-conn",
            Some(json!({
                "connector_type": "http",
                "config": {
                    "type": "http",
                    "url": "https://updated.example.com/api"
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["name"], "type-config-test");
}

// Update connector with name only
#[tokio::test]
async fn test_update_connector_name_only() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "name-only-conn",
                "name": "original-name",
                "connector_type": "http",
                "config": {"type": "http", "url": "https://example.com"}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/connectors/name-only-conn",
            Some(json!({"name": "renamed-connector"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["name"], "renamed-connector");
}

// ============================================================
// Get Trace - completed with result
// ============================================================

#[tokio::test]
async fn test_get_completed_trace_with_result() {
    let app = common::test_app().await;

    // Submit async trace
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/test-ch/async",
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    let trace_id = body["trace_id"].as_str().unwrap().to_string();

    // Wait for completion
    for _ in 0..40 {
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        let resp = app
            .clone()
            .oneshot(json_request(
                "GET",
                &format!("/api/v1/data/traces/{}", trace_id),
                None,
            ))
            .await
            .unwrap();
        let body = body_json(resp).await;
        if body["status"] == "completed" {
            assert!(body.get("message").is_some());
            assert!(body.get("started_at").is_some());
            assert!(body.get("completed_at").is_some());
            return;
        }
    }
    panic!("Trace did not complete in time");
}

// ============================================================
// Validate rule - task condition JSONLogic
// ============================================================

#[tokio::test]
async fn test_validate_rule_with_task_condition() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/validate",
            Some(json!({
                "name": "Condition Test",
                "channel": "test",
                "condition": true,
                "tasks": [{
                    "id": "t1",
                    "name": "Task with condition",
                    "condition": {">": [{"var": "data.amount"}, 100]},
                    "function": {"name": "log", "input": {"message": "test"}}
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["valid"], true);
}

#[tokio::test]
async fn test_validate_rule_with_connector_warning() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/validate",
            Some(json!({
                "name": "Connector Warning Test",
                "channel": "test",
                "condition": true,
                "tasks": [{
                    "id": "t1",
                    "name": "HTTP Call",
                    "function": {
                        "name": "http_call",
                        "input": {
                            "connector": "nonexistent-connector",
                            "method": "GET",
                            "path": "/api",
                            "timeout_ms": 5000
                        }
                    }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let warnings = body["warnings"].as_array().unwrap();
    let has_connector_warning = warnings
        .iter()
        .any(|w| w["message"].as_str().unwrap_or("").contains("not found"));
    assert!(has_connector_warning);
}

#[tokio::test]
async fn test_validate_rule_with_empty_channel() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/validate",
            Some(json!({
                "name": "Empty Channel Test",
                "channel": "",
                "condition": true,
                "tasks": [{
                    "id": "t1",
                    "name": "Log",
                    "function": {"name": "log", "input": {"message": "test"}}
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["valid"], false);
    let errors = body["errors"].as_array().unwrap();
    let has_channel_error = errors
        .iter()
        .any(|e| e["field"].as_str().unwrap_or("") == "channel");
    assert!(has_channel_error);
}

// ============================================================
// HTTP Call End-to-End with Mock Server
// ============================================================

#[tokio::test]
async fn test_http_call_end_to_end() {
    // Start mock HTTP server
    let mock_app = axum::Router::new().route(
        "/api/users",
        axum::routing::post(|| async {
            axum::Json(json!({"user_id": "123", "status": "created"}))
        }),
    );
    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = mock_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(mock_listener, mock_app).await.unwrap();
    });

    let app = common::test_app().await;

    // Create connector pointing to mock server
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "mock-http-api",
                "name": "mock-http-api",
                "connector_type": "http",
                "config": {
                    "type": "http",
                    "url": format!("http://{}", mock_addr),
                    "retry": {"max_retries": 0, "retry_delay_ms": 10}
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Create and activate rule with http_call task
    common::create_and_activate_rule(
        &app,
        json!({
            "name": "HTTP Call Integration",
            "channel": "http-call-ch",
            "condition": true,
            "tasks": [{
                "id": "call-api",
                "name": "Call Mock API",
                "function": {
                    "name": "http_call",
                    "input": {
                        "connector": "mock-http-api",
                        "method": "POST",
                        "path": "/api/users",
                        "body": {"test": true},
                        "response_path": "api_response",
                        "timeout_ms": 5000
                    }
                }
            }]
        }),
    )
    .await;

    // Process data through the channel
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/http-call-ch",
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
}

// ============================================================
// Enrich End-to-End with Mock Server
// ============================================================

#[tokio::test]
async fn test_enrich_end_to_end() {
    // Start mock HTTP server for enrichment
    let mock_app = axum::Router::new().route(
        "/api/details",
        axum::routing::get(|| async {
            axum::Json(json!({"detail": "enriched_data", "score": 95}))
        }),
    );
    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = mock_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(mock_listener, mock_app).await.unwrap();
    });

    let app = common::test_app().await;

    // Create connector pointing to mock server
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "mock-enrich-api",
                "name": "mock-enrich-api",
                "connector_type": "http",
                "config": {
                    "type": "http",
                    "url": format!("http://{}", mock_addr),
                    "retry": {"max_retries": 0, "retry_delay_ms": 10}
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Create and activate rule with enrich task
    common::create_and_activate_rule(
        &app,
        json!({
            "name": "Enrich Integration",
            "channel": "enrich-ch",
            "condition": true,
            "tasks": [{
                "id": "enrich-data",
                "name": "Enrich from API",
                "function": {
                    "name": "enrich",
                    "input": {
                        "connector": "mock-enrich-api",
                        "method": "GET",
                        "path": "/api/details",
                        "merge_path": "enrichment",
                        "on_error": "skip",
                        "timeout_ms": 5000
                    }
                }
            }]
        }),
    )
    .await;

    // Process data
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/enrich-ch",
            Some(json!({"data": {"item_id": "abc"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
}

// ============================================================
// Test rule with test endpoint (dry-run with data)
// ============================================================

#[tokio::test]
async fn test_rule_test_with_metadata() {
    let app = common::test_app().await;

    // Create a rule (draft is fine for test endpoint)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "rule_id": "test-dry-run-meta",
                "name": "Dry Run Rule",
                "channel": "dry-run",
                "condition": true,
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "dry run"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Test the rule with metadata
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/test-dry-run-meta/test",
            Some(json!({
                "data": {"key": "value"},
                "metadata": {"source": "test-suite"}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body.get("matched").is_some());
    assert!(body.get("trace").is_some());
    assert!(body.get("output").is_some());
    assert!(body.get("errors").is_some());
}

// Test with non-object data payload
#[tokio::test]
async fn test_rule_test_with_non_object_data() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "rule_id": "test-non-obj",
                "name": "Non Object Data",
                "channel": "test",
                "condition": true,
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Test with a non-object data (string)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/test-non-obj/test",
            Some(json!({
                "data": "just a string"
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ============================================================
// Import rules - all success case
// ============================================================

#[tokio::test]
async fn test_import_all_success() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/import",
            Some(json!([
                {
                    "rule_id": "import-1",
                    "name": "Import Rule 1",
                    "channel": "import-ch",
                    "condition": true,
                    "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "imported"}}}]
                },
                {
                    "rule_id": "import-2",
                    "name": "Import Rule 2",
                    "channel": "import-ch",
                    "condition": true,
                    "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "imported"}}}]
                }
            ])),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["imported"], 2);
    assert_eq!(body["failed"], 0);
    assert!(body["errors"].as_array().unwrap().is_empty());
}

// ============================================================
// Data sync processing with metadata
// ============================================================

#[tokio::test]
async fn test_sync_processing_with_metadata() {
    let app = common::test_app().await;

    // Create and activate a rule
    common::create_and_activate_rule(
        &app,
        json!({
            "name": "Metadata Rule",
            "channel": "meta-ch",
            "condition": true,
            "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "meta test"}}}]
        }),
    )
    .await;

    // Process with metadata
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/meta-ch",
            Some(json!({
                "data": {"order_id": "ORD-001"},
                "metadata": {"source": "api", "trace_id": "abc123"}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert!(body.get("id").is_some());
    assert!(body.get("data").is_some());
    assert!(body.get("errors").is_some());
}

// ============================================================
// Connector list with pagination
// ============================================================

#[tokio::test]
async fn test_connector_list_with_pagination() {
    let app = common::test_app().await;

    // Create two connectors
    for i in 0..2 {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/admin/connectors",
                Some(json!({
                    "id": format!("page-conn-{}", i),
                    "name": format!("page-conn-{}", i),
                    "connector_type": "http",
                    "config": {"type": "http", "url": "https://example.com"}
                })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    // List with limit=1
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/connectors?limit=1&offset=0",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["limit"], 1);
    assert_eq!(body["offset"], 0);
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert!(body["total"].as_i64().unwrap() >= 2);
}

// ============================================================
// Create rule with description and custom ID
// ============================================================

#[tokio::test]
async fn test_create_rule_with_description_and_tags() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "rule_id": "desc-rule",
                "name": "Described Rule",
                "description": "This is a test rule with a description",
                "channel": "desc-ch",
                "priority": 5,
                "condition": {"==": [1, 1]},
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}],
                "tags": ["production", "important"],
                "continue_on_error": true
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["rule_id"], "desc-rule");
    assert_eq!(
        body["data"]["description"],
        "This is a test rule with a description"
    );
    assert_eq!(body["data"]["priority"], 5);
    assert!(body["data"]["continue_on_error"].as_bool().unwrap());
}

// ============================================================
// Update rule with description
// ============================================================

#[tokio::test]
async fn test_update_rule_with_description() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "rule_id": "upd-desc-rule",
                "name": "Update Desc Rule",
                "channel": "test",
                "condition": true,
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Update the draft with description and channel
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/rules/upd-desc-rule",
            Some(json!({
                "description": "Updated description",
                "channel": "updated-ch"
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ============================================================
// Create connector with custom ID
// ============================================================

#[tokio::test]
async fn test_create_connector_with_custom_id() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "my-custom-conn-id",
                "name": "custom-id-test",
                "connector_type": "http",
                "config": {"type": "http", "url": "https://example.com"}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["id"], "my-custom-conn-id");
}

// ============================================================
// Connector enable/disable via update
// ============================================================

#[tokio::test]
async fn test_update_connector_enabled_flag() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "enable-conn",
                "name": "enable-test",
                "connector_type": "http",
                "config": {"type": "http", "url": "https://example.com"}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Disable it
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/connectors/enable-conn",
            Some(json!({"enabled": false})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["enabled"], false);
}
