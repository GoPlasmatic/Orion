use crate::common;

use crate::common::{body_json, json_request};
use axum::body::Body;
use axum::http::{Request, StatusCode};
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
    config.admin_auth.api_keys = vec!["test-secret-key".to_string()];
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
    config.admin_auth.api_keys = vec!["test-secret-key".to_string()];
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
    config.admin_auth.api_keys = vec!["test-secret-key".to_string()];
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
async fn test_admin_auth_sha256_hashed_key_accepts_plaintext_token() {
    // S11: operators may store `sha256:<hex>` digests instead of plaintext
    // keys; clients still present the plaintext key.
    use sha2::Digest;
    let digest = sha2::Sha256::digest(b"hashed-secret-key");
    let mut config = orion::config::AppConfig::default();
    config.admin_auth.enabled = true;
    config.admin_auth.api_keys = vec![format!("sha256:{}", hex::encode(digest))];
    let app = common::test_app_with_config(config).await;

    // Correct plaintext token → 200
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/admin/engine/status")
        .header("Authorization", "Bearer hashed-secret-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Presenting the digest itself must NOT authenticate
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/admin/engine/status")
        .header(
            "Authorization",
            format!("Bearer sha256:{}", hex::encode(digest)),
        )
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Wrong token → 401
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/admin/engine/status")
        .header("Authorization", "Bearer wrong-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_admin_auth_custom_header() {
    let mut config = orion::config::AppConfig::default();
    config.admin_auth.enabled = true;
    config.admin_auth.api_keys = vec!["my-api-key".to_string()];
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
    config.admin_auth.api_keys = vec!["test-secret-key".to_string()];
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
async fn test_traces_list_requires_admin_key() {
    let mut config = orion::config::AppConfig::default();
    config.admin_auth.enabled = true;
    config.admin_auth.api_keys = vec!["test-secret-key".to_string()];
    let app = common::test_app_with_config(config).await;

    // Without a key → 401 (traces expose full payloads)
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/data/traces", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // With the admin key → 200
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/data/traces")
        .header("Authorization", "Bearer test-secret-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_trace_get_requires_admin_key_or_token() {
    let mut config = orion::config::AppConfig::default();
    config.admin_auth.enabled = true;
    config.admin_auth.api_keys = vec!["test-secret-key".to_string()];
    let state = common::test_state_with_config(config).await;
    let app = orion::server::build_router(state.clone());

    // Seed a token-bearing async trace via the repository (the admin API
    // requires the key this test is about).
    use sha2::Digest;
    let token = "r12-capability-token";
    let token_hash = hex::encode(sha2::Sha256::digest(token.as_bytes()));
    let trace = state
        .trace_repo
        .create_pending("r12-guarded", None, "async", None, Some(&token_hash))
        .await
        .unwrap();
    let trace_id = trace.id;

    // Missing trace → 404 regardless of credentials.
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/data/traces/some-id", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Real trace, no key and no token → 401 (R12, handler-enforced).
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/data/traces/{trace_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // The admin key grants access without the token…
    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/data/traces/{trace_id}"))
        .header("Authorization", "Bearer test-secret-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // …and the capability token grants access without the key, so a
    // data-plane caller can poll its own submission under admin auth.
    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/data/traces/{trace_id}"))
        .header("x-trace-token", token)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_trace_token_scopes_reads_on_default_config() {
    // Default config: admin auth off. R12's whole point — one caller must
    // not be able to read another's async result just by knowing/enumerating
    // trace ids.
    let app = common::test_app().await;

    common::create_and_activate_channel(&app, "r12-open", common::simple_log_workflow("R12 Open"))
        .await;
    let (trace_id, token) = common::submit_async(
        &app,
        "/api/v1/data/r12-open/async",
        json!({"data": {"secret_payload": "for-my-eyes-only"}}),
    )
    .await;

    // No token → 401 even with auth disabled.
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/data/traces/{trace_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Wrong token → 401.
    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/data/traces/{trace_id}"))
        .header("x-trace-token", "not-the-token")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Right token via the query-param form → 200.
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/data/traces/{trace_id}?token={token}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_admin_auth_health_not_protected() {
    let mut config = orion::config::AppConfig::default();
    config.admin_auth.enabled = true;
    config.admin_auth.api_keys = vec!["test-secret-key".to_string()];
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
// Multi-API-key authentication (key rotation)
// ============================================================

#[tokio::test]
async fn test_admin_auth_multiple_api_keys() {
    let mut config = orion::config::AppConfig::default();
    config.admin_auth.enabled = true;
    config.admin_auth.api_keys = vec![
        "primary-key".to_string(),
        "rotation-key-1".to_string(),
        "rotation-key-2".to_string(),
    ];
    let app = common::test_app_with_config(config).await;

    // Primary key should work
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/admin/engine/status")
        .header("Authorization", "Bearer primary-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // First rotation key should work
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/admin/engine/status")
        .header("Authorization", "Bearer rotation-key-1")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Second rotation key should work
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/admin/engine/status")
        .header("Authorization", "Bearer rotation-key-2")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Unknown key should be rejected
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/admin/engine/status")
        .header("Authorization", "Bearer unknown-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
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
    config.admin_auth.api_keys = vec!["test-secret-key".to_string()];
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

// ============================================================
// /health topology disclosure (O9)
// ============================================================

#[tokio::test]
async fn test_health_hides_topology_from_anonymous_when_auth_enabled() {
    let mut config = orion::config::AppConfig::default();
    config.admin_auth.enabled = true;
    config.admin_auth.api_keys = vec!["test-secret-key".to_string()];
    let app = common::test_app_with_config(config).await;

    let resp = app
        .oneshot(json_request("GET", "/health", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;

    // Coarse signal stays public…
    assert_eq!(body["status"], "ok");
    assert!(body["components"]["database"].is_string());
    // …but topology detail requires a credential (O9).
    for key in [
        "git_hash",
        "build_timestamp",
        "workflows_loaded",
        "connectors",
        "channels",
    ] {
        assert!(
            body.get(key).is_none(),
            "anonymous /health must not carry {key}, body={body}"
        );
    }
}

#[tokio::test]
async fn test_health_serves_topology_with_admin_key() {
    let mut config = orion::config::AppConfig::default();
    config.admin_auth.enabled = true;
    config.admin_auth.api_keys = vec!["test-secret-key".to_string()];
    let app = common::test_app_with_config(config).await;

    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .header("Authorization", "Bearer test-secret-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["git_hash"].is_string());
    assert!(body["connectors"]["circuit_breakers"].is_object());
    assert!(body["channels"]["quarantined"].is_array());
}

#[tokio::test]
async fn test_health_serves_topology_when_auth_disabled() {
    // Auth off = the whole admin plane is open; /health hiding detail would
    // protect nothing and would blind dev setups.
    let app = common::test_app().await;

    let resp = app
        .oneshot(json_request("GET", "/health", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["git_hash"].is_string());
    assert!(body["connectors"]["circuit_breakers"].is_object());
}
