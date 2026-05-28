//! A1 (Tier-1 ergonomics): schema-validated function input contracts.
//!
//! Verifies that:
//!   - workflow create rejects tasks with missing/typed-wrong function inputs
//!     and returns field-pathed details via A3's envelope
//!   - the POST /workflows/validate endpoint reports the same issues in its
//!     `errors[]` array
//!   - GET /api/v1/admin/functions returns the registered schema list

use crate::common::{body_json, json_request, test_app};
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn create_workflow_with_missing_function_input_field_returns_field_pathed_details() {
    let app = test_app().await;
    // db_read requires connector + query. Omit connector.
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "broken-db-read",
                "tasks": [{
                    "id": "t1",
                    "name": "read",
                    "function": {
                        "name": "db_read",
                        "input": { "query": "SELECT 1" }
                    }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    let details = body["error"]["details"]
        .as_array()
        .expect("expected field-pathed details");
    assert!(
        details
            .iter()
            .any(|d| d["path"] == "tasks[0].function.input.connector" && d["code"] == "REQUIRED"),
        "details should report missing connector field, got {body:?}"
    );
}

#[tokio::test]
async fn create_workflow_with_wrong_input_type_returns_type_mismatch_with_got() {
    let app = test_app().await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "wrong-types",
                "tasks": [{
                    "id": "t1",
                    "name": "read",
                    "function": {
                        "name": "cache_read",
                        "input": { "connector": 42, "key": "k" }
                    }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    let details = body["error"]["details"].as_array().unwrap();
    let mismatch = details
        .iter()
        .find(|d| d["path"] == "tasks[0].function.input.connector")
        .expect("expected TYPE_MISMATCH on connector");
    assert_eq!(mismatch["code"], "TYPE_MISMATCH");
    assert_eq!(mismatch["expected"], "string");
    assert_eq!(mismatch["got"], 42);
}

#[tokio::test]
async fn create_workflow_with_valid_inputs_succeeds() {
    let app = test_app().await;
    // Use cache_read with all required fields — must NOT trip the schema check.
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "happy-path",
                "tasks": [{
                    "id": "t1",
                    "name": "read",
                    "function": {
                        "name": "cache_read",
                        "input": { "connector": "my-cache", "key": "k" }
                    }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn validate_endpoint_returns_schema_errors_in_errors_array() {
    let app = test_app().await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/validate",
            Some(json!({
                "name": "test-validate",
                "tasks": [{
                    "id": "t1",
                    "name": "broken",
                    "function": { "name": "mongo_read", "input": {} }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["valid"], false);
    let errs = body["errors"].as_array().unwrap();
    let paths: Vec<&str> = errs
        .iter()
        .map(|e| e["field"].as_str().unwrap_or(""))
        .collect();
    assert!(paths.contains(&"tasks[0].function.input.connector"));
    assert!(paths.contains(&"tasks[0].function.input.database"));
    assert!(paths.contains(&"tasks[0].function.input.collection"));
}

#[tokio::test]
async fn list_functions_returns_registry_with_schemas() {
    let app = test_app().await;
    let resp = app
        .oneshot(json_request("GET", "/api/v1/admin/functions", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let data = body["data"].as_array().expect("data must be an array");
    let names: Vec<&str> = data.iter().filter_map(|f| f["name"].as_str()).collect();
    assert!(names.contains(&"cache_read"));
    assert!(names.contains(&"db_read"));
    assert!(names.contains(&"channel_call"));

    // Spot-check one entry's input field schema.
    let cache_read = data
        .iter()
        .find(|f| f["name"] == "cache_read")
        .expect("cache_read must be present");
    let fields = cache_read["input_fields"].as_array().unwrap();
    let connector = fields
        .iter()
        .find(|f| f["name"] == "connector")
        .expect("connector field must be present");
    assert_eq!(connector["required"], true);
    assert_eq!(connector["kind"], "string");
}

#[tokio::test]
async fn channel_call_without_channel_or_logic_is_rejected() {
    let app = test_app().await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "broken-channel-call",
                "tasks": [{
                    "id": "t1",
                    "name": "call",
                    "function": { "name": "channel_call", "input": {} }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    let details = body["error"]["details"].as_array().unwrap();
    assert!(
        details
            .iter()
            .any(|d| d["path"] == "tasks[0].function.input" && d["code"] == "REQUIRED"),
        "should report channel_call needs channel/channel_logic, got {body:?}"
    );
}
