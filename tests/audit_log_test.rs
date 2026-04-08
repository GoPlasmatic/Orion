mod common;

use axum::http::StatusCode;
use common::{body_json, json_request};
use serde_json::json;
use tower::ServiceExt;

/// Helper: sleep briefly to let fire-and-forget audit log spawned tasks complete.
async fn wait_for_audit() {
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
}

// ============================================================
// 1. Workflow CRUD generates audit entries
// ============================================================

#[tokio::test]
async fn test_audit_workflow_crud() {
    let app = common::test_app().await;

    // Create workflow (draft)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(common::simple_log_workflow("Audit WF")),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    let wf_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    // Activate
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{}/status", wf_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Archive
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{}/status", wf_id),
            Some(json!({"status": "archived"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Delete
    let resp = app
        .clone()
        .oneshot(json_request(
            "DELETE",
            &format!("/api/v1/admin/workflows/{}", wf_id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    wait_for_audit().await;

    // Fetch audit logs
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/audit-logs", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let entries = body["data"].as_array().expect("data should be an array");

    // Filter to workflow entries
    let wf_entries: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|e| e["resource_type"] == "workflow")
        .collect();

    let actions: Vec<&str> = wf_entries
        .iter()
        .map(|e| e["action"].as_str().unwrap())
        .collect();

    assert!(actions.contains(&"create"), "missing 'create' audit entry");
    assert!(
        actions.contains(&"status_active"),
        "missing 'status_active' audit entry"
    );
    assert!(
        actions.contains(&"status_archived"),
        "missing 'status_archived' audit entry"
    );
    assert!(actions.contains(&"delete"), "missing 'delete' audit entry");
}

// ============================================================
// 2. Channel CRUD generates audit entries
// ============================================================

#[tokio::test]
async fn test_audit_channel_crud() {
    let app = common::test_app().await;

    // Create and activate a workflow (required for channel)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(common::simple_log_workflow("Chan Audit WF")),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    let wf_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{}/status", wf_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Create channel
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(common::sync_http_channel("audit-chan", &wf_id)),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    let ch_id = body["data"]["channel_id"].as_str().unwrap().to_string();

    // Activate channel
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{}/status", ch_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Archive channel
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{}/status", ch_id),
            Some(json!({"status": "archived"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    wait_for_audit().await;

    // Fetch audit logs
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/audit-logs", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let entries = body["data"].as_array().expect("data should be an array");

    // Filter to channel entries
    let ch_entries: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|e| e["resource_type"] == "channel")
        .collect();

    let actions: Vec<&str> = ch_entries
        .iter()
        .map(|e| e["action"].as_str().unwrap())
        .collect();

    assert!(
        actions.contains(&"create"),
        "missing 'create' audit entry for channel"
    );
    assert!(
        actions.contains(&"status_active"),
        "missing 'status_active' audit entry for channel"
    );
    assert!(
        actions.contains(&"status_archived"),
        "missing 'status_archived' audit entry for channel"
    );
}

// ============================================================
// 3. Connector CRUD generates audit entries
// ============================================================

#[tokio::test]
async fn test_audit_connector_crud() {
    let app = common::test_app().await;

    // Create connector
    let connector_id = common::create_connector(&app, common::db_connector("audit-conn")).await;

    // Update connector
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/connectors/{}", connector_id),
            Some(json!({"name": "audit-conn-renamed"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Delete connector
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

    wait_for_audit().await;

    // Fetch audit logs
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/audit-logs", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let entries = body["data"].as_array().expect("data should be an array");

    // Filter to connector entries
    let conn_entries: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|e| e["resource_type"] == "connector")
        .collect();

    let actions: Vec<&str> = conn_entries
        .iter()
        .map(|e| e["action"].as_str().unwrap())
        .collect();

    assert!(
        actions.contains(&"create"),
        "missing 'create' audit entry for connector"
    );
    assert!(
        actions.contains(&"update"),
        "missing 'update' audit entry for connector"
    );
    assert!(
        actions.contains(&"delete"),
        "missing 'delete' audit entry for connector"
    );
}

// ============================================================
// 4. Engine reload generates audit entry
// ============================================================

#[tokio::test]
async fn test_audit_engine_reload() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request("POST", "/api/v1/admin/engine/reload", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    wait_for_audit().await;

    // Fetch audit logs
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/audit-logs", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let entries = body["data"].as_array().expect("data should be an array");

    let engine_entries: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|e| e["resource_type"] == "engine")
        .collect();

    let actions: Vec<&str> = engine_entries
        .iter()
        .map(|e| e["action"].as_str().unwrap())
        .collect();

    assert!(
        actions.contains(&"reload"),
        "missing 'reload' audit entry for engine"
    );
}

// ============================================================
// 5. Pagination of audit logs
// ============================================================

#[tokio::test]
async fn test_audit_pagination() {
    let app = common::test_app().await;

    // Create 5 workflows to produce at least 5 audit entries
    for i in 0..5 {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/admin/workflows",
                Some(common::simple_log_workflow(&format!("Pagination WF {}", i))),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    wait_for_audit().await;

    // Page 1: limit=2, offset=0
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/audit-logs?limit=2&offset=0",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let page1 = body["data"].as_array().unwrap();
    assert_eq!(page1.len(), 2, "first page should have 2 entries");
    assert!(
        body["pagination"]["total"].as_i64().unwrap() >= 5,
        "total should be at least 5"
    );
    assert_eq!(body["pagination"]["offset"], 0);
    assert_eq!(body["pagination"]["limit"], 2);

    // Page 2: limit=2, offset=2
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/audit-logs?limit=2&offset=2",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let page2 = body["data"].as_array().unwrap();
    assert_eq!(page2.len(), 2, "second page should have 2 entries");
    assert_eq!(body["pagination"]["offset"], 2);
}

// ============================================================
// 6. Import workflows generates audit entry
// ============================================================

#[tokio::test]
async fn test_audit_import() {
    let app = common::test_app().await;

    // Import 2 workflows
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/import",
            Some(json!([
                {
                    "workflow_id": "audit-import-1",
                    "name": "Audit Import WF 1",
                    "condition": true,
                    "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"imported"}}}]
                },
                {
                    "workflow_id": "audit-import-2",
                    "name": "Audit Import WF 2",
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

    wait_for_audit().await;

    // Fetch audit logs
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/audit-logs", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let entries = body["data"].as_array().expect("data should be an array");

    let import_entries: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|e| e["resource_type"] == "workflow" && e["action"] == "import")
        .collect();

    assert!(
        !import_entries.is_empty(),
        "expected at least one 'import' audit entry for workflow resource_type"
    );
}
