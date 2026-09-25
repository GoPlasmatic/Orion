use crate::common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

/// Write a value to the cache then read it back in the same workflow.
#[tokio::test]
async fn test_cache_write_then_read() {
    let app = common::test_app().await;

    common::create_connector(&app, common::cache_connector_memory("test-cache")).await;

    common::create_and_activate_channel(
        &app,
        "cache-wr",
        common::workflow_with_tasks(
            "CacheWriteRead",
            json!([
                {
                    "id": "t1",
                    "name": "Write greeting",
                    "function": {
                        "name": "cache_write",
                        "input": {
                            "connector": "test-cache",
                            "key": "greeting",
                            "value": "hello"
                        }
                    }
                },
                {
                    "id": "t2",
                    "name": "Read greeting",
                    "function": {
                        "name": "cache_read",
                        "input": {
                            "connector": "test-cache",
                            "key": "greeting",
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
            "/api/v1/data/cache-wr",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["data"]["cached"], "hello");
}

/// Reading a key that was never written should produce null.
#[tokio::test]
async fn test_cache_read_missing_key() {
    let app = common::test_app().await;

    common::create_connector(&app, common::cache_connector_memory("miss-cache")).await;

    common::create_and_activate_channel(
        &app,
        "cache-miss",
        common::workflow_with_tasks(
            "CacheReadMissing",
            json!([
                {
                    "id": "t1",
                    "name": "Read absent key",
                    "function": {
                        "name": "cache_read",
                        "input": {
                            "connector": "miss-cache",
                            "key": "does-not-exist",
                            "output": "data.result"
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
            "/api/v1/data/cache-miss",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert!(body["data"]["result"].is_null());
}

/// Write a key with a TTL, verify it is readable immediately, then verify it
/// has expired after advancing the (pausable) clock past the TTL.
///
/// The TTL is 300s and expiry is driven by pause–advance–resume rather than
/// a real sleep: the store's clock is `tokio::time::Instant`, and the large
/// TTL keeps incidental timer auto-advance from ever crossing it early.
#[tokio::test]
async fn test_cache_write_with_ttl_expiry() {
    let app = common::test_app().await;

    common::create_connector(&app, common::cache_connector_memory("ttl-cache")).await;

    // Channel that writes a key with a 300-second TTL
    common::create_and_activate_channel(
        &app,
        "cache-ttl-write",
        common::workflow_with_tasks(
            "CacheTTLWrite",
            json!([
                {
                    "id": "t1",
                    "name": "Write with TTL",
                    "function": {
                        "name": "cache_write",
                        "input": {
                            "connector": "ttl-cache",
                            "key": "ephemeral",
                            "value": "short-lived",
                            "ttl_secs": 300
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
        "cache-ttl-read",
        common::workflow_with_tasks(
            "CacheTTLRead",
            json!([
                {
                    "id": "t1",
                    "name": "Read ephemeral key",
                    "function": {
                        "name": "cache_read",
                        "input": {
                            "connector": "ttl-cache",
                            "key": "ephemeral",
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
            "/api/v1/data/cache-ttl-write",
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
            "/api/v1/data/cache-ttl-read",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["data"]["val"], "short-lived");

    // Advance the paused clock past the TTL — no wall time burned.
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(301)).await;
    tokio::time::resume();

    // Read again -- should be null
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/cache-ttl-read",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert!(body["data"]["val"].is_null());
}

/// Write a JSON object as the cache value and verify it round-trips correctly.
#[tokio::test]
async fn test_cache_json_value_round_trip() {
    let app = common::test_app().await;

    common::create_connector(&app, common::cache_connector_memory("json-cache")).await;

    common::create_and_activate_channel(
        &app,
        "cache-json",
        common::workflow_with_tasks(
            "CacheJsonRoundTrip",
            json!([
                {
                    "id": "t1",
                    "name": "Write JSON value",
                    "function": {
                        "name": "cache_write",
                        "input": {
                            "connector": "json-cache",
                            "key": "profile",
                            "value": {"user": "alice", "score": 42}
                        }
                    }
                },
                {
                    "id": "t2",
                    "name": "Read JSON value",
                    "function": {
                        "name": "cache_read",
                        "input": {
                            "connector": "json-cache",
                            "key": "profile",
                            "output": "data.profile"
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
            "/api/v1/data/cache-json",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["data"]["profile"]["user"], "alice");
    assert_eq!(body["data"]["profile"]["score"], 42);
}

/// Write the same key twice with different values, then read. The second
/// write should overwrite the first.
#[tokio::test]
async fn test_cache_overwrite_key() {
    let app = common::test_app().await;

    common::create_connector(&app, common::cache_connector_memory("ow-cache")).await;

    // Channel that writes value "A"
    common::create_and_activate_channel(
        &app,
        "cache-ow-write-a",
        common::workflow_with_tasks(
            "CacheOverwriteA",
            json!([
                {
                    "id": "t1",
                    "name": "Write A",
                    "function": {
                        "name": "cache_write",
                        "input": {
                            "connector": "ow-cache",
                            "key": "k",
                            "value": "A"
                        }
                    }
                }
            ]),
        ),
    )
    .await;

    // Channel that writes value "B"
    common::create_and_activate_channel(
        &app,
        "cache-ow-write-b",
        common::workflow_with_tasks(
            "CacheOverwriteB",
            json!([
                {
                    "id": "t1",
                    "name": "Write B",
                    "function": {
                        "name": "cache_write",
                        "input": {
                            "connector": "ow-cache",
                            "key": "k",
                            "value": "B"
                        }
                    }
                }
            ]),
        ),
    )
    .await;

    // Channel that reads
    common::create_and_activate_channel(
        &app,
        "cache-ow-read",
        common::workflow_with_tasks(
            "CacheOverwriteRead",
            json!([
                {
                    "id": "t1",
                    "name": "Read k",
                    "function": {
                        "name": "cache_read",
                        "input": {
                            "connector": "ow-cache",
                            "key": "k",
                            "output": "data.val"
                        }
                    }
                }
            ]),
        ),
    )
    .await;

    // First write: key=k, value=A
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/cache-ow-write-a",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Second write: key=k, value=B (overwrites A)
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/cache-ow-write-b",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Read: should get B
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/cache-ow-read",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["data"]["val"], "B");
}

/// Referencing a connector that does not exist should produce an error.
#[tokio::test]
async fn test_cache_missing_connector_error() {
    let app = common::test_app().await;

    // R5 blocks activating against a missing connector, so create it first
    // and delete it afterwards — the one way this state is still reachable.
    let conn_id =
        common::create_connector(&app, common::cache_connector_memory("nonexistent-cache")).await;
    common::create_and_activate_channel(
        &app,
        "cache-bad-conn",
        common::workflow_with_tasks(
            "CacheMissingConnector",
            json!([
                {
                    "id": "t1",
                    "name": "Read from missing connector",
                    "function": {
                        "name": "cache_read",
                        "input": {
                            "connector": "nonexistent-cache",
                            "key": "anything",
                            "output": "data.result"
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
            "DELETE",
            &format!("/api/v1/admin/connectors/{conn_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/cache-bad-conn",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();

    // The engine returns a 500 with ENGINE_ERROR because the connector is not found
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = common::body_json(resp).await;
    assert_eq!(body["error"]["code"], "ENGINE_ERROR");
}

/// #354: the generation-counter pattern end to end. A write bumps the
/// counter; a read embeds the generation it read in its own key and fetches
/// several keys in one `cache_read`; `cache_delete` drops exact keys.
#[tokio::test]
async fn cache_incr_keys_and_delete_compose_a_generation_counter() {
    let app = common::test_app().await;
    common::create_connector(&app, common::cache_connector_memory("gen-cache")).await;

    common::create_and_activate_channel(
        &app,
        "gen-flow",
        common::workflow_with_tasks(
            "GenFlow",
            json!([
                {"id": "bump1", "name": "bump", "function": {"name": "cache_incr", "input": {
                    "connector": "gen-cache", "key": "gen:ladder", "output": "data.g1"}}},
                {"id": "bump2", "name": "bump by 5", "function": {"name": "cache_incr", "input": {
                    "connector": "gen-cache", "key": "gen:ladder", "by": 5, "ttl_secs": 60,
                    "output": "data.g2"}}},
                {"id": "silent", "name": "bump, no output", "function": {"name": "cache_incr", "input": {
                    "connector": "gen-cache", "key": "gen:season"}}},
                {"id": "store", "name": "store entry", "function": {"name": "cache_write", "input": {
                    "connector": "gen-cache",
                    "key": {"cat": ["ladder:v", {"var": "data.g2"}]},
                    "value": {"top": "ada"}}}},
                {"id": "read", "name": "read several", "function": {"name": "cache_read", "input": {
                    "connector": "gen-cache",
                    "keys": ["gen:ladder", "gen:season", {"cat": ["ladder:v", {"var": "data.g2"}]}, "absent"],
                    "output": "data.many"}}},
                {"id": "del", "name": "drop", "function": {"name": "cache_delete", "input": {
                    "connector": "gen-cache",
                    "keys": [{"cat": ["ladder:v", {"var": "data.g2"}]}, "absent"],
                    "output": "data.deleted"}}},
                {"id": "reread", "name": "read after delete", "function": {"name": "cache_read", "input": {
                    "connector": "gen-cache",
                    "key": {"cat": ["ladder:v", {"var": "data.g2"}]},
                    "output": "data.after"}}}
            ]),
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/gen-flow",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["data"]["g1"], 1);
    assert_eq!(body["data"]["g2"], 6);
    assert_eq!(body["data"]["many"], json!([6, 1, {"top": "ada"}, null]));
    assert_eq!(body["data"]["deleted"], json!({"deleted": 1}));
    assert!(body["data"]["after"].is_null());
}

/// `cache_read` takes `key` or `keys`, never both and never neither — refused
/// when the workflow is created, not when a message reaches the task.
#[tokio::test]
async fn cache_read_requires_exactly_one_of_key_and_keys() {
    let app = common::test_app().await;
    common::create_connector(&app, common::cache_connector_memory("kk-cache")).await;
    for (id, input) in [
        (
            "both",
            json!({"connector": "kk-cache", "key": "a", "keys": ["b"]}),
        ),
        ("neither", json!({"connector": "kk-cache"})),
    ] {
        let resp = app
            .clone()
            .oneshot(common::json_request(
                "POST",
                "/api/v1/admin/workflows",
                Some(common::workflow_with_tasks(
                    id,
                    json!([{"id": "t", "name": "t", "function": {"name": "cache_read", "input": input}}]),
                )),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{id}");
    }
}
