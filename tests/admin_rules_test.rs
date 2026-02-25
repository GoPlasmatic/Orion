mod common;

use axum::http::StatusCode;
use common::{body_json, json_request};
use serde_json::{Value, json};
use tower::ServiceExt;

#[tokio::test]
async fn test_rules_crud_lifecycle() {
    let app = common::test_app().await;

    // Create a rule
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "name": "Test Rule",
                "channel": "orders",
                "priority": 10,
                "condition": true,
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    let rule_id = body["data"]["id"].as_str().unwrap().to_string();
    assert_eq!(body["data"]["name"], "Test Rule");
    assert_eq!(body["data"]["channel"], "orders");
    assert_eq!(body["data"]["version"], 1);
    assert_eq!(body["data"]["status"], "active");

    // Get the rule
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/rules/{}", rule_id),
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["name"], "Test Rule");
    assert_eq!(body["version_count"], 1);

    // List rules
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/rules", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert_eq!(body["total"], 1);
    assert_eq!(body["limit"], 50);
    assert_eq!(body["offset"], 0);

    // Update the rule
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/rules/{}", rule_id),
            Some(json!({ "name": "Updated Rule", "priority": 20 })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["name"], "Updated Rule");
    assert_eq!(body["data"]["priority"], 20);
    assert_eq!(body["data"]["version"], 2);

    // Delete the rule
    let resp = app
        .clone()
        .oneshot(json_request(
            "DELETE",
            &format!("/api/v1/admin/rules/{}", rule_id),
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Verify 404
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/rules/{}", rule_id),
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_rule_status_change() {
    let app = common::test_app().await;

    // Create a rule
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "name": "Status Rule",
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();

    let body = body_json(resp).await;
    let rule_id = body["data"]["id"].as_str().unwrap().to_string();

    // Pause the rule
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/rules/{}/status", rule_id),
            Some(json!({ "status": "paused" })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["status"], "paused");

    // Invalid status
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/rules/{}/status", rule_id),
            Some(json!({ "status": "invalid" })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_rule_import_export() {
    let app = common::test_app().await;

    // Import rules
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/import",
            Some(json!([
                {
                    "name": "Import 1",
                    "channel": "ch1",
                    "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"1"}}}]
                },
                {
                    "name": "Import 2",
                    "channel": "ch2",
                    "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"2"}}}]
                }
            ])),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["imported"], 2);
    assert_eq!(body["failed"], 0);

    // Export all
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/rules/export", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"].as_array().unwrap().len(), 2);

    // Export with channel filter
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/rules/export?channel=ch1",
            None,
        ))
        .await
        .unwrap();

    let body = body_json(resp).await;
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn test_rule_test_endpoint() {
    let app = common::test_app().await;

    // Create a rule with a condition
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "name": "Test Dry Run",
                "condition": { ">": [{ "var": "amount" }, 100] },
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"big order"}}}]
            })),
        ))
        .await
        .unwrap();

    let body = body_json(resp).await;
    let rule_id = body["data"]["id"].as_str().unwrap().to_string();

    // Test with matching data
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/admin/rules/{}/test", rule_id),
            Some(json!({ "data": { "amount": 200 } })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body.get("trace").is_some());
    assert!(body.get("output").is_some());
}

#[tokio::test]
async fn test_engine_status_and_reload() {
    let app = common::test_app().await;

    // Engine status
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/engine/status", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["rules_count"], 0);
    assert!(body["uptime_seconds"].as_i64().unwrap() >= 0);

    // Engine reload
    let resp = app
        .clone()
        .oneshot(json_request("POST", "/api/v1/admin/engine/reload", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["reloaded"], true);
}

#[tokio::test]
async fn test_error_malformed_json_body() {
    let app = common::test_app().await;

    // Send non-JSON body to a JSON endpoint
    let resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/v1/admin/rules")
                .header("content-type", "application/json")
                .body(axum::body::Body::from("not json"))
                .unwrap(),
        )
        .await
        .unwrap();

    // Axum returns 422 for JSON parse failures
    assert!(resp.status().is_client_error());
}

#[tokio::test]
async fn test_error_nonexistent_rule() {
    let app = common::test_app().await;

    // Get non-existent rule
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/rules/nonexistent-id",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert!(body["error"]["code"].as_str().is_some());
    assert!(body["error"]["message"].as_str().is_some());
}

#[tokio::test]
async fn test_error_empty_channel_rejected() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/%20",
            Some(json!({ "data": { "key": "value" } })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_error_empty_batch_rejected() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/batch",
            Some(json!({ "messages": [] })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_end_to_end_rule_execution() {
    let app = common::test_app().await;

    // Create a rule with a map transform
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "name": "Transform Rule",
                "channel": "orders",
                "condition": { ">": [{ "var": "data.total" }, 100] },
                "tasks": [{
                    "id": "transform",
                    "name": "Add label",
                    "function": {
                        "name": "map",
                        "input": {
                            "mappings": [{
                                "path": "data.label",
                                "logic": { "cat": ["Order: $", { "var": "data.total" }] }
                            }]
                        }
                    }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Send data that matches the condition
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/orders",
            Some(json!({ "data": { "total": 250 } })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
    // The map transform adds label inside the data namespace
    assert!(
        body.get("data").is_some(),
        "Response should have data field"
    );

    // Send data that does NOT match the condition (total <= 100)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/orders",
            Some(json!({ "data": { "total": 50 } })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn test_rules_pagination() {
    let app = common::test_app().await;

    // Create 3 rules
    for i in 1..=3 {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/admin/rules",
                Some(json!({
                    "name": format!("Rule {i}"),
                    "channel": "orders",
                    "priority": i,
                    "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
                })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    // Page 1: limit=2, offset=0 -> 2 results, total=3
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/rules?limit=2&offset=0",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"].as_array().unwrap().len(), 2);
    assert_eq!(body["total"], 3);
    assert_eq!(body["limit"], 2);
    assert_eq!(body["offset"], 0);

    // Page 2: limit=2, offset=2 -> 1 result, total=3
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/rules?limit=2&offset=2",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert_eq!(body["total"], 3);

    // Limit clamped to 1000
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/rules?limit=5000", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["limit"], 1000);
}

#[tokio::test]
async fn test_rules_pagination_with_filters() {
    let app = common::test_app().await;

    // Create 2 rules in "orders" and 1 in "events"
    for (i, ch) in [(1, "orders"), (2, "orders"), (3, "events")] {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/admin/rules",
                Some(json!({
                    "name": format!("Rule {i}"),
                    "channel": ch,
                    "priority": i,
                    "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
                })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    // Filter by channel=orders with limit=1 -> total should be 2 (filtered count)
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/rules?channel=orders&limit=1",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert_eq!(body["total"], 2);
    assert_eq!(body["limit"], 1);
}

// ============================================================
// Rule Validation Tests
// ============================================================

fn valid_rule_body() -> Value {
    json!({
        "name": "Test Rule",
        "channel": "orders",
        "priority": 10,
        "condition": true,
        "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
    })
}

#[tokio::test]
async fn test_validate_rule_valid() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/validate",
            Some(valid_rule_body()),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["valid"], true);
    assert!(body["errors"].as_array().unwrap().is_empty());
    assert!(body["warnings"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_validate_rule_empty_name() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/validate",
            Some(json!({
                "name": "",
                "channel": "orders",
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["valid"], false);
    let errors = body["errors"].as_array().unwrap();
    assert!(errors.iter().any(|e| e["field"] == "name"));
}

#[tokio::test]
async fn test_validate_rule_empty_tasks() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/validate",
            Some(json!({
                "name": "Test",
                "channel": "orders",
                "tasks": []
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["valid"], false);
    let errors = body["errors"].as_array().unwrap();
    assert!(errors.iter().any(|e| e["field"] == "tasks"));
}

#[tokio::test]
async fn test_validate_rule_tasks_not_array() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/validate",
            Some(json!({
                "name": "Test",
                "channel": "orders",
                "condition": true,
                "tasks": "not_an_array"
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["valid"], false);
    let errors = body["errors"].as_array().unwrap();
    assert!(errors.iter().any(|e| e["field"] == "tasks"));
}

#[tokio::test]
async fn test_validate_rule_duplicate_task_ids() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/validate",
            Some(json!({
                "name": "Test",
                "channel": "orders",
                "tasks": [
                    {"id":"t1","name":"Log","function":{"name":"log","input":{"message":"a"}}},
                    {"id":"t1","name":"Log2","function":{"name":"log","input":{"message":"b"}}}
                ]
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["valid"], false);
    let errors = body["errors"].as_array().unwrap();
    assert!(
        errors
            .iter()
            .any(|e| e["field"] == "tasks" && e["message"].as_str().unwrap().contains("Duplicate"))
    );
}

#[tokio::test]
async fn test_validate_rule_missing_task_fields() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/validate",
            Some(json!({
                "name": "Test",
                "channel": "orders",
                "tasks": [{"some_field": "value"}]
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["valid"], false);
    let errors = body["errors"].as_array().unwrap();
    assert!(errors.iter().any(|e| e["field"] == "tasks[0].id"));
    assert!(
        errors
            .iter()
            .any(|e| e["field"] == "tasks[0].function.name")
    );
}

#[tokio::test]
async fn test_validate_rule_unknown_function() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/validate",
            Some(json!({
                "name": "Test",
                "channel": "orders",
                "tasks": [{"id":"t1","name":"DoStuff","function":{"name":"foobar","input":{}}}]
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["valid"], true);
    let warnings = body["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w["field"] == "tasks[0].function.name"
                && w["message"].as_str().unwrap().contains("foobar"))
    );
}

#[tokio::test]
async fn test_validate_rule_missing_connector() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/validate",
            Some(json!({
                "name": "Test",
                "channel": "orders",
                "tasks": [{
                    "id": "t1",
                    "name": "Call API",
                    "function": {
                        "name": "http_call",
                        "input": { "connector": "nonexistent_api" }
                    }
                }]
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["valid"], true);
    let warnings = body["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w["field"] == "tasks[0].function.input.connector"
                && w["message"].as_str().unwrap().contains("nonexistent_api"))
    );
}

#[tokio::test]
async fn test_validate_no_persistence() {
    let app = common::test_app().await;

    // Validate a rule
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/validate",
            Some(valid_rule_body()),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["valid"], true);

    // List rules — should be empty
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/rules", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["total"], 0);
    assert!(body["data"].as_array().unwrap().is_empty());
}

// ============================================================
// Tag Filter Wildcard Escaping Tests
// ============================================================

#[tokio::test]
async fn test_tag_filter_with_percent_character() {
    let app = common::test_app().await;

    // Create a rule with tag containing %
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "name": "Percent Tag Rule",
                "channel": "test",
                "tags": ["100%"],
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Create another rule with different tag
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "name": "Other Rule",
                "channel": "test",
                "tags": ["other"],
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Filter by tag "100%" should only return the first rule
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/rules?tag=100%25", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["total"], 1);
    assert_eq!(body["data"][0]["name"], "Percent Tag Rule");
}

#[tokio::test]
async fn test_tag_filter_with_underscore_character() {
    let app = common::test_app().await;

    // Create a rule with tag containing _
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "name": "Underscore Tag Rule",
                "channel": "test",
                "tags": ["my_tag"],
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Create a rule with tag "myXtag" — should NOT match "my_tag" filter
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "name": "Similar Tag Rule",
                "channel": "test",
                "tags": ["myXtag"],
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Filter by tag "my_tag" should only match the underscore one
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/rules?tag=my_tag", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["total"], 1);
    assert_eq!(body["data"][0]["name"], "Underscore Tag Rule");
}

// ============================================================
// Bulk Import Partial Failure Tests
// ============================================================

#[tokio::test]
async fn test_import_partial_failure_with_duplicate_ids() {
    let app = common::test_app().await;

    // Import rules with a duplicate ID
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/import",
            Some(json!([
                {
                    "id": "dup-id",
                    "name": "First",
                    "channel": "ch1",
                    "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"1"}}}]
                },
                {
                    "id": "dup-id",
                    "name": "Duplicate",
                    "channel": "ch1",
                    "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"2"}}}]
                },
                {
                    "name": "Third",
                    "channel": "ch1",
                    "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"3"}}}]
                }
            ])),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["imported"], 2);
    assert_eq!(body["failed"], 1);
    assert_eq!(body["errors"].as_array().unwrap().len(), 1);
}

// ============================================================
// Rule Version History Tests
// ============================================================

#[tokio::test]
async fn test_rule_version_history() {
    let app = common::test_app().await;

    // Create
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "name": "Versioned Rule",
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"v1"}}}]
            })),
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let rule_id = body["data"]["id"].as_str().unwrap().to_string();

    // Update
    let _resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/rules/{}", rule_id),
            Some(json!({ "name": "Versioned Rule v2" })),
        ))
        .await
        .unwrap();

    // Change status
    let _resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/rules/{}/status", rule_id),
            Some(json!({ "status": "paused" })),
        ))
        .await
        .unwrap();

    // Get version count
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/rules/{}", rule_id),
            None,
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["version_count"], 3);

    // Get version history
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/rules/{}/versions", rule_id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["total"], 3);
    let versions = body["data"].as_array().unwrap();
    // Ordered by version DESC
    assert_eq!(versions[0]["version"], 3);
    assert_eq!(versions[1]["version"], 2);
    assert_eq!(versions[2]["version"], 1);
}

// ============================================================
// Batch Processing Tests
// ============================================================

#[tokio::test]
async fn test_batch_mixed_success_failure() {
    let app = common::test_app().await;

    // Create a rule for "orders" channel
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "name": "Order Rule",
                "channel": "orders",
                "condition": true,
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"order"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Batch with mixed channels (orders exists, nonexistent does not have rules)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/batch",
            Some(json!({
                "messages": [
                    { "channel": "orders", "data": { "amount": 100 } },
                    { "channel": "orders", "data": { "amount": 200 } },
                    { "channel": "nonexistent", "data": { "value": 1 } }
                ]
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    // All should succeed — channels without rules just pass through
    for r in results {
        assert_eq!(r["status"], "ok");
    }
}

// ============================================================
// Validate Rule with Invalid Inputs
// ============================================================

#[tokio::test]
async fn test_validate_rule_workflow_conversion_failure() {
    let app = common::test_app().await;

    // tasks as a non-array causes validation failure
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/validate",
            Some(json!({
                "name": "Test",
                "channel": "orders",
                "condition": true,
                "tasks": "not_valid_tasks_array"
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["valid"], false);
    let errors = body["errors"].as_array().unwrap();
    assert!(errors.iter().any(|e| e["field"] == "tasks"));
}

#[tokio::test]
async fn test_validate_rule_all_checks_combined() {
    let app = common::test_app().await;

    // Multiple validation errors at once
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/validate",
            Some(json!({
                "name": "",
                "channel": "",
                "tasks": [
                    { "id": "", "name": "", "function": {} }
                ]
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["valid"], false);
    let errors = body["errors"].as_array().unwrap();
    // Should have errors for name, channel, task id, task name, function.name
    assert!(errors.len() >= 4);
}

// ============================================================
// JSON Response Format Tests
// ============================================================

#[tokio::test]
async fn test_rule_response_has_parsed_json_fields() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "name": "Parsed JSON Test",
                "channel": "test",
                "condition": { ">": [{ "var": "amount" }, 100] },
                "tags": ["tag1", "tag2"],
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;

    // condition should be a parsed JSON object, not a string
    assert!(body["data"]["condition"].is_object());
    assert_eq!(body["data"]["condition"][">"][1], 100);

    // tags should be a parsed JSON array, not a string
    assert!(body["data"]["tags"].is_array());
    assert_eq!(body["data"]["tags"][0], "tag1");

    // tasks should be a parsed JSON array, not a string
    assert!(body["data"]["tasks"].is_array());
    assert_eq!(body["data"]["tasks"][0]["id"], "t1");
}

// ============================================================
// Job Listing Tests
// ============================================================

#[tokio::test]
async fn test_list_jobs_empty() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/data/jobs", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["total"], 0);
    assert!(body["data"].as_array().unwrap().is_empty());
}

// ============================================================
// Rule Version History 404 Test
// ============================================================

#[tokio::test]
async fn test_list_versions_nonexistent_rule() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/rules/nonexistent/versions",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ============================================================
// Input Validation Tests
// ============================================================

#[tokio::test]
async fn test_create_rule_oversized_id() {
    let app = common::test_app().await;
    let long_id = "a".repeat(129);

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "id": long_id,
                "name": "Test",
                "channel": "orders",
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_create_rule_invalid_id_chars() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "id": "has spaces!",
                "name": "Test",
                "channel": "orders",
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_create_rule_empty_name_rejected() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "name": "",
                "channel": "orders",
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_create_rule_oversized_name() {
    let app = common::test_app().await;
    let long_name = "a".repeat(256);

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "name": long_name,
                "channel": "orders",
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_create_rule_invalid_channel_chars() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "name": "Test",
                "channel": "has spaces!",
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_update_rule_empty_name_rejected() {
    let app = common::test_app().await;

    // Create a valid rule first
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "name": "Valid Rule",
                "channel": "orders",
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let rule_id = body["data"]["id"].as_str().unwrap().to_string();

    // Try to update with empty name
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/rules/{}", rule_id),
            Some(json!({ "name": "   " })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_update_rule_invalid_channel_rejected() {
    let app = common::test_app().await;

    // Create a valid rule first
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "name": "Valid Rule",
                "channel": "orders",
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let rule_id = body["data"]["id"].as_str().unwrap().to_string();

    // Try to update with invalid channel
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/rules/{}", rule_id),
            Some(json!({ "channel": "bad channel!" })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
