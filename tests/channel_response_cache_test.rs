mod common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use common::{
    body_json, cache_connector_memory, create_and_activate_channel_with_config, create_connector,
    json_request, workflow_with_tasks,
};

/// A workflow that parses the JSON payload and echoes it back with a marker.
/// This lets us observe the response shape and verify cache behaviour.
fn echo_workflow(name: &str) -> serde_json::Value {
    workflow_with_tasks(
        name,
        json!([
            {
                "id": "parse",
                "name": "Parse payload",
                "function": {
                    "name": "parse_json",
                    "input": { "source": "payload", "target": "input" }
                }
            },
            {
                "id": "echo",
                "name": "Echo input",
                "function": {
                    "name": "map",
                    "input": {
                        "mappings": [
                            {
                                "path": "data.echo",
                                "logic": { "var": "data.input" }
                            },
                            {
                                "path": "data.processed",
                                "logic": true
                            }
                        ]
                    }
                }
            }
        ]),
    )
}

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
                "ttl_secs": 1
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

    // Wait for the 1-second TTL to expire
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

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
