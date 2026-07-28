use crate::common;

use crate::common::{body_json, json_request};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

// ============================================================
// Content type & body validation
// ============================================================

#[tokio::test]
async fn test_non_json_content_type_rejected_with_415() {
    let app = common::test_app().await;

    // Non-JSON Content-Type with a body should return 415 Unsupported Media Type.
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/data/orders")
        .header("content-type", "text/plain")
        .body(Body::from(r#"{"data": {"key": "value"}}"#))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn test_completely_invalid_json_body() {
    let app = common::test_app().await;

    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/data/orders")
        .header("content-type", "application/json")
        .body(Body::from("this is not json {{{"))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert!(resp.status().is_client_error());
}

#[tokio::test]
async fn test_empty_body_accepted() {
    let app = common::test_app().await;
    common::create_and_activate_channel(
        &app,
        "empty-body-ch",
        common::simple_log_workflow("Empty Body WF"),
    )
    .await;

    // Empty body is treated as {data: {}, metadata: {}} — valid for GET/DELETE
    // or any request without payload.
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/data/empty-body-ch")
        .header("content-type", "application/json")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert!(resp.status().is_success());
}

/// R13: this endpoint had three behaviours for three body shapes, and only
/// one was documented. `{"data": …}` was the envelope, an empty body became
/// `{"data":{}}`, and a **bare object** — the obvious thing to send, and what
/// every other JSON API accepts — failed with *missing field `data`*. On the
/// most-hit endpoint in the product.
///
/// One rule now: an object carrying `data` or `metadata` is the envelope;
/// anything else is the payload.
#[tokio::test]
async fn a_bare_object_body_is_the_payload() {
    let app = common::test_app().await;
    let wf_id = common::create_and_activate_workflow(&app, common::echo_workflow("Echo")).await;
    common::create_rest_channel(&app, "bare", "/bare", vec!["POST"], &wf_id).await;

    // Bare object: previously a 400.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/bare",
            Some(json!({"amount": 5})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bare = common::body_json(resp).await;

    // …and it means exactly what the explicit envelope means.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/bare",
            Some(json!({"data": {"amount": 5}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let enveloped = common::body_json(resp).await;
    assert_eq!(bare["data"], enveloped["data"], "{bare} vs {enveloped}");

    // A non-object body is a payload too.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/bare",
            Some(json!([1, 2, 3])),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// An envelope naming only `metadata` carries no payload — which is `{}`, the
/// same thing an empty body means.
#[tokio::test]
async fn an_envelope_with_only_metadata_is_accepted() {
    let app = common::test_app().await;
    common::create_and_activate_channel(
        &app,
        "meta-only",
        common::simple_log_workflow("Meta Only"),
    )
    .await;

    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/data/meta-only",
            Some(json!({"metadata": {"source": "test"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ============================================================
// 404 on nonexistent resources
// ============================================================

#[tokio::test]
async fn test_nonexistent_trace_returns_404() {
    let app = common::test_app().await;

    let resp = app
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/traces/nonexistent-trace-id",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_nonexistent_workflow_returns_404() {
    let app = common::test_app().await;

    let resp = app
        .oneshot(json_request(
            "DELETE",
            "/api/v1/admin/workflows/nonexistent-workflow",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_nonexistent_connector_returns_404() {
    let app = common::test_app().await;

    let resp = app
        .oneshot(json_request(
            "DELETE",
            "/api/v1/admin/connectors/nonexistent-connector",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_update_nonexistent_workflow_returns_400() {
    let app = common::test_app().await;

    // update_draft returns BadRequest "No draft version found"
    let resp = app
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/workflows/nonexistent-workflow",
            Some(json!({"name": "Updated Name"})),
        ))
        .await
        .unwrap();

    assert!(resp.status().is_client_error());
}

// ============================================================
// Invalid status transition
// ============================================================

#[tokio::test]
async fn test_invalid_status_transition() {
    let app = common::test_app().await;

    // Create a workflow (draft)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "Status Test Workflow",
                "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    let workflow_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    // Try to set an invalid status
    let resp = app
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{}/status", workflow_id),
            Some(json!({"status": "invalid_status"})),
        ))
        .await
        .unwrap();

    // Invalid enum values are rejected at deserialization by OrionJson with
    // the 400 + field-pathed details envelope every admin body error uses.
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "VALIDATION_ERROR");
}

// ============================================================
// Duplicate resource handling
// ============================================================

#[tokio::test]
async fn test_duplicate_workflow_id_rejected() {
    let app = common::test_app().await;

    let workflow = json!({
        "workflow_id": "duplicate-workflow",
        "name": "First Workflow",
        "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
    });

    // First creation succeeds
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(workflow.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // D16: a duplicate id is a client error, not a 500. On SQLite the
    // single-draft trigger raises a generic error (no UniqueViolation kind),
    // which must still map to 409 with the error envelope.
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(workflow),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "CONFLICT", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("duplicate-workflow"),
        "{body}"
    );
}

/// D16, the other duplicate shape: once the draft is activated there is no
/// draft left for the trigger to reject, so the second create hits the
/// `(workflow_id, version)` primary key instead — a structured
/// `UniqueViolation` — and must map to the same 409.
#[tokio::test]
async fn test_duplicate_of_activated_workflow_returns_conflict() {
    let app = common::test_app().await;

    let workflow = json!({
        "workflow_id": "duplicate-active-workflow",
        "name": "First Workflow",
        "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
    });
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(workflow.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/workflows/duplicate-active-workflow/status",
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(workflow),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "CONFLICT", "{body}");
}

#[tokio::test]
async fn test_duplicate_channel_id_returns_conflict() {
    let app = common::test_app().await;

    let channel = json!({
        "channel_id": "duplicate-channel",
        "name": "first-channel",
        "channel_type": "sync",
        "protocol": "rest",
        "methods": ["POST"],
        "route_pattern": "/dup-ch"
    });
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(channel.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // D16: same contract as workflows — 409 with the envelope, not a 500.
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(channel),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "CONFLICT", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("duplicate-channel"),
        "{body}"
    );
}

#[tokio::test]
async fn test_duplicate_connector_id_returns_conflict() {
    let app = common::test_app().await;

    let connector = json!({
        "id": "dup-conn",
        "name": "First Connector",
        "connector_type": "http",
        "config": {"url": "https://example.com/api", "method": "POST"}
    });

    // First creation succeeds
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(connector),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Second creation with same ID triggers UNIQUE constraint
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "dup-conn",
                "name": "Different Name",
                "connector_type": "http",
                "config": {"url": "https://example.com/api2", "method": "GET"}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

// ============================================================
// Router fallbacks — every non-2xx uses the error envelope (R9)
// ============================================================

#[tokio::test]
async fn unmatched_path_returns_the_error_envelope() {
    // Previously a zero-length body, which broke any client that parses the
    // body on error and contradicted the documented contract.
    let app = common::test_app().await;

    let resp = app
        .oneshot(json_request("GET", "/no/such/path", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(
        resp.headers().contains_key("x-request-id"),
        "the 404 must be correlatable"
    );
    let body = common::body_json(resp).await;
    assert_eq!(body["error"]["code"], "NOT_FOUND");
    assert!(body["error"]["message"].is_string());
}

#[tokio::test]
async fn method_mismatch_returns_405_with_the_error_envelope() {
    let app = common::test_app().await;

    // /healthz is GET-only.
    let resp = app
        .oneshot(json_request("DELETE", "/healthz", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    let body = common::body_json(resp).await;
    assert_eq!(body["error"]["code"], "METHOD_NOT_ALLOWED");
}

// ============================================================
// Bulk import is bounded (R14)
// ============================================================

#[tokio::test]
async fn oversized_import_batch_is_refused_before_any_write() {
    let app = common::test_app().await;

    let items: Vec<serde_json::Value> = (0..1001)
        .map(|i| {
            json!({
                "name": format!("wf-{i}"),
                "tasks": [{"id": "t1", "name": "Log",
                           "function": {"name": "log", "input": {"message": "x"}}}]
            })
        })
        .collect();

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/import",
            Some(json!(items)),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = common::body_json(resp).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("at most 1000 items"),
        "the refusal should state the cap: {body}"
    );

    // Nothing was written.
    let resp = app
        .oneshot(json_request("GET", "/api/v1/admin/workflows", None))
        .await
        .unwrap();
    let body = common::body_json(resp).await;
    assert_eq!(
        body["total"], 0,
        "an over-cap batch must not partially apply"
    );
}

// ============================================================
// R16: the admin body limit is separate from the data-plane one
// ============================================================

/// R16: the body limit was one global layer set from `ingest.max_payload_size`
/// — a name that says *data plane* — so admin bulk import, connector config
/// PUTs and `POST /workflows/{id}/test` shared a ceiling with anonymous channel
/// traffic. Raising it for a big import raised it for the unauthenticated plane
/// too, which is the opposite of what an operator wants.
#[tokio::test]
async fn the_admin_body_limit_is_independent_of_the_data_plane_one() {
    use orion::config::AppConfig;

    let mut config = AppConfig::default();
    // A data plane locked down tight, and an admin API that can still take an
    // import. Under one shared limit this pair is unexpressible.
    config.ingest.max_payload_size = 512;
    config.server.max_admin_body_size = 256 * 1024;

    let app = common::test_app_with_config(config).await;
    common::create_and_activate_channel(&app, "tiny", common::simple_log_workflow("Tiny")).await;

    let big_payload = json!({ "data": { "blob": "x".repeat(4096) } });

    // Data plane: over its 512-byte bound.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/tiny",
            Some(big_payload.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "the data plane must still honour ingest.max_payload_size"
    );

    // Admin: the same bytes, well under its own bound.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "Big Description",
                "description": "y".repeat(2000),
                "tasks": [{"id":"t1","name":"Log",
                           "function":{"name":"log","input":{"message":"x"}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "an admin body over the data-plane limit must still be accepted"
    );
}
