use crate::common;

use crate::common::{body_json, json_request};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

// ============================================================
// SQL injection resistance
// ============================================================

/// Upload a v1-shaped workflow body as a workflow definition, answering its
/// digest.
async fn upload_workflow(app: &axum::Router, workflow: serde_json::Value) -> String {
    common::deploy::upload(app, common::deploy::workflow_definition(&workflow)).await
}

/// The stored definition a digest names.
async fn read_definition(app: &axum::Router, digest: &str) -> serde_json::Value {
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v2/admin/definitions/{digest}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

/// A workflow whose name is SQL uploads, reads back by its digest intact, and
/// deploys and serves: nothing interpreted it.
#[tokio::test]
async fn test_sql_injection_in_workflow_name() {
    let app = common::test_app().await;

    let malicious_name = "'; DROP TABLE workflows;--";
    let workflow = json!({
        "name": malicious_name,
        "condition": true,
        "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
    });

    // Verify the definition is stored safely and retrievable
    let digest = upload_workflow(&app, workflow.clone()).await;
    let stored = read_definition(&app, &digest).await;
    assert_eq!(stored["definition"]["name"], malicious_name);

    // …and that it deploys and serves.
    common::create_and_activate_channel(&app, "sqli-name-ch", workflow).await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v2/data/sqli-name-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_sql_injection_in_audit_log_filter() {
    let app = common::test_app().await;

    // One audited mutation, so an injected filter that matched everything
    // would have something to return.
    upload_workflow(&app, common::simple_log_workflow("Audited Workflow")).await;
    common::wait_for_body(&app, "/api/v2/admin/audit-logs", |b| b["total"] == 1).await;

    // Attempt SQL injection via a filter (percent-encoded: ' OR 1=1 --)
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v2/admin/audit-logs?resource_type=%27%20OR%201%3D1%20--",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    // The injection must not return every row: no resource type matches the payload.
    assert_eq!(body["total"], 0, "{body}");
}

// ============================================================
// XSS resistance
// ============================================================

#[tokio::test]
async fn test_xss_in_workflow_description() {
    let app = common::test_app().await;

    let xss_payload = "<script>alert('xss')</script>";

    let digest = upload_workflow(
        &app,
        json!({
            "name": "XSS Test Workflow",
            "description": xss_payload,
            "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
        }),
    )
    .await;

    // Verify the description is stored and returned as-is (JSON-escaped, not interpreted)
    let stored = read_definition(&app, &digest).await;
    assert_eq!(stored["definition"]["description"], xss_payload);
}

// ============================================================
// Payload size & depth limits
// ============================================================

#[tokio::test]
async fn test_deeply_nested_json_payload() {
    let app = common::test_app().await;
    common::create_and_activate_channel(
        &app,
        "deep-json-ch",
        common::simple_log_workflow("Deep JSON WF"),
    )
    .await;

    // Build a 100-level deep nested JSON structure
    let mut nested = json!({"leaf": true});
    for _ in 0..100 {
        nested = json!({"nested": nested});
    }

    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v2/data/deep-json-ch",
            Some(json!({"data": nested})),
        ))
        .await
        .unwrap();

    // 100 levels is inside serde_json's 128-level recursion limit: the
    // payload parses and the request completes normally.
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
}

/// Past serde_json's 128-level recursion limit the payload must be refused
/// as a client error — a controlled rejection, not a stack overflow (which
/// would abort the process and fail the whole test binary).
#[tokio::test]
async fn test_json_past_recursion_limit_is_rejected_as_client_error() {
    let app = common::test_app().await;

    let mut nested = json!({"leaf": true});
    for _ in 0..200 {
        nested = json!({"nested": nested});
    }
    // Serialize manually: the request body string itself is fine to build,
    // only deserialization on the server enforces the depth limit.
    let body_string = serde_json::to_string(&json!({"data": nested})).unwrap();
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v2/data/orders")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body_string))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
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

    let digest = upload_workflow(
        &app,
        json!({
            "name": emoji_name,
            "description": cjk_description,
            "tags": unicode_tags,
            "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
        }),
    )
    .await;

    // Verify roundtrip
    let stored = read_definition(&app, &digest).await;
    let def = &stored["definition"];
    assert_eq!(def["name"], emoji_name);
    assert_eq!(def["description"], cjk_description);
    let tags = def["tags"].as_array().unwrap();
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

    let (status, body) = common::deploy::try_upload(
        &app,
        json!({"kind": "workflow", "definition": {
            "name": name_with_null,
            "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}]
        }}),
    )
    .await;

    // System should either accept safely or reject -- must not panic
    if status.is_success() {
        // Verify retrieval is consistent
        let digest = body["digest"].as_str().unwrap();
        let stored = read_definition(&app, digest).await;
        assert_eq!(stored["definition"]["name"], name_with_null);
    } else {
        assert!(status.is_client_error(), "{status}: {body}");
    }
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
        .oneshot(json_request("GET", "/api/v2/admin/engine/status", None))
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
        .uri("/api/v2/admin/engine/status")
        .header("Authorization", "Bearer wrong-key")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "UNAUTHORIZED");
}

// ------------------------------------------------------------
// Middleware ordering contract (proposal S16)
//
// `Router::layer` wraps, so the LAST layer added is the OUTERMOST. Getting
// this backwards silently removes controls rather than failing loudly, so the
// consequences are pinned here rather than left to review.
// ------------------------------------------------------------

#[tokio::test]
async fn test_401_carries_security_headers_and_request_id() {
    // The 401 is produced by admin auth, which returns without calling
    // `next.run`. When the security-header and request-id layers sat *inside*
    // it, this response escaped with neither.
    let mut config = orion::config::AppConfig::default();
    config.admin_auth.enabled = true;
    config.admin_auth.api_keys = vec!["a-sufficiently-long-test-secret-key-000".to_string()];
    let app = common::test_app_with_config(config).await;

    let resp = app
        .oneshot(json_request("GET", "/api/v2/admin/engine/status", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        resp.headers()
            .get("x-content-type-options")
            .map(|v| v.to_str().unwrap()),
        Some("nosniff"),
        "security headers must wrap the 401"
    );
    assert!(
        resp.headers().contains_key("x-frame-options"),
        "security headers must wrap the 401"
    );
    assert!(
        resp.headers().contains_key("x-request-id"),
        "the 401 must be correlatable"
    );

    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "UNAUTHORIZED");
    assert!(
        body["error"]["request_id"].is_string(),
        "the error envelope must carry the request id, got {body}"
    );
}

#[tokio::test]
async fn test_admin_preflight_is_answered_not_rejected() {
    // A browser preflight is sent without credentials by definition. With CORS
    // layered inside admin auth, every preflight to an admin route 401'd and
    // the admin API was unusable from a browser whenever auth was on.
    let mut config = orion::config::AppConfig::default();
    config.admin_auth.enabled = true;
    config.admin_auth.api_keys = vec!["a-sufficiently-long-test-secret-key-000".to_string()];
    let app = common::test_app_with_config(config).await;

    let req = Request::builder()
        .method("OPTIONS")
        .uri("/api/v2/admin/definitions")
        .header("Origin", "https://console.example.com")
        .header("Access-Control-Request-Method", "GET")
        // A browser calling an authenticated admin route sends this. Omitting
        // it is why the wildcard/`Authorization` defect below survived: the
        // preflight passed because it never asked for anything.
        .header(
            "Access-Control-Request-Headers",
            "authorization, content-type",
        )
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "preflight must be answered by the CORS layer, not rejected by admin auth"
    );
    assert!(
        resp.headers().contains_key("access-control-allow-origin"),
        "preflight response must carry CORS headers, got {:?}",
        resp.headers()
    );

    // The default config is `allowed_origins = ["*"]`, which used to take the
    // `CorsLayer::permissive()` branch and answer `Access-Control-Allow-Headers: *`.
    // Per the Fetch Standard `Authorization` is a CORS non-wildcard
    // request-header name, so `*` never covers it and a browser calling the
    // admin API with a bearer token failed preflight on a **default install**.
    let allowed = resp
        .headers()
        .get("access-control-allow-headers")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        allowed.split(',').any(|h| h.trim() == "authorization"),
        "Authorization must be named explicitly; '*' does not authorize it. Got {allowed:?}"
    );
}

#[tokio::test]
async fn test_admin_auth_correct_token_returns_200() {
    let mut config = orion::config::AppConfig::default();
    config.admin_auth.enabled = true;
    config.admin_auth.api_keys = vec!["test-secret-key".to_string()];
    let app = common::test_app_with_config(config).await;

    let req = Request::builder()
        .method("GET")
        .uri("/api/v2/admin/engine/status")
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
        .uri("/api/v2/admin/engine/status")
        .header("Authorization", "Bearer hashed-secret-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Presenting the digest itself must NOT authenticate
    let req = Request::builder()
        .method("GET")
        .uri("/api/v2/admin/engine/status")
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
        .uri("/api/v2/admin/engine/status")
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
        .uri("/api/v2/admin/engine/status")
        .header("Authorization", "Bearer my-api-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Correct custom header → 200
    let req = Request::builder()
        .method("GET")
        .uri("/api/v2/admin/engine/status")
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
            "/api/v2/data/orders",
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
        .oneshot(json_request("GET", "/api/v2/admin/traces", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // With the admin key → 200
    let req = Request::builder()
        .method("GET")
        .uri("/api/v2/admin/traces")
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
        .repos
        .traces
        .create_pending("r12-guarded", None, "async", None, Some(&token_hash))
        .await
        .unwrap();
    let trace_id = trace.id;

    // Missing trace → 404 regardless of credentials.
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v2/admin/traces/some-id", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Real trace, no key and no token → 401 (R12, handler-enforced).
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v2/admin/traces/{trace_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // The admin key grants access without the token…
    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/v2/admin/traces/{trace_id}"))
        .header("Authorization", "Bearer test-secret-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // …and the capability token grants access without the key, so a
    // data-plane caller can poll its own submission under admin auth.
    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/v2/admin/traces/{trace_id}"))
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
        "/api/v2/data/r12-open/async",
        json!({"data": {"secret_payload": "for-my-eyes-only"}}),
    )
    .await;

    // No token → 401 even with auth disabled.
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v2/admin/traces/{trace_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Wrong token → 401.
    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/v2/admin/traces/{trace_id}"))
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
            &format!("/api/v2/admin/traces/{trace_id}?token={token}"),
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
        .oneshot(json_request("GET", "/api/v2/admin/engine/status", None))
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
        .uri("/api/v2/admin/engine/status")
        .header("Authorization", "Bearer primary-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // First rotation key should work
    let req = Request::builder()
        .method("GET")
        .uri("/api/v2/admin/engine/status")
        .header("Authorization", "Bearer rotation-key-1")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Second rotation key should work
    let req = Request::builder()
        .method("GET")
        .uri("/api/v2/admin/engine/status")
        .header("Authorization", "Bearer rotation-key-2")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Unknown key should be rejected
    let req = Request::builder()
        .method("GET")
        .uri("/api/v2/admin/engine/status")
        .header("Authorization", "Bearer unknown-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
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
    // O12: the route exists only when metrics are collected, so this has to
    // ask for them — `AppConfig::default()` leaves them off.
    let mut config = orion::config::AppConfig::default();
    config.metrics.enabled = true;
    let app = common::test_app_with_config(config).await;

    // Process a message first so the exposition has real content
    let _ = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/test-channel",
            Some(json!({
                "data": { "key": "value" },
                "metadata": {}
            })),
        ))
        .await
        .unwrap();

    let resp = app
        .oneshot(json_request("GET", "/metrics", None))
        .await
        .unwrap();
    // The contract under test: no auth required when admin auth is disabled.
    // Exposition *content* is not asserted here — the Prometheus recorder is
    // process-global, so in this shared test binary only the first-created
    // app's handle is wired and every other app renders an empty body.
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        String::from_utf8(bytes.to_vec()).is_ok(),
        "exposition must be valid UTF-8 text"
    );
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

// ============================================================
// S13: read-only admin keys
// ============================================================

/// A read-only key reads the admin plane and cannot mutate it: GET passes,
/// every mutating method answers 403 (not 401 — the credential is valid, its
/// authority is not), and the full-access key on the same install still
/// writes.
#[tokio::test]
async fn test_read_only_key_reads_but_cannot_mutate() {
    let mut config = orion::config::AppConfig::default();
    config.admin_auth.enabled = true;
    config.admin_auth.api_keys = vec!["full-access-key-long-enough-000000".to_string()];
    config.admin_auth.read_only_api_keys = vec!["read-only-key-long-enough-00000000".to_string()];
    let app = common::test_app_with_config(config).await;

    let with_key = |method: &str, uri: &str, key: &str, body: Option<serde_json::Value>| {
        let builder = Request::builder()
            .method(method)
            .uri(uri.to_string())
            .header("Authorization", format!("Bearer {key}"))
            .header("content-type", "application/json");
        match body {
            Some(b) => builder.body(Body::from(serde_json::to_vec(&b).unwrap())),
            None => builder.body(Body::empty()),
        }
        .unwrap()
    };

    // Reads pass.
    let resp = app
        .clone()
        .oneshot(with_key(
            "GET",
            "/api/v2/admin/engine/status",
            "read-only-key-long-enough-00000000",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // A mutation with the read-only key is 403 FORBIDDEN.
    let create = serde_json::json!({"kind": "workflow", "definition": {
        "name": "s13-wf",
        "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"x"}}}]
    }});
    let resp = app
        .clone()
        .oneshot(with_key(
            "POST",
            "/api/v2/admin/definitions",
            "read-only-key-long-enough-00000000",
            Some(create.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "FORBIDDEN");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("read-only"),
        "{body}"
    );

    // The full key on the same install still writes.
    let resp = app
        .clone()
        .oneshot(with_key(
            "POST",
            "/api/v2/admin/definitions",
            "full-access-key-long-enough-000000",
            Some(create),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Applying a deployment with the read-only key: same refusal.
    let resp = app
        .clone()
        .oneshot(with_key(
            "POST",
            "/api/v2/admin/deployments",
            "read-only-key-long-enough-00000000",
            Some(serde_json::json!({
                "idempotency_key": "s13-apply",
                "base_revision": 0,
                "manifest": {"schema_version": 2, "providers": [], "members": []},
                "reason": "read-only key",
                // Refused before it is read: the key may not mutate.
                "reviewed_manifest_digest": format!("sha256:{}", "0".repeat(64)),
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// The `sha256:` at-rest form carries the role too.
#[tokio::test]
async fn test_read_only_role_applies_to_the_digest_form() {
    use sha2::Digest as _;
    let key = "digest-form-read-only-key-0000000000";
    let digest = hex::encode(sha2::Sha256::digest(key.as_bytes()));

    let mut config = orion::config::AppConfig::default();
    config.admin_auth.enabled = true;
    config.admin_auth.api_keys = vec!["full-access-key-long-enough-000000".to_string()];
    config.admin_auth.read_only_api_keys = vec![format!("sha256:{digest}")];
    let app = common::test_app_with_config(config).await;

    let req = Request::builder()
        .method("POST")
        .uri("/api/v2/admin/definitions")
        .header("Authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({"kind": "workflow", "definition": {
                "name": "digest-form-wf",
                "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"x"}}}]
            }}))
            .unwrap(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let req = Request::builder()
        .method("GET")
        .uri("/api/v2/admin/engine/status")
        .header("Authorization", format!("Bearer {key}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
