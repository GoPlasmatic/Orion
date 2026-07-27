use crate::common;

use axum::http::StatusCode;
use serde_json::json;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::redis::Redis;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Redis cache integration tests
//
// Each test starts its own ephemeral Redis testcontainer (Docker required),
// mirroring tests/cluster. Run with:
//   cargo test --test integration -- --ignored connector_redis_test
// ---------------------------------------------------------------------------

/// Start a Redis testcontainer and return `(container guard, redis URL)`.
/// The container lives as long as the guard is held.
async fn redis_container() -> (testcontainers::ContainerAsync<Redis>, String) {
    let redis = Redis::default().start().await.expect("start redis");
    let port = redis.get_host_port_ipv4(6379).await.expect("redis port");
    (redis, format!("redis://127.0.0.1:{port}"))
}

/// Write a value to Redis then read it back in the same workflow.
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test integration -- --ignored connector_redis_test"]
async fn test_redis_cache_write_then_read() {
    let app = common::test_app().await;
    let (_redis, redis_url) = redis_container().await;

    common::create_connector(
        &app,
        common::cache_connector_redis("redis-cache", &redis_url),
    )
    .await;

    common::create_and_activate_channel(
        &app,
        "redis-wr",
        common::workflow_with_tasks(
            "RedisCacheWriteRead",
            json!([
                {
                    "id": "t1",
                    "name": "Write to Redis",
                    "function": {
                        "name": "cache_write",
                        "input": {
                            "connector": "redis-cache",
                            "key": "redis-test-key",
                            "value": "hello-redis"
                        }
                    }
                },
                {
                    "id": "t2",
                    "name": "Read from Redis",
                    "function": {
                        "name": "cache_read",
                        "input": {
                            "connector": "redis-cache",
                            "key": "redis-test-key",
                            "output": "data.cached"
                        }
                    }
                }
            ]),
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/redis-wr",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["data"]["cached"], "hello-redis");
}

/// Write a value with a short TTL, read immediately (present), wait for
/// expiry, then read again (null).
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test integration -- --ignored connector_redis_test"]
async fn test_redis_cache_ttl_expiry() {
    let app = common::test_app().await;
    let (_redis, redis_url) = redis_container().await;

    common::create_connector(&app, common::cache_connector_redis("redis-ttl", &redis_url)).await;

    // Channel that writes a key with 1-second TTL
    common::create_and_activate_channel(
        &app,
        "redis-ttl-write",
        common::workflow_with_tasks(
            "RedisTTLWrite",
            json!([
                {
                    "id": "t1",
                    "name": "Write with TTL",
                    "function": {
                        "name": "cache_write",
                        "input": {
                            "connector": "redis-ttl",
                            "key": "ephemeral-redis",
                            "value": "short-lived",
                            "ttl_secs": 1
                        }
                    }
                }
            ]),
        ),
    )
    .await;

    // Channel that reads the same key
    common::create_and_activate_channel(
        &app,
        "redis-ttl-read",
        common::workflow_with_tasks(
            "RedisTTLRead",
            json!([
                {
                    "id": "t1",
                    "name": "Read ephemeral key",
                    "function": {
                        "name": "cache_read",
                        "input": {
                            "connector": "redis-ttl",
                            "key": "ephemeral-redis",
                            "output": "data.val"
                        }
                    }
                }
            ]),
        ),
    )
    .await;

    // Write the key
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/redis-ttl-write",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Read immediately -- should be present
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/redis-ttl-read",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["data"]["val"], "short-lived");

    // Wait for the TTL to expire
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Read again -- should be null
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/redis-ttl-read",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert!(body["data"]["val"].is_null());
}

use crate::common::post_with_idempotency_key;

/// Channel with Redis-backed deduplication: sending the same idempotency key
/// twice should return 200 then 409.
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test integration -- --ignored connector_redis_test"]
async fn test_redis_dedup_via_check_and_insert() {
    let app = common::test_app().await;
    let (_redis, redis_url) = redis_container().await;

    // Create the Redis cache connector that backs the dedup store
    common::create_connector(
        &app,
        common::cache_connector_redis("redis-dedup", &redis_url),
    )
    .await;

    common::create_and_activate_channel_with_config(
        &app,
        "redis-dedup-ch",
        common::simple_log_workflow("Redis Dedup WF"),
        json!({
            "deduplication": {
                "header": "Idempotency-Key",
                "window_secs": 300,
                "connector": "redis-dedup"
            }
        }),
    )
    .await;

    let payload = json!({"data": {"k": "v"}});

    // First request -- should succeed
    let resp = app
        .clone()
        .oneshot(post_with_idempotency_key(
            "/api/v1/data/redis-dedup-ch",
            "redis-dedup-key-001",
            payload.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Second request with the same key -- should be rejected as duplicate
    let resp = app
        .clone()
        .oneshot(post_with_idempotency_key(
            "/api/v1/data/redis-dedup-ch",
            "redis-dedup-key-001",
            payload.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = common::body_json(resp).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Duplicate"),
        "Expected error message to contain 'Duplicate', got: {}",
        body["error"]["message"]
    );
}
