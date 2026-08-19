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
    assert_eq!(body["data"]["enabled"], false);
    assert!(body["data"].get("breakers").is_some());
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

/// F18: workflows bind connectors by *name*, and nothing tied the two
/// together. Renaming a connector left every active workflow referencing the
/// old name resolving to nothing — a 500 per request, with no error at rename
/// time and no load issue (that list covers connectors that failed to load,
/// not dangling references to them).
#[tokio::test]
async fn test_rename_is_refused_while_an_active_workflow_references_the_name() {
    let app = common::test_app().await;
    let conn_id = common::create_connector(&app, common::db_connector("orders-db")).await;

    // A workflow referencing it, activated.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "reads-orders",
                "tasks": [{
                    "id": "t1",
                    "name": "read",
                    "function": { "name": "db_read", "input": {
                        "connector": "orders-db",
                        "query": "SELECT 1",
                        "output": "data.r"
                    }}
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let wf_id = body_json(resp).await["data"]["workflow_id"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{wf_id}/status"),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The rename is refused, and says which workflow is in the way.
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/connectors/{conn_id}"),
            Some(json!({"name": "orders-db-v2"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let msg = body_json(resp).await.to_string();
    assert!(msg.contains("orders-db"), "must name the connector: {msg}");
    assert!(msg.contains(&wf_id), "must name the workflow: {msg}");

    // A non-rename update of the same connector is unaffected.
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/connectors/{conn_id}"),
            Some(json!({"enabled": true})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Archive the workflow and the rename goes through.
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{wf_id}/status"),
            Some(json!({"status": "archived"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/connectors/{conn_id}"),
            Some(json!({"name": "orders-db-v2"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"]["name"], "orders-db-v2");
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
                    "max_response_size": 1024,
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

    // Edit one unrelated (allowlist-readable) field and PUT the whole
    // object back. It must be a field the read API serves readable — an
    // unknown key's value comes back masked under the H3 allowlist, so a
    // client cannot meaningfully edit one from a GET.
    config["max_response_size"] = json!(2048);
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
    assert_eq!(
        config["max_response_size"], 2048,
        "the edit must be persisted"
    );

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

/// S18 gave one URL two maskable positions (userinfo password + secret-named
/// query parameter). Rotating one while round-tripping the other still
/// masked must restore the masked one positionally — the pre-fix behaviour
/// persisted the literal `"******"` as the live credential, because neither
/// the whole-string restore nor the identity-based detection matched a
/// partially-edited multi-secret URL.
#[tokio::test]
async fn test_connector_multi_secret_url_partial_round_trip() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "s18-multi",
                "name": "s18-multi",
                "connector_type": "http",
                "config": {
                    "url": "https://svc:oldpass@api.example.com/v1?api_key=real-key&page=2",
                    "method": "GET"
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // GET shows both positions masked.
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/connectors/s18-multi",
            None,
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let config: serde_json::Value =
        serde_json::from_str(body["data"]["config_json"].as_str().unwrap()).unwrap();
    assert_eq!(
        config["url"],
        "https://svc:******@api.example.com/v1?api_key=******&page=2"
    );

    // Rotate the password, round-trip the query secret still masked.
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/connectors/s18-multi",
            Some(json!({"config": {
                "url": "https://svc:newpass@api.example.com/v1?api_key=******&page=2",
                "method": "GET"
            }})),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the still-masked query secret must be restored, not rejected or persisted"
    );

    // Round-trip once more with everything masked. Restoring requires the
    // stored values to differ from the mask, so a 200 here proves the first
    // PUT persisted the real api_key rather than the sentinel — the same
    // trick as the F34 double round-trip above.
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/connectors/s18-multi",
            None,
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let config: serde_json::Value =
        serde_json::from_str(body["data"]["config_json"].as_str().unwrap()).unwrap();
    assert_eq!(
        config["url"],
        "https://svc:******@api.example.com/v1?api_key=******&page=2"
    );
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/connectors/s18-multi",
            Some(json!({"config": config})),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "second round-trip failed, which means the first PUT persisted the sentinel"
    );
}

/// A masked query parameter the stored URL never carried cannot be restored
/// and must be refused — the sentinel never persists.
#[tokio::test]
async fn test_connector_update_rejects_unmatched_url_query_mask() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "s18-reject",
                "name": "s18-reject",
                "connector_type": "http",
                "config": {"url": "https://api.example.com/v1?page=2", "method": "GET"}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // The stored URL has no api_key parameter, so this mask is not a
    // round-trip of anything.
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/connectors/s18-reject",
            Some(json!({"config": {
                "url": "https://api.example.com/v1?page=2&api_key=******",
                "method": "GET"
            }})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("url"),
        "the error must name the offending field, got: {message}"
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

// ---------------------------------------------------------------------------
// S6: every connector variant's endpoint is scheme-checked at the door, and
// address-checked when it is first dialled. Before 1.0 only `http` was gated,
// so a db connector holding `postgres://…@169.254.169.254/…` was accepted and
// connected to.
// ---------------------------------------------------------------------------

async fn create_connector_status(
    app: &axum::Router,
    id: &str,
    connector_type: &str,
    config: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": id,
                "name": id,
                "connector_type": connector_type,
                "config": config,
            })),
        ))
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

#[tokio::test]
async fn s6_db_connector_rejects_schemes_that_are_not_databases() {
    let app = common::test_app().await;

    for (id, conn) in [
        ("s6-db-http", "http://169.254.169.254/latest/meta-data"),
        ("s6-db-file", "file:///etc/passwd"),
        ("s6-db-redis", "redis://cache.example.com:6379"),
        ("s6-db-gopher", "gopher://example.com:70/"),
        ("s6-db-bare", "/var/lib/orion/orion.db"),
    ] {
        let (status, body) =
            create_connector_status(&app, id, "db", json!({"connection_string": conn})).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{conn} must be refused, got {body}"
        );
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Allowed:"),
            "{conn}: message should name the allowed schemes, got {body}"
        );
    }
}

#[tokio::test]
async fn s6_db_connector_accepts_every_real_backend_scheme() {
    let app = common::test_app().await;

    for (id, conn) in [
        ("s6-ok-pg", "postgres://u:p@db.example.com/orion"),
        ("s6-ok-pgsql", "postgresql://u:p@db.example.com/orion"),
        ("s6-ok-mysql", "mysql://u:p@db.example.com/orion"),
        ("s6-ok-sqlite", "sqlite::memory:"),
        ("s6-ok-mongo", "mongodb://m.example.com:27017/orion"),
        ("s6-ok-srv", "mongodb+srv://cluster.example.com/orion"),
    ] {
        let (status, body) =
            create_connector_status(&app, id, "db", json!({"connection_string": conn})).await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "{conn} must be accepted: {body}"
        );
    }
}

#[tokio::test]
async fn s6_cache_and_kafka_endpoints_are_shape_checked() {
    let app = common::test_app().await;

    // A redis cache pointed at an HTTP endpoint.
    let (status, body) = create_connector_status(
        &app,
        "s6-cache-http",
        "cache",
        json!({"backend": "redis", "url": "http://cache.example.com"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // rediss:// (TLS) is legitimate.
    let (status, body) = create_connector_status(
        &app,
        "s6-cache-tls",
        "cache",
        json!({"backend": "redis", "url": "rediss://cache.example.com:6379"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Kafka brokers are host:port, never URLs — a scheme here is a mistake
    // librdkafka would only report much later.
    let (status, body) = create_connector_status(
        &app,
        "s6-kafka-url",
        "kafka",
        json!({"brokers": ["http://b:9092"], "topic": "t"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = create_connector_status(
        &app,
        "s6-kafka-ok",
        "kafka",
        json!({"brokers": ["b1.example.com:9092", "b2.example.com:9092"], "topic": "t"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// The scheme gate is not the address gate: a perfectly well-formed
/// `postgres://` URL aimed at the cloud metadata endpoint is stored happily
/// and refused when the pool is opened.
#[tokio::test]
async fn s6_private_db_target_is_refused_when_the_pool_is_opened() {
    let app = common::test_app().await;

    common::create_connector(
        &app,
        json!({
            "id": "s6-metadata",
            "name": "s6-metadata",
            "connector_type": "db",
            "config": {
                "type": "db",
                "connection_string": "postgres://u:p@169.254.169.254:5432/orion",
                "connect_timeout_ms": 1000
            }
        }),
    )
    .await;

    common::create_and_activate_channel(
        &app,
        "s6-ch",
        common::workflow_with_tasks(
            "s6",
            json!([{
                "id": "r", "name": "r",
                "continue_on_error": true,
                "function": { "name": "db_read", "input": {
                    "connector": "s6-metadata",
                    "query": "SELECT 1",
                    "output": "data.rows"
                }}
            }]),
        ),
    )
    .await;

    let (_, body) = common::dsl::post(&app, "s6-ch", json!({ "data": {} })).await;

    // The task must have failed — the connection was refused, not made.
    assert!(
        !body["errors"].as_array().is_none_or(|e| e.is_empty()),
        "db_read against a link-local target must fail: {body}"
    );

    // G3 keeps the detail off the anonymous data plane ("full detail is
    // available in the trace"), so the message that names the target and the
    // opt-out is asserted where it actually lives.
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/traces?channel=s6-ch&limit=1",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    let trace_id = list["data"][0]["id"].as_str().expect("trace id");

    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/traces/{trace_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let rendered = body_json(resp).await.to_string();
    assert!(
        rendered.contains("169.254.169.254") && rendered.contains("allow_private_urls"),
        "the trace must name the refused target and the opt-out: {rendered}"
    );
}

/// ...and the opt-out has to actually work, or every private-network
/// deployment is broken.
#[tokio::test]
async fn s6_allow_private_urls_lets_a_loopback_db_through() {
    let app = common::test_app().await;

    // sqlite has no host, so use it to prove the *scheme* path stays open;
    // the address path is proved by the container suites, which all run
    // against 127.0.0.1 with allow_private_urls set.
    common::create_connector(
        &app,
        common::db_connector_sqlite("s6-local", "sqlite:file:s6_local?mode=memory&cache=shared"),
    )
    .await;

    common::create_and_activate_channel(
        &app,
        "s6-local-ch",
        common::workflow_with_tasks(
            "s6-local",
            json!([{
                "id": "r", "name": "r",
                "function": { "name": "db_read", "input": {
                    "connector": "s6-local",
                    "query": "SELECT 1 AS one",
                    "output": "data.rows"
                }}
            }]),
        ),
    )
    .await;

    let (status, body) = common::dsl::post(&app, "s6-local-ch", json!({ "data": {} })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

// ============================================================
// Connectivity probe: POST /connectors/{id}/test
// ============================================================

/// A working database connector reports reachable.
///
/// `test-connectivity` probes the *configured* storage and Kafka only; a saved
/// connector could not be checked at all, so bad credentials surfaced at the
/// first real request instead of when the operator saved them.
#[tokio::test]
async fn probing_a_working_db_connector_reports_reachable() {
    let app = common::test_app().await;
    let id = common::create_connector(&app, common::db_connector("probe-ok")).await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/admin/connectors/{id}/test"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["reachable"], true, "{body}");
    assert_eq!(body["data"]["connector_type"], "db");
    assert_eq!(
        body["data"]["probe"], "SELECT 1",
        "the probe names what it did: {body}"
    );
}

/// An unreachable backend is a `200` carrying `reachable: false`.
///
/// The probe ran and this is its answer — a 5xx would say Orion failed, which
/// is a different claim and would make the endpoint useless for the case it
/// exists to report.
#[tokio::test]
async fn probing_an_unreachable_db_connector_reports_the_failure() {
    let app = common::test_app().await;
    let id = common::create_connector(
        &app,
        json!({
            "name": "probe-broken",
            "connector_type": "db",
            "config": {"connection_string": "postgres://nobody@127.0.0.1:1/nope"}
        }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/admin/connectors/{id}/test"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "an unreachable backend is a finding, not a server error"
    );
    let body = body_json(resp).await;
    assert_eq!(body["data"]["reachable"], false, "{body}");
    assert!(
        body["data"]["error"].is_string(),
        "a failure must say why: {body}"
    );
}

/// A connector whose `env://` secret is unset on this host is reported as such.
///
/// This is precisely when an operator reaches for the endpoint — the connector
/// failed to load, so it has no registry entry — which is why the probe reads
/// the stored row rather than the registry.
#[tokio::test]
async fn probing_reports_an_unresolvable_secret() {
    let app = common::test_app().await;
    let id = common::create_connector(
        &app,
        json!({
            "name": "probe-secret",
            "connector_type": "http",
            "config": {
                "url": "https://example.com",
                "auth": {"type": "bearer", "token": "env://ORION_TEST_UNSET_PROBE"}
            }
        }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/admin/connectors/{id}/test"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["reachable"], false, "{body}");
    assert_eq!(body["data"]["probe"], "secret resolution", "{body}");
}

#[tokio::test]
async fn probing_an_unknown_connector_is_404() {
    let app = common::test_app().await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors/no-such-connector/test",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ============================================================
// SMTP connector (#262)
// ============================================================

#[tokio::test]
async fn test_smtp_connector_create_mask_and_validation() {
    let app = common::test_app().await;

    // A well-formed SMTP connector is accepted, and the password is masked on
    // the way back out like every other credential.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "name": "mailer",
                "connector_type": "smtp",
                "config": {
                    "host": "smtp.example.test",
                    "port": 587,
                    "tls": "starttls",
                    "auth": {"type": "basic", "username": "u", "password": "hunter2"},
                    "from": "Orion <noreply@example.test>"
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_ne!(body["data"]["config"]["auth"]["password"], json!("hunter2"));

    // Malformed configs are refused at the door, naming the problem.
    for (config, expected) in [
        // A pasted URL where a hostname belongs.
        (
            json!({"host": "smtp://smtp.example.test", "from": "a@b.test"}),
            "hostname",
        ),
        // The default sender must parse.
        (
            json!({"host": "smtp.example.test", "from": "not an address"}),
            "email address",
        ),
        // Basic auth without a username.
        (
            json!({"host": "smtp.example.test", "from": "a@b.test",
                   "auth": {"type": "basic", "username": "", "password": "p"}}),
            "username",
        ),
    ] {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/admin/connectors",
                Some(json!({
                    "name": "bad-mailer",
                    "connector_type": "smtp",
                    "config": config
                })),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "expected 400 for {config}"
        );
        let body = body_json(resp).await;
        assert!(
            body["error"].to_string().contains(expected),
            "{config} should have reported '{expected}', got {}",
            body["error"]
        );
    }

    // `tls: "none"` is legal (dev relays) but draws the loud warning.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors/validate",
            Some(json!({
                "name": "dev-mailer",
                "connector_type": "smtp",
                "config": {"host": "localhost", "tls": "none", "from": "a@b.test"}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["valid"], json!(true));
    let warnings = body["data"]["warnings"].to_string();
    assert!(warnings.contains("cleartext"), "{warnings}");
}

#[tokio::test]
async fn test_send_email_workflow_validation_at_create() {
    let app = common::test_app().await;

    // The full message shape is accepted as a draft.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "OTP Mail",
                "condition": true,
                "tasks": [{
                    "id": "t1", "name": "Send",
                    "function": {"name": "send_email", "input": {
                        "connector": "mailer",
                        "to": {"var": "data.email"},
                        "bcc": ["Audit <audit@example.test>"],
                        "subject": "Your code",
                        "text": {"var": "temp_data.mail_body"},
                        "headers": {"Auto-Submitted": "auto-generated"},
                        "output": "temp_data.mail"
                    }}
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Authoring-time refusals: no body, a protected header, a bad static
    // address — each a named 400.
    for (input, expected) in [
        (
            json!({"connector": "m", "to": "a@b.test", "subject": "s"}),
            "'text' or 'html'",
        ),
        (
            json!({"connector": "m", "to": "a@b.test", "subject": "s",
                   "text": "b", "headers": {"From": "x@y.test"}}),
            "structured field",
        ),
        (
            json!({"connector": "m", "to": "not-an-address", "subject": "s", "text": "b"}),
            "email address",
        ),
    ] {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/admin/workflows",
                Some(json!({
                    "name": "Bad Mail",
                    "condition": true,
                    "tasks": [{
                        "id": "t1", "name": "Send",
                        "function": {"name": "send_email", "input": input}
                    }]
                })),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "expected 400 for {input}"
        );
        let body = body_json(resp).await;
        let details = body["error"]["details"].to_string();
        assert!(
            details.contains(expected),
            "{input} should have reported {expected}, got {details}"
        );
    }
}
