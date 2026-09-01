use crate::common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use crate::common::{
    body_json, cache_connector_memory, create_and_activate_channel_with_config, create_connector,
    echo_workflow, json_request, workflow_with_tasks,
};

// ============================================================
// 1. Cache hit on same request — identical responses
// ============================================================

#[tokio::test]
async fn test_cache_hit_on_same_request() {
    let app = common::test_app().await;

    create_and_activate_channel_with_config(
        &app,
        "cache-hit-ch",
        echo_workflow("Cache Hit WF"),
        json!({
            "cache": {
                "enabled": true,
                "ttl_secs": 300
            }
        }),
    )
    .await;

    let payload = json!({"data": {"key": "value"}});

    // First request — cache miss, workflow executes
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/cache-hit-ch",
            Some(payload.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_a = body_json(resp).await;
    assert_eq!(body_a["status"], "ok");

    // Second request — same data, should be cache hit
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/cache-hit-ch",
            Some(payload.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_b = body_json(resp).await;

    // Cache hit returns the exact same serialized response
    assert_eq!(body_a, body_b, "Expected identical responses on cache hit");
}

// ============================================================
// 2. Cache miss on different data — different responses
// ============================================================

#[tokio::test]
async fn test_cache_miss_on_different_data() {
    let app = common::test_app().await;

    create_and_activate_channel_with_config(
        &app,
        "cache-miss-ch",
        echo_workflow("Cache Miss WF"),
        json!({
            "cache": {
                "enabled": true,
                "ttl_secs": 300
            }
        }),
    )
    .await;

    // First request with data key "a"
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/cache-miss-ch",
            Some(json!({"data": {"key": "a"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_a = body_json(resp).await;

    // Second request with different data key "b" — cache miss (different hash)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/cache-miss-ch",
            Some(json!({"data": {"key": "b"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_b = body_json(resp).await;

    // The echo workflow reflects input back, so responses should differ
    assert_ne!(
        body_a["data"]["echo"], body_b["data"]["echo"],
        "Expected different echo data for different inputs"
    );
}

// ============================================================
// 3. cache_key_fields scoping — same scoped fields = cache hit
// ============================================================

#[tokio::test]
async fn test_cache_key_fields_scoping() {
    let app = common::test_app().await;

    create_and_activate_channel_with_config(
        &app,
        "cache-fields-ch",
        echo_workflow("Cache Fields WF"),
        json!({
            "cache": {
                "enabled": true,
                "ttl_secs": 300,
                "cache_key_fields": ["user_id"]
            }
        }),
    )
    .await;

    // Request A: user_id = u1, ts = t1
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/cache-fields-ch",
            Some(json!({"data": {"user_id": "u1", "ts": "t1"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_a = body_json(resp).await;

    // Request B: same user_id = u1, different ts = t2
    // Should be a cache hit because cache_key_fields only includes "user_id"
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/cache-fields-ch",
            Some(json!({"data": {"user_id": "u1", "ts": "t2"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_b = body_json(resp).await;

    // Cache hit — response B should be identical to response A (the cached version)
    assert_eq!(
        body_a, body_b,
        "Expected cache hit: same user_id should return cached response"
    );

    // Request C: different user_id = u2 — cache miss
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/cache-fields-ch",
            Some(json!({"data": {"user_id": "u2", "ts": "t1"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_c = body_json(resp).await;

    // Different user_id means different cache key — response C should differ from A
    assert_ne!(
        body_a["data"]["echo"], body_c["data"]["echo"],
        "Expected cache miss: different user_id should produce different response"
    );
}

// ============================================================
// 4. Cache expires after TTL
// ============================================================

#[tokio::test]
async fn test_cache_expires_after_ttl() {
    let app = common::test_app().await;

    create_and_activate_channel_with_config(
        &app,
        "cache-ttl-ch",
        echo_workflow("Cache TTL WF"),
        json!({
            "cache": {
                "enabled": true,
                // 300s + pause–advance–resume instead of a 1s TTL and a real
                // sleep: expiry runs on tokio's clock, and the large TTL
                // keeps timer auto-advance from crossing it early.
                "ttl_secs": 300
            }
        }),
    )
    .await;

    let payload = json!({"data": {"key": "ttl-test"}});

    // First request — populates cache
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/cache-ttl-ch",
            Some(payload.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_a = body_json(resp).await;
    assert_eq!(body_a["status"], "ok");

    // Advance the paused clock past the TTL — no wall time burned.
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(301)).await;
    tokio::time::resume();

    // Same request after expiry — should still return 200 (fresh execution)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/cache-ttl-ch",
            Some(payload.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_b = body_json(resp).await;
    assert_eq!(body_b["status"], "ok");
}

// ============================================================
// 5. Cache with a named memory connector
// ============================================================

#[tokio::test]
async fn test_cache_with_named_connector() {
    let app = common::test_app().await;

    // Create an in-memory cache connector
    create_connector(&app, cache_connector_memory("response-cache")).await;

    create_and_activate_channel_with_config(
        &app,
        "cache-connector-ch",
        echo_workflow("Cache Connector WF"),
        json!({
            "cache": {
                "enabled": true,
                "ttl_secs": 300,
                "connector": "response-cache"
            }
        }),
    )
    .await;

    let payload = json!({"data": {"key": "connector-test"}});

    // First request — cache miss, workflow executes
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/cache-connector-ch",
            Some(payload.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_a = body_json(resp).await;
    assert_eq!(body_a["status"], "ok");

    // Second request — cache hit via the named connector
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/cache-connector-ch",
            Some(payload.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_b = body_json(resp).await;

    // Both responses should be identical (cache hit)
    assert_eq!(
        body_a, body_b,
        "Expected identical responses when using named cache connector"
    );
}

// ============================================================
// N2: error responses are never cached
// ============================================================

/// A transient downstream failure must not be pinned in the response cache
/// and replayed to every caller for the full TTL. The workflow's first
/// request fails (the connector it needs is gone); once the dependency is
/// restored, the very next request must execute rather than serve the
/// cached failure.
#[tokio::test]
async fn test_error_response_is_not_cached() {
    let app = common::test_app().await;

    // Create the connector so activation passes (R5), then delete it so the
    // task fails at runtime — the transient-failure shape N2 is about.
    let conn_id = create_connector(&app, common::db_connector("n2-flaky-db")).await;
    create_and_activate_channel_with_config(
        &app,
        "n2-cache-ch",
        workflow_with_tasks(
            "N2 Failing WF",
            json!([{
                "id": "t1",
                "name": "Read",
                "continue_on_error": true,
                "function": { "name": "db_read", "input": {
                    "connector": "n2-flaky-db",
                    "query": "SELECT 1",
                    "output": "data.rows"
                }}
            }]),
        ),
        json!({ "cache": { "enabled": true, "ttl_secs": 300 } }),
    )
    .await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "DELETE",
            &format!("/api/v1/admin/connectors/{conn_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let payload = json!({"data": {"k": "v"}});

    // First request fails at the task level (continue_on_error → 200 + errors[]).
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/n2-cache-ch",
            Some(payload.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let failed = body_json(resp).await;
    assert!(
        !failed["errors"]
            .as_array()
            .map(Vec::is_empty)
            .unwrap_or(true),
        "expected task errors, got {failed}"
    );

    // Restore the dependency…
    create_connector(&app, common::db_connector("n2-flaky-db")).await;

    // …and the next request must run the workflow again rather than replay
    // the cached failure.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/n2-cache-ch",
            Some(payload),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let after = body_json(resp).await;
    assert!(
        after["errors"]
            .as_array()
            .map(Vec::is_empty)
            .unwrap_or(true),
        "error response was cached and replayed after recovery (N2): {after}"
    );
}

// ============================================================
// A response that sets a cookie is per-caller and never cached (#298)
// ============================================================

/// The cache key is built from the method, path params, query and payload —
/// never from who is calling. So a stored `Set-Cookie` would be replayed to
/// every caller who repeats the request for the TTL, handing them the first
/// caller's session.
///
/// The workflow stamps a counter into the cookie, so a replay is visible: two
/// requests with identical payloads must produce two *different* cookies, which
/// they can only do if the second one re-ran rather than being served from the
/// cache.
#[tokio::test]
async fn a_shaped_response_that_sets_a_cookie_is_not_cached() {
    let app = common::test_app().await;
    create_connector(&app, cache_connector_memory("cookie-cache")).await;

    let workflow = workflow_with_tasks(
        "cookie-session",
        json!([
            {"id": "mint", "name": "Mint", "function": {"name": "map", "input": {"mappings": [
                // `random` is Orion's own operator: a fresh value per run, so
                // a cached replay is detectable rather than merely suspected.
                {"path": "data.sid", "logic": {"random": ["uuid"]}}
            ]}}},
            {"id": "shape", "name": "Shape", "function": {"name": "map", "input": {"mappings": [
                {"path": "data._orion.response.status", "logic": 200},
                {"path": "data._orion.response.cookies", "logic": [
                    {"name": "session", "value": {"var": "data.sid"},
                     "path": "/", "http_only": true}
                ]}
            ]}}}
        ]),
    );

    create_and_activate_channel_with_config(
        &app,
        "cookie-cache-ch",
        workflow,
        json!({
            "response": { "mode": "shaped", "cookies": true },
            "cache": { "enabled": true, "ttl_secs": 300, "connector": "cookie-cache" }
        }),
    )
    .await;

    let payload = json!({"data": {"same": "every time"}});
    let mut cookies = Vec::new();
    for _ in 0..2 {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/data/cookie-cache-ch",
                Some(payload.clone()),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let set = resp
            .headers()
            .get("set-cookie")
            .expect("the response sets a cookie")
            .to_str()
            .unwrap()
            .to_string();
        cookies.push(set);
    }

    assert_ne!(
        cookies[0], cookies[1],
        "the second identical request replayed the first caller's session \
         cookie from the response cache: {cookies:?}"
    );
}
