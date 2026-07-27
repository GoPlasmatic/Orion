use crate::common;

use crate::common::{body_json, json_request};
use axum::http::StatusCode;
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
// 6. `details` is populated end-to-end (D3)
// ============================================================

/// Before D3 the `Some(details)` branch built malformed SQL, so the column was
/// dead. Every audit write now carries the request-scoped id.
#[tokio::test]
async fn test_audit_details_carries_request_id() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request("POST", "/api/v1/admin/engine/reload", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    wait_for_audit().await;

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/audit-logs", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let entry = body["data"]
        .as_array()
        .expect("data should be an array")
        .iter()
        .find(|e| e["resource_type"] == "engine")
        .expect("reload must produce an audit entry")
        .clone();

    let details = entry["details"]
        .as_str()
        .expect("details must be persisted");
    let parsed: serde_json::Value = serde_json::from_str(details).expect("details is JSON");
    assert!(
        parsed["request_id"].as_str().is_some_and(|s| !s.is_empty()),
        "details should carry the request id, got {details}"
    );
}

// ============================================================
// 7. Import workflows generates audit entry
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

// ============================================================
// 8. Server-side filtering (O8)
// ============================================================

/// Seed a mix of workflow / channel / connector audit entries and return the
/// app plus the workflow id used.
async fn app_with_mixed_audit_entries() -> (axum::Router, String) {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(common::simple_log_workflow("Filter WF")),
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

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(common::sync_http_channel("filter-chan", &wf_id)),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let ch_id = body_json(resp).await["data"]["channel_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{ch_id}/status"),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    common::create_connector(&app, common::db_connector("filter-conn")).await;

    wait_for_audit().await;
    (app, wf_id)
}

async fn audit_query(app: &axum::Router, query: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/audit-logs{query}"),
            None,
        ))
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

#[tokio::test]
async fn test_audit_filter_each_param_narrows_results() {
    let (app, wf_id) = app_with_mixed_audit_entries().await;

    let (status, all) = audit_query(&app, "").await;
    assert_eq!(status, StatusCode::OK);
    let total = all["pagination"]["total"].as_i64().unwrap();
    assert!(total >= 5, "expected a mixed seed, got {total}");

    for (query, key, value) in [
        ("?resource_type=workflow", "resource_type", "workflow"),
        ("?resource_type=connector", "resource_type", "connector"),
        ("?action=create", "action", "create"),
        ("?action=status_active", "action", "status_active"),
    ] {
        let (status, body) = audit_query(&app, query).await;
        assert_eq!(status, StatusCode::OK, "query {query}");
        let entries = body["data"].as_array().unwrap();
        assert!(!entries.is_empty(), "{query} should match something");
        assert!(
            entries.iter().all(|e| e[key] == value),
            "{query} returned rows that do not match: {entries:?}"
        );
        assert!(
            body["pagination"]["total"].as_i64().unwrap() < total,
            "{query} must narrow the result set (total was {total})"
        );
        assert_eq!(
            body["pagination"]["total"].as_i64().unwrap(),
            entries.len() as i64,
            "total must count filtered rows"
        );
    }

    let (status, body) = audit_query(&app, &format!("?resource_id={wf_id}")).await;
    assert_eq!(status, StatusCode::OK);
    let entries = body["data"].as_array().unwrap();
    assert!(!entries.is_empty());
    assert!(entries.iter().all(|e| e["resource_id"] == wf_id));

    let (status, body) = audit_query(&app, "?principal=anonymous").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["pagination"]["total"].as_i64().unwrap(),
        total,
        "auth is off in tests, so every entry is 'anonymous'"
    );

    let (status, body) = audit_query(&app, "?principal=nobody-else").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pagination"]["total"], 0);
}

#[tokio::test]
async fn test_audit_filters_and_together() {
    let (app, wf_id) = app_with_mixed_audit_entries().await;

    // The documented example: ?action=…&resource_type=workflow.
    let (status, body) = audit_query(&app, "?action=status_active&resource_type=workflow").await;
    assert_eq!(status, StatusCode::OK);
    let entries = body["data"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["resource_id"], wf_id);

    // A matching action with a non-matching type must yield nothing, not the union.
    let (status, body) = audit_query(&app, "?action=status_active&resource_type=connector").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pagination"]["total"], 0);
    assert!(body["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_audit_time_range_filter() {
    let (app, _) = app_with_mixed_audit_entries().await;

    let (_, all) = audit_query(&app, "").await;
    let total = all["pagination"]["total"].as_i64().unwrap();

    let (status, body) = audit_query(&app, "?start_time=2000-01-01T00:00:00Z").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pagination"]["total"].as_i64().unwrap(), total);

    let (status, body) = audit_query(&app, "?end_time=2000-01-01T00:00:00Z").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pagination"]["total"], 0);

    let (status, body) = audit_query(&app, "?start_time=not-a-timestamp").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "BAD_REQUEST");
}

/// The core of O8: before the fix an unknown parameter was dropped and the
/// caller got a 200 with the *unfiltered* table.
#[tokio::test]
async fn test_audit_unknown_query_param_is_rejected() {
    let (app, _) = app_with_mixed_audit_entries().await;

    let (status, body) = audit_query(&app, "?resource_types=workflow").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a mistyped filter must not silently return everything"
    );
    assert_eq!(body["error"]["code"], "BAD_REQUEST");
    assert!(body.get("data").is_none());
}
