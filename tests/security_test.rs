mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{body_json, json_request};
use serde_json::json;
use tower::ServiceExt;

// ============================================================
// SQL injection resistance
// ============================================================

#[tokio::test]
async fn test_sql_injection_in_workflow_name() {
    let app = common::test_app().await;

    let malicious_name = "'; DROP TABLE workflows;--";

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": malicious_name,
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    let workflow_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    // Verify the workflow is stored safely and retrievable
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/workflows/{}", workflow_id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["name"], malicious_name);

    // Verify the workflows table still exists by listing workflows
    let resp = app
        .oneshot(json_request("GET", "/api/v1/admin/workflows", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["total"].as_i64().unwrap() >= 1);
}

#[tokio::test]
async fn test_sql_injection_in_tag_filter() {
    let app = common::test_app().await;

    // Create a workflow with a normal tag
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "Tagged Workflow",
                "tags": ["safe-tag"],
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Attempt SQL injection via tag filter (percent-encoded: %' OR 1=1 --)
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/workflows?tag=%25%27%20OR%201%3D1%20--",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    // The injection should not return all workflows -- should match 0 since no tag matches the payload
    assert_eq!(body["total"], 0);
}

// ============================================================
// XSS resistance
// ============================================================

#[tokio::test]
async fn test_xss_in_workflow_description() {
    let app = common::test_app().await;

    let xss_payload = "<script>alert('xss')</script>";

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "XSS Test Workflow",
                "description": xss_payload,
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    let workflow_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    // Verify the description is stored and returned as-is (JSON-escaped, not interpreted)
    let resp = app
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/workflows/{}", workflow_id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["description"], xss_payload);
}

// ============================================================
// Payload size & depth limits
// ============================================================

#[tokio::test]
async fn test_deeply_nested_json_payload() {
    let app = common::test_app().await;

    // Build a 100-level deep nested JSON structure
    let mut nested = json!({"leaf": true});
    for _ in 0..100 {
        nested = json!({"nested": nested});
    }

    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/data/orders",
            Some(json!({"data": nested})),
        ))
        .await
        .unwrap();

    // Should process without stack overflow -- either succeeds or returns a client/server error
    assert!(resp.status().as_u16() < 600);
}

// ============================================================
// Unicode handling
// ============================================================

#[tokio::test]
async fn test_unicode_in_workflow_fields() {
    let app = common::test_app().await;

    let emoji_name = "Order Processing \u{1F680}\u{2728}";
    let cjk_description = "\u{4E1A}\u{52A1}\u{89C4}\u{5219} (Business Workflow)";
    let unicode_tags = vec!["tag-\u{00E9}\u{00E8}\u{00EA}", "\u{0442}\u{0435}\u{0433}"];

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": emoji_name,
                "description": cjk_description,
                "tags": unicode_tags,
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    let workflow_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    // Verify roundtrip
    let resp = app
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/workflows/{}", workflow_id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["name"], emoji_name);
    assert_eq!(body["data"]["description"], cjk_description);
    let tags = body["data"]["tags"].as_array().unwrap();
    assert_eq!(tags[0], unicode_tags[0]);
    assert_eq!(tags[1], unicode_tags[1]);
}

// ============================================================
// Null byte handling
// ============================================================

#[tokio::test]
async fn test_null_bytes_in_string_fields() {
    let app = common::test_app().await;

    let name_with_null = "test\0name";

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": name_with_null,
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
            })),
        ))
        .await
        .unwrap();

    // System should either accept safely or reject -- must not panic
    if resp.status().is_success() {
        let body = body_json(resp).await;
        let workflow_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

        // Verify retrieval is consistent
        let resp = app
            .oneshot(json_request(
                "GET",
                &format!("/api/v1/admin/workflows/{}", workflow_id),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    // If rejected, that is also acceptable -- no crash or corruption
}

// ============================================================
// Admin API authentication
// ============================================================

#[tokio::test]
async fn test_admin_auth_missing_token_returns_401() {
    let mut config = orion::config::AppConfig::default();
    config.admin_auth.enabled = true;
    config.admin_auth.api_key = "test-secret-key".to_string();
    let app = common::test_app_with_config(config).await;

    let resp = app
        .oneshot(json_request("GET", "/api/v1/admin/engine/status", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn test_admin_auth_wrong_token_returns_401() {
    let mut config = orion::config::AppConfig::default();
    config.admin_auth.enabled = true;
    config.admin_auth.api_key = "test-secret-key".to_string();
    let app = common::test_app_with_config(config).await;

    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/admin/engine/status")
        .header("Authorization", "Bearer wrong-key")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn test_admin_auth_correct_token_returns_200() {
    let mut config = orion::config::AppConfig::default();
    config.admin_auth.enabled = true;
    config.admin_auth.api_key = "test-secret-key".to_string();
    let app = common::test_app_with_config(config).await;

    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/admin/engine/status")
        .header("Authorization", "Bearer test-secret-key")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_admin_auth_custom_header() {
    let mut config = orion::config::AppConfig::default();
    config.admin_auth.enabled = true;
    config.admin_auth.api_key = "my-api-key".to_string();
    config.admin_auth.header = "X-API-Key".to_string();
    let app = common::test_app_with_config(config).await;

    // Wrong header name → 401
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/admin/engine/status")
        .header("Authorization", "Bearer my-api-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Correct custom header → 200
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/admin/engine/status")
        .header("X-API-Key", "my-api-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_admin_auth_data_routes_not_protected() {
    let mut config = orion::config::AppConfig::default();
    config.admin_auth.enabled = true;
    config.admin_auth.api_key = "test-secret-key".to_string();
    let app = common::test_app_with_config(config).await;

    // Data endpoint should work without auth
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/orders",
            Some(json!({"data": {"test": true}})),
        ))
        .await
        .unwrap();
    // Should NOT be 401 — either 200 (no channel) or 404, but not auth-blocked
    assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_admin_auth_health_not_protected() {
    let mut config = orion::config::AppConfig::default();
    config.admin_auth.enabled = true;
    config.admin_auth.api_key = "test-secret-key".to_string();
    let app = common::test_app_with_config(config).await;

    let resp = app
        .oneshot(json_request("GET", "/health", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_admin_auth_disabled_allows_all() {
    // Default config has auth disabled
    let app = common::test_app().await;

    let resp = app
        .oneshot(json_request("GET", "/api/v1/admin/engine/status", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ============================================================
// Connector secret masking
// ============================================================

#[tokio::test]
async fn test_connector_secret_masking() {
    let app = common::test_app().await;

    let secret_token = "super-secret-token-12345";

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "name": "Secret Connector",
                "connector_type": "http",
                "config": {
                    "url": "https://example.com/api",
                    "method": "POST",
                    "auth": {
                        "type": "bearer",
                        "token": secret_token
                    }
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    let connector_id = body["data"]["id"].as_str().unwrap().to_string();

    // The create response should already have masked secrets
    let config_json_str = body["data"]["config_json"].as_str().unwrap();
    let config: serde_json::Value = serde_json::from_str(config_json_str).unwrap();
    assert_eq!(config["auth"]["token"], "******");
    assert_ne!(config["auth"]["token"], secret_token);

    // GET should also return masked secrets
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/connectors/{}", connector_id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let config_json_str = body["data"]["config_json"].as_str().unwrap();
    let config: serde_json::Value = serde_json::from_str(config_json_str).unwrap();
    assert_eq!(config["auth"]["token"], "******");

    // URL should NOT be masked
    assert_eq!(config["url"], "https://example.com/api");
}

// ============================================================
// Metrics endpoint authentication
// ============================================================

#[tokio::test]
async fn test_metrics_endpoint_protected_when_auth_enabled() {
    let mut config = orion::config::AppConfig::default();
    config.admin_auth.enabled = true;
    config.admin_auth.api_key = "test-secret-key".to_string();
    config.metrics.enabled = true;
    let app = common::test_app_with_config(config).await;

    // Without auth header → 401
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/metrics", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // With valid auth header → 200
    let req = Request::builder()
        .method("GET")
        .uri("/metrics")
        .header("Authorization", "Bearer test-secret-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_metrics_endpoint_open_when_auth_disabled() {
    let app = common::test_app().await;

    let resp = app
        .oneshot(json_request("GET", "/metrics", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
