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
async fn test_redis_dedup_rejects_a_replayed_idempotency_key() {
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

/// N16: the same idempotency claim, exercised against the backend the guard
/// actually uses in production.
///
/// The guard's unit tests run against in-memory stubs, so nothing checked
/// that Redis implements the contract the claim depends on. That contract is
/// no longer "is this key new?" — a bare boolean cannot distinguish a second
/// delivery from a redelivery of the *same* one, and treating the latter as a
/// duplicate is how a Kafka record gets committed without ever running. So
/// `claim_dedup_key` reports **who** holds the key, and `remove` hands it
/// back; `SET NX EX` + `GET` and `DEL` are what implement that here.
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test integration -- --ignored connector_redis_test"]
async fn test_redis_dedup_claim_reports_the_holder_and_can_be_released() {
    use orion::connector::cache_backend::{CacheBackend, RedisCacheBackend};

    let (_redis, redis_url) = redis_container().await;
    let client = redis::Client::open(redis_url.as_str()).expect("redis client");
    let conn = client
        .get_connection_manager()
        .await
        .expect("redis connection");
    let backend = RedisCacheBackend::new(conn);

    let key = "dedup:orders:ORD-77";

    // A free key is claimed, and the claim is silent about any holder.
    assert_eq!(
        backend
            .claim_dedup_key(key, "kafka:orders/0/7", 300)
            .await
            .expect("claim"),
        None
    );

    // A *different* delivery presenting the same key is told who holds it, so
    // the guard can see the holder is not itself and refuse with 409.
    assert_eq!(
        backend
            .claim_dedup_key(key, "kafka:orders/0/12", 300)
            .await
            .expect("claim"),
        Some("kafka:orders/0/7".to_string()),
        "a held key must name its holder, not merely report 'taken'"
    );

    // The *same* delivery coming back — an in-place retry, or a redelivery of
    // an offset that was never committed — reads its own token and proceeds.
    assert_eq!(
        backend
            .claim_dedup_key(key, "kafka:orders/0/7", 300)
            .await
            .expect("claim"),
        Some("kafka:orders/0/7".to_string()),
        "a redelivery must be able to recognise its own unsettled claim"
    );

    // Releasing an unsettled delivery frees the key for whoever comes next.
    backend.remove(key).await.expect("release");
    assert_eq!(
        backend
            .claim_dedup_key(key, "kafka:orders/0/12", 300)
            .await
            .expect("claim"),
        None,
        "a released key must be claimable again"
    );

    // Releasing a key nobody holds is not an error — the settle path runs on
    // every outcome, including ones that never claimed anything.
    backend
        .remove("dedup:orders:never-claimed")
        .await
        .expect("remove absent key");
}

/// T16: the fixed-window limiter's own doc comment names the trade — "up to
/// 2x burst at a window boundary" — and nothing pinned it. The window
/// semantics are the contract multi-node rate limiting rests on: within one
/// window the limit holds exactly, and across one boundary at most 2x passes.
/// This drives the backend directly (the cluster test aligns to a fresh
/// window on purpose and tolerates the boundary; this test *is* the
/// boundary).
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test integration -- --ignored connector_redis_test"]
async fn redis_fixed_window_holds_per_window_and_spills_at_most_2x_at_the_boundary() {
    use orion::channel::{RateLimitBackend, RedisRateLimitBackend};

    let (_redis, url) = redis_container().await;
    let client = redis::Client::open(url).expect("redis client");
    let conn = redis::aio::ConnectionManager::new(client)
        .await
        .expect("redis connection");
    // limit_per_window = rps + burst = 3.
    let backend = RedisRateLimitBackend::new(conn, "t16-rollover".into(), 3, 0);

    // Land just after a second boundary, so the in-window burst below cannot
    // straddle one by accident.
    align_to_fresh_window().await;

    for i in 0..3 {
        assert!(
            backend.check("caller".into()).await.expect("check"),
            "request {i} within the window's limit must pass"
        );
    }
    assert!(
        !backend.check("caller".into()).await.expect("check"),
        "the 4th request in one window must be refused"
    );

    // Cross into the next window. 3 more pass — 6 total in well under two
    // seconds: the documented up-to-2x boundary spill, no more.
    align_to_fresh_window().await;
    for i in 0..3 {
        assert!(
            backend.check("caller".into()).await.expect("check"),
            "request {i} in the next window must pass (the 2x spill)"
        );
    }
    assert!(
        !backend.check("caller".into()).await.expect("check"),
        "the spill is bounded: the 4th request of the second window must be refused"
    );
}

/// Sleep until shortly after the next second boundary (the backend's window
/// edge), leaving ~800ms of headroom for the requests that follow.
async fn align_to_fresh_window() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock");
    let into_window = u64::from(now.subsec_millis());
    tokio::time::sleep(std::time::Duration::from_millis(1000 - into_window + 60)).await;
}
