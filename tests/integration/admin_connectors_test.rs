use crate::common;

use crate::common::{body_json, json_request};
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn test_connectors_crud_lifecycle() {
    let app = common::test_app().await;

    // Create a connector
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "name": "test-http",
                "connector_type": "http",
                "config": {
                    "url": "https://example.com/api",
                    "method": "POST",
                    "headers": { "Authorization": "Bearer secret-token-123" }
                }
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    let connector_id = body["data"]["id"].as_str().unwrap().to_string();
    assert_eq!(body["data"]["name"], "test-http");
    assert_eq!(body["data"]["connector_type"], "http");

    // Get the connector
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
    assert_eq!(body["data"]["name"], "test-http");

    // List connectors (should include our new one)
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/connectors", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let connectors = body["data"].as_array().unwrap();
    assert!(!connectors.is_empty());
    assert_eq!(body["total"].as_i64().unwrap(), connectors.len() as i64);
    assert_eq!(body["limit"], 50);
    assert_eq!(body["offset"], 0);

    // Update the connector
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/connectors/{}", connector_id),
            Some(json!({
                "config": {
                    "url": "https://example.com/v2/api",
                    "method": "PUT"
                }
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["name"], "test-http");

    // Delete the connector
    let resp = app
        .clone()
        .oneshot(json_request(
            "DELETE",
            &format!("/api/v1/admin/connectors/{}", connector_id),
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
            &format!("/api/v1/admin/connectors/{}", connector_id),
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ============================================================
// Connector Input Validation Tests
// ============================================================

#[tokio::test]
async fn test_create_connector_invalid_type() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "name": "bad-connector",
                "connector_type": "grpc",
                "config": {}
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_create_connector_invalid_config_structure() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "name": "bad-config",
                "connector_type": "http",
                "config": "not an object"
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_create_connector_non_http_url_scheme() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "name": "ftp-connector",
                "connector_type": "http",
                "config": {
                    "url": "ftp://example.com/files",
                    "method": "GET"
                }
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_update_connector_invalid_type() {
    let app = common::test_app().await;

    // Create a valid connector first
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "name": "valid-http",
                "connector_type": "http",
                "config": {
                    "url": "https://example.com/api",
                    "method": "POST"
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    let connector_id = body["data"]["id"].as_str().unwrap().to_string();

    // Try to update with invalid type
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/connectors/{}", connector_id),
            Some(json!({ "connector_type": "grpc" })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

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
// Connector Update with type + config validation
// ============================================================

#[tokio::test]
async fn test_update_connector_with_type_and_config() {
    let app = common::test_app().await;

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
// Data sync processing with metadata
// ============================================================

#[tokio::test]
async fn test_sync_processing_with_metadata() {
    let app = common::test_app().await;

    common::create_and_activate_channel(
        &app,
        "meta-ch",
        json!({
            "name": "Metadata Workflow",
            "condition": true,
            "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "meta test"}}}]
        }),
    )
    .await;

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

// ============================================================
// R4: config-only updates are validated against the stored type
// ============================================================

#[tokio::test]
async fn test_update_connector_config_only_invalid_rejected() {
    let app = common::test_app().await;

    // Create a db connector (stored type: db)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "r4-db-conn",
                "name": "r4-db",
                "connector_type": "db",
                "config": {"connection_string": "sqlite::memory:"}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Config-only update (no connector_type) that is invalid for type db
    // (missing connection_string) must be rejected, not silently persisted.
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/connectors/r4-db-conn",
            Some(json!({"config": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("connector config"),
        "expected connector config error, got {body:?}"
    );

    // The stored config must be unchanged
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/connectors/r4-db-conn",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let config_json = body["data"]["config_json"].as_str().unwrap();
    // `connection_string` is masked on read, so assert the key survived rather
    // than its value: the rejected `{}` must not have replaced the stored row.
    let stored: serde_json::Value = serde_json::from_str(config_json).unwrap();
    assert!(
        stored.get("connection_string").is_some(),
        "stored config should be unchanged, got {config_json}"
    );
}

#[tokio::test]
async fn test_update_connector_config_only_valid_accepted() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "r4-db-conn-ok",
                "name": "r4-db-ok",
                "connector_type": "db",
                "config": {"connection_string": "sqlite::memory:"}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // A valid config-only update still passes
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/connectors/r4-db-conn-ok",
            Some(json!({"config": {"connection_string": "sqlite:file.db"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_update_connector_config_only_wrong_shape_for_stored_type() {
    let app = common::test_app().await;

    // Create a cache connector; then try to push an invalid backend via a
    // config-only update.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "r4-cache-conn",
                "name": "r4-cache",
                "connector_type": "cache",
                "config": {"backend": "memory"}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/connectors/r4-cache-conn",
            Some(json!({"config": {"backend": "memcached"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ============================================================
// F34: the mask must never round-trip back in as a credential
// ============================================================

/// The exact cycle an admin UI performs: read the connector, change one
/// visible field, PUT the whole object back. Every secret in that object is
/// `"******"`, because that is all the reader was ever shown.
#[tokio::test]
async fn test_connector_get_edit_put_preserves_secrets() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "f34-http",
                "name": "f34-http",
                "connector_type": "http",
                "config": {
                    "url": "https://api.example.com/v1",
                    "method": "POST",
                    "timeout_secs": 5,
                    "auth": {"type": "bearer", "token": "real-secret-token"}
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Read it back the way a UI would.
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/connectors/f34-http",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let mut config: serde_json::Value =
        serde_json::from_str(body["data"]["config_json"].as_str().unwrap()).unwrap();
    assert_eq!(config["auth"]["token"], "******", "GET must mask the token");

    // Edit one unrelated field and PUT the whole object back.
    config["timeout_secs"] = json!(30);
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/connectors/f34-http",
            Some(json!({"config": config})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The edit landed.
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/connectors/f34-http",
            None,
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let config: serde_json::Value =
        serde_json::from_str(body["data"]["config_json"].as_str().unwrap()).unwrap();
    assert_eq!(config["timeout_secs"], 30, "the edit must be persisted");

    // A second read reads `"******"` whether the stored token is the real
    // secret or the mask itself, so it proves nothing on its own. Round-trip
    // once more: restoring requires the stored value to *differ* from the
    // mask, so if the first PUT had persisted `"******"` this one has nothing
    // to restore and comes back 400. A 200 is only reachable when the real
    // secret is still in the database.
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/connectors/f34-http",
            Some(json!({"config": config})),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "second round-trip failed, which means the first one persisted the mask"
    );
}

/// A mask with no stored counterpart cannot be restored, so it must be
/// refused rather than written as a credential.
#[tokio::test]
async fn test_connector_update_rejects_unmatched_mask() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "f34-reject",
                "name": "f34-reject",
                "connector_type": "http",
                "config": {"url": "https://api.example.com", "method": "GET"}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // `auth.token` was never stored, so "******" here is not a round-trip.
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/connectors/f34-reject",
            Some(json!({"config": {
                "url": "https://api.example.com",
                "method": "GET",
                "auth": {"type": "bearer", "token": "******"}
            }})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("auth.token"),
        "the error must name the offending field, got: {message}"
    );
}

/// On create there is nothing to restore from, so any mask is a mistake.
#[tokio::test]
async fn test_connector_create_rejects_mask_placeholder() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "name": "f34-create",
                "connector_type": "http",
                "config": {
                    "url": "https://api.example.com",
                    "method": "GET",
                    "auth": {"type": "bearer", "token": "******"}
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// The URL form of the mask round-trips too — F3 widened masking to `url`,
/// which is the field most likely to be hand-edited.
#[tokio::test]
async fn test_connector_url_password_round_trip() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "f34-cache",
                "name": "f34-cache",
                "connector_type": "cache",
                "config": {"backend": "redis", "url": "redis://admin:hunter2@redis:6379"}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/connectors/f34-cache",
            None,
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let config: serde_json::Value =
        serde_json::from_str(body["data"]["config_json"].as_str().unwrap()).unwrap();
    assert_eq!(config["url"], "redis://admin:******@redis:6379");

    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/connectors/f34-cache",
            Some(json!({"config": config})),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a round-tripped URL password must be restored, not rejected"
    );
}

/// Config validation moved out of `validate_update_connector` and into the
/// handler (it has to run *after* masked fields are restored, F34). This is
/// the end-to-end proof that an explicit type plus a bad config is still
/// rejected — the case the retired unit test used to cover.
#[tokio::test]
async fn test_update_connector_type_and_invalid_config_rejected() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "bad-cfg-conn",
                "name": "bad-cfg-conn",
                "connector_type": "http",
                "config": {"url": "https://example.com/api"}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    for bad in [json!("not an object"), json!({"method": "GET"})] {
        let resp = app
            .clone()
            .oneshot(json_request(
                "PUT",
                "/api/v1/admin/connectors/bad-cfg-conn",
                Some(json!({"connector_type": "http", "config": bad})),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "invalid config {bad} must be rejected"
        );
    }
}

/// Retry counts are exponents in the backoff schedule, so an unbounded value
/// is a config-reachable multi-hour stall (and arithmetic on it has to stay
/// overflow-safe) — the same bound Q4 put on `queue.dlq_max_retries`.
#[tokio::test]
async fn test_connector_retry_count_is_bounded() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "huge-retry",
                "name": "huge-retry",
                "connector_type": "http",
                "config": {
                    "type": "http",
                    "url": "https://api.example.com",
                    "retry": {"max_retries": 4294967295u32, "retry_delay_ms": 1000}
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // 16 is accepted.
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "ok-retry",
                "name": "ok-retry",
                "connector_type": "http",
                "config": {
                    "type": "http",
                    "url": "https://api.example.com",
                    "retry": {"max_retries": 16, "retry_delay_ms": 1000}
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}
