mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

fn json_request(method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let body = match body {
        Some(v) => Body::from(serde_json::to_string(&v).unwrap()),
        None => Body::empty(),
    };
    builder.body(body).unwrap()
}

async fn body_json(response: axum::http::Response<Body>) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

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
