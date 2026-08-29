use crate::common;

use crate::common::{body_json, json_request};
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

/// Audit writes are fire-and-forget spawned tasks, so their completion is a
/// condition to poll for, not a duration to guess. Polls the admin list
/// until every `(resource_type, action)` pair is present (5s deadline).
async fn wait_for_audit_entries(app: &axum::Router, expected: &[(&str, &str)]) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let resp = app
            .clone()
            .oneshot(json_request(
                "GET",
                "/api/v1/admin/audit-logs?limit=100",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let empty = Vec::new();
        let entries = body["data"].as_array().unwrap_or(&empty);
        if expected.iter().all(|(rt, action)| {
            entries
                .iter()
                .any(|e| e["resource_type"] == *rt && e["action"] == *action)
        }) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "audit entries {expected:?} not all present within 5s; last body: {body}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Poll until the audit log holds at least `min_total` entries (5s deadline).
async fn wait_for_audit_total(app: &axum::Router, min_total: i64) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let resp = app
            .clone()
            .oneshot(json_request(
                "GET",
                "/api/v1/admin/audit-logs?limit=1",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        if body["total"].as_i64().unwrap_or(0) >= min_total {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "audit total did not reach {min_total} within 5s; last body: {body}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
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

    wait_for_audit_entries(
        &app,
        &[
            ("workflow", "create"),
            ("workflow", "status_active"),
            ("workflow", "status_archived"),
            ("workflow", "delete"),
        ],
    )
    .await;

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

    wait_for_audit_entries(
        &app,
        &[
            ("channel", "create"),
            ("channel", "status_active"),
            ("channel", "status_archived"),
        ],
    )
    .await;

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

    wait_for_audit_entries(
        &app,
        &[
            ("connector", "create"),
            ("connector", "update"),
            ("connector", "delete"),
        ],
    )
    .await;

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

    wait_for_audit_entries(&app, &[("engine", "reload")]).await;

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

    wait_for_audit_total(&app, 5).await;

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
        body["total"].as_i64().unwrap() >= 5,
        "total should be at least 5"
    );
    assert_eq!(body["offset"], 0);
    assert_eq!(body["limit"], 2);

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
    assert_eq!(body["offset"], 2);
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

    wait_for_audit_entries(&app, &[("engine", "reload")]).await;

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
    assert_eq!(body["data"]["imported"], 2);

    wait_for_audit_entries(&app, &[("workflow", "import")]).await;

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

    wait_for_audit_entries(
        &app,
        &[
            ("workflow", "create"),
            ("workflow", "status_active"),
            ("channel", "create"),
            ("channel", "status_active"),
            ("connector", "create"),
        ],
    )
    .await;
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
    let total = all["total"].as_i64().unwrap();
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
            body["total"].as_i64().unwrap() < total,
            "{query} must narrow the result set (total was {total})"
        );
        assert_eq!(
            body["total"].as_i64().unwrap(),
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
        body["total"].as_i64().unwrap(),
        total,
        "auth is off in tests, so every entry is 'anonymous'"
    );

    let (status, body) = audit_query(&app, "?principal=nobody-else").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 0);
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
    assert_eq!(body["total"], 0);
    assert!(body["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_audit_time_range_filter() {
    let (app, _) = app_with_mixed_audit_entries().await;

    let (_, all) = audit_query(&app, "").await;
    let total = all["total"].as_i64().unwrap();

    let (status, body) = audit_query(&app, "?start_time=2000-01-01T00:00:00Z").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"].as_i64().unwrap(), total);

    let (status, body) = audit_query(&app, "?end_time=2000-01-01T00:00:00Z").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 0);

    let (status, body) = audit_query(&app, "?start_time=not-a-timestamp").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "VALIDATION_ERROR");
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
    assert_eq!(body["error"]["code"], "VALIDATION_ERROR");
    assert!(body.get("data").is_none());
}

// ============================================================
// Audit v2 (O7): actor identity, request context, drain, /test
// ============================================================

/// Two admin keys sharing a long prefix — the case the old 8-character
/// `key_prefix` actor could not tell apart, because it was literally the first
/// eight characters of the presented token.
const KEY_A: &str = "orion_sk_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const KEY_B: &str = "orion_sk_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn authed_request(
    method: &str,
    uri: &str,
    key: &str,
    body: Option<serde_json::Value>,
) -> axum::http::Request<axum::body::Body> {
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", format!("Bearer {key}"))
        .header("user-agent", "orion-tests/1.0")
        .header("x-request-id", "req-audit-o7");
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            axum::body::Body::from(serde_json::to_string(&v).unwrap())
        }
        None => axum::body::Body::empty(),
    };
    builder.body(body).unwrap()
}

async fn authed_app() -> axum::Router {
    let mut config = orion::config::AppConfig::default();
    config.admin_auth.enabled = true;
    config.admin_auth.api_keys = vec![KEY_A.to_string(), KEY_B.to_string()];
    common::test_app_with_config(config).await
}

/// Poll the (authenticated) audit list until `pred` holds.
async fn wait_for_audit<F>(app: &axum::Router, key: &str, pred: F) -> serde_json::Value
where
    F: Fn(&serde_json::Value) -> bool,
{
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let resp = app
            .clone()
            .oneshot(authed_request(
                "GET",
                "/api/v1/admin/audit-logs?limit=100",
                key,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        if pred(&body) {
            return body;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "audit condition not met within 5s; last body: {body}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Two keys with a shared 9-character prefix must produce two distinct
/// actors, and neither may echo any part of the credential.
#[tokio::test]
async fn audit_actor_distinguishes_keys_sharing_a_prefix() {
    let app = authed_app().await;

    for key in [KEY_A, KEY_B] {
        let resp = app
            .clone()
            .oneshot(authed_request(
                "POST",
                "/api/v1/admin/workflows",
                key,
                Some(common::simple_log_workflow("O7 actor")),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    let body = wait_for_audit(&app, KEY_A, |b| {
        b["data"]
            .as_array()
            .map(|entries| entries.iter().filter(|e| e["action"] == "create").count() >= 2)
            .unwrap_or(false)
    })
    .await;

    let principals: std::collections::BTreeSet<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == "create")
        .filter_map(|e| e["principal"].as_str())
        .collect();
    assert_eq!(
        principals.len(),
        2,
        "two distinct keys must be two distinct actors, got {principals:?}"
    );
    for p in &principals {
        assert!(p.starts_with("key-"), "unexpected actor form: {p}");
        assert!(
            !p.contains("orion_sk") && !p.contains("aaaa") && !p.contains("bbbb"),
            "the actor must not echo the credential: {p}"
        );
    }
}

/// Every audit row carries the request context an investigation needs:
/// request id, client address and user-agent.
#[tokio::test]
async fn audit_records_request_context() {
    let app = authed_app().await;

    let resp = app
        .clone()
        .oneshot(authed_request(
            "POST",
            "/api/v1/admin/workflows",
            KEY_A,
            Some(common::simple_log_workflow("O7 context")),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let body = wait_for_audit(&app, KEY_A, |b| {
        b["data"]
            .as_array()
            .map(|entries| entries.iter().any(|e| e["action"] == "create"))
            .unwrap_or(false)
    })
    .await;
    let entry = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["action"] == "create")
        .expect("create entry");
    let details: serde_json::Value = serde_json::from_str(
        entry["details"]
            .as_str()
            .expect("details must be present, not null"),
    )
    .expect("details must be JSON");

    assert_eq!(details["request_id"], "req-audit-o7");
    assert_eq!(details["user_agent"], "orion-tests/1.0");
    // `oneshot` supplies no ConnectInfo, so the address resolves to the same
    // "unknown" the rate limiter uses — the field is populated either way.
    assert!(
        details["client_ip"].is_string(),
        "client_ip must be recorded: {details}"
    );
}

/// O7: `POST /workflows/{id}/test` executes the workflow's tasks against
/// **live connectors** and used to emit no audit event at all.
#[tokio::test]
async fn workflow_test_emits_an_audit_event() {
    let app = authed_app().await;

    let resp = app
        .clone()
        .oneshot(authed_request(
            "POST",
            "/api/v1/admin/workflows",
            KEY_A,
            Some(common::simple_log_workflow("O7 test-run")),
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
        .oneshot(authed_request(
            "POST",
            &format!("/api/v1/admin/workflows/{wf_id}/test"),
            KEY_A,
            Some(json!({"data": {"x": 1}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = wait_for_audit(&app, KEY_A, |b| {
        b["data"]
            .as_array()
            .map(|entries| entries.iter().any(|e| e["action"] == "test"))
            .unwrap_or(false)
    })
    .await;
    let entry = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["action"] == "test")
        .expect("running live connectors must be on the record");
    assert_eq!(entry["resource_type"], "workflow");
    assert_eq!(entry["resource_id"], wf_id);
}

/// Wiring check for the shutdown drain: the queue an admin handler submits to
/// must be the one `TaskHandles::shutdown()` drains, and the row must be
/// readable from the database afterwards. (The drain's *timing* — that a write
/// still in flight is waited for rather than abandoned — is covered where it
/// can be forced deterministically, in
/// `queue::audit_queue::tests::shutdown_drains_events_submitted_at_the_last_moment`.)
#[tokio::test]
async fn mutations_just_before_shutdown_are_still_recorded() {
    let (state, handles) =
        common::test_state_with_handles(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());
    let repo = state.repos.audit_logs.clone();

    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(common::simple_log_workflow("O7 shutdown")),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Exactly what main.rs does: drop the state (releasing the last audit
    // sender), then run the background-task shutdown, which drains.
    drop(state);
    handles.shutdown().await;

    let rows = repo
        .list_paginated(&Default::default())
        .await
        .expect("audit rows readable after shutdown");
    assert!(
        rows.data.iter().any(|e| e.action == "create"),
        "the drain must persist a mutation accepted immediately before shutdown"
    );
}

/// §2.6: an active-set change and its audit row are one commit, so the row is
/// there the instant the mutation answers — no queue to drain, nothing to poll
/// for.
///
/// This is the observable form of the guarantee. Before, the audit row was
/// handed to a bounded queue after the entity write had already committed, so
/// between those two points the change was live and unrecorded — and it stayed
/// that way if the process exited, if the queue was full, or if the INSERT
/// failed. Every other test in this file polls (`wait_for_audit_entries`)
/// because that is what a queued write requires; this one deliberately does
/// not, and would go red if the audit row went back on the queue.
#[tokio::test]
async fn an_active_set_change_is_audited_before_it_answers() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(common::simple_log_workflow("Synchronous Audit")),
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

    // No polling, no sleep: the next request the client could possibly make.
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/audit-logs?limit=100",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let entries = body["data"].as_array().expect("audit entries");

    assert!(
        entries.iter().any(|e| e["resource_type"] == "workflow"
            && e["action"] == "status_active"
            && e["resource_id"] == wf_id.as_str()),
        "the activation committed but its audit row is not visible yet — the row \
         must commit with the change, not behind it: {body}"
    );
}
