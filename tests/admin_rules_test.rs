mod common;

use axum::http::StatusCode;
use common::{body_json, json_request};
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn test_rules_crud_lifecycle() {
    let app = common::test_app().await;

    // Create a rule (starts as draft)
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
    let rule_id = body["data"]["rule_id"].as_str().unwrap().to_string();
    assert_eq!(body["data"]["name"], "Test Rule");
    assert_eq!(body["data"]["channel"], "orders");
    assert_eq!(body["data"]["version"], 1);
    assert_eq!(body["data"]["status"], "draft");
    assert_eq!(body["data"]["rollout_percentage"], 100);

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

    // List rules
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/rules", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["total"].as_i64().unwrap() >= 1);
    assert!(!body["data"].as_array().unwrap().is_empty());

    // Update the draft rule
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/rules/{}", rule_id),
            Some(json!({"name": "Updated Rule", "priority": 20})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["name"], "Updated Rule");
    assert_eq!(body["data"]["priority"], 20);

    // Activate the rule
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/rules/{}/status", rule_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["status"], "active");

    // Archive the rule
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/rules/{}/status", rule_id),
            Some(json!({"status": "archived"})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["status"], "archived");

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
async fn test_rule_status_transitions() {
    let app = common::test_app().await;

    // Create a rule (draft)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "rule_id": "status-test",
                "name": "Status Test",
                "channel": "orders",
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["status"], "draft");

    // Activate
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/rules/status-test/status",
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["status"], "active");

    // Archive
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/rules/status-test/status",
            Some(json!({"status": "archived"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["status"], "archived");
}

#[tokio::test]
async fn test_rule_version_history() {
    let app = common::test_app().await;

    // Create and activate a rule
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "rule_id": "ver-test",
                "name": "Version Test",
                "channel": "orders",
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"v1"}}}]
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
            "/api/v1/admin/rules/ver-test/status",
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Create new draft version
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/ver-test/versions",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["version"], 2);
    assert_eq!(body["data"]["status"], "draft");

    // Update the draft
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/rules/ver-test",
            Some(json!({"name": "Version Test v2"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["name"], "Version Test v2");

    // Activate v2
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/rules/ver-test/status",
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // List versions
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/rules/ver-test/versions",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["total"].as_i64().unwrap() >= 2);
    // Versions should have rule_id field
    let versions = body["data"].as_array().unwrap();
    assert!(versions.iter().all(|v| v.get("rule_id").is_some()));
}

#[tokio::test]
async fn test_rule_list_with_filters() {
    let app = common::test_app().await;

    // Create two rules in different channels
    common::create_and_activate_rule(
        &app,
        json!({
            "name": "Filter Rule A",
            "channel": "alpha",
            "tags": ["production"],
            "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
        }),
    )
    .await;

    common::create_and_activate_rule(
        &app,
        json!({
            "name": "Filter Rule B",
            "channel": "beta",
            "tags": ["staging"],
            "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
        }),
    )
    .await;

    // Filter by channel
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/rules?channel=alpha",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let data = body["data"].as_array().unwrap();
    assert!(!data.is_empty());
    for rule in data {
        assert_eq!(rule["channel"], "alpha");
    }

    // Filter by status
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/rules?status=active",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let data = body["data"].as_array().unwrap();
    for rule in data {
        assert_eq!(rule["status"], "active");
    }

    // Filter by tag
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/rules?tag=production",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let data = body["data"].as_array().unwrap();
    assert!(!data.is_empty());
}

#[tokio::test]
async fn test_rule_pagination() {
    let app = common::test_app().await;

    // Create 3 rules
    for i in 0..3 {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/admin/rules",
                Some(json!({
                    "name": format!("Pagination Rule {}", i),
                    "channel": "orders",
                    "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
                })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    // Get page 1
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
    assert!(body["total"].as_i64().unwrap() >= 3);
    assert_eq!(body["limit"], 2);
    assert_eq!(body["offset"], 0);
}

#[tokio::test]
async fn test_rule_with_custom_id() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "rule_id": "my-custom-rule",
                "name": "Custom ID Rule",
                "channel": "orders",
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["rule_id"], "my-custom-rule");
}

#[tokio::test]
async fn test_rule_import_export() {
    let app = common::test_app().await;

    // Import
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
                    "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"imported"}}}]
                },
                {
                    "rule_id": "import-2",
                    "name": "Import Rule 2",
                    "channel": "import-ch",
                    "condition": true,
                    "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"imported"}}}]
                }
            ])),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["imported"], 2);
    assert_eq!(body["failed"], 0);

    // Export
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/rules/export?channel=import-ch",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
}

#[tokio::test]
async fn test_rule_test_endpoint() {
    let app = common::test_app().await;

    // Create a rule
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules",
            Some(json!({
                "rule_id": "test-dry-run",
                "name": "Dry Run Rule",
                "channel": "dry-run",
                "condition": true,
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"dry run"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Test the rule
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/test-dry-run/test",
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body.get("matched").is_some());
    assert!(body.get("trace").is_some());
    assert!(body.get("output").is_some());
}

#[tokio::test]
async fn test_rule_validate_endpoint() {
    let app = common::test_app().await;

    // Valid rule
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/rules/validate",
            Some(json!({
                "name": "Valid Rule",
                "channel": "orders",
                "condition": true,
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["valid"], true);

    // Invalid rule (empty name)
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
}

#[tokio::test]
async fn test_cannot_update_active_rule() {
    let app = common::test_app().await;

    // Create and activate a rule
    let rule_id = common::create_and_activate_rule(
        &app,
        json!({
            "rule_id": "no-update-active",
            "name": "No Update Active",
            "channel": "orders",
            "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"test"}}}]
        }),
    )
    .await;

    // Try to update the active rule — should fail (no draft exists)
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/rules/{}", rule_id),
            Some(json!({"name": "Should Fail"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_create_new_version_and_edit() {
    let app = common::test_app().await;

    // Create and activate a rule
    let rule_id = common::create_and_activate_rule(
        &app,
        json!({
            "rule_id": "new-ver-test",
            "name": "New Version Test",
            "channel": "orders",
            "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"v1"}}}]
        }),
    )
    .await;

    // Create new version (draft v2)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/admin/rules/{}/versions", rule_id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["version"], 2);
    assert_eq!(body["data"]["status"], "draft");

    // Edit the draft
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/rules/{}", rule_id),
            Some(json!({"name": "New Version Test v2"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Cannot create another draft when one exists
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/admin/rules/{}/versions", rule_id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}
