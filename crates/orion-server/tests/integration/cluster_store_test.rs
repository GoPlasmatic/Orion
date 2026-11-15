//! An edition's cluster store, composed into a node with
//! `cluster.enabled = true`: dedup claims, the response cache and per-channel
//! rate windows go through it rather than the state database, and `/readyz`
//! reports it as its own component.
//!
//! The node runs on in-memory SQLite: what is under test is that the shared
//! hot-path state reaches the composed store, which a database with one
//! node can show as well as a fleet.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use crate::common::{
    self, body_json, create_and_activate_channel_with_config, echo_workflow, json_request,
    post_with_idempotency_key, simple_log_workflow,
};

pub(crate) use crate::memory_store::CountingStore;

/// The tests' in-memory store keeps the contract every cluster store does,
/// so what the tests below and the multi-node suite prove over it holds for
/// a real one.
#[tokio::test]
async fn the_in_memory_store_keeps_the_store_contract() {
    orion::testing::shared_store_contract(Arc::new(CountingStore::default())).await;
}

/// A cluster node on in-memory SQLite whose shared store is `store`.
async fn node_over(store: Arc<CountingStore>) -> orion::runtime::state::AppState {
    let mut config = orion::testing::test_config();
    config.cluster.enabled = true;
    // The per-channel limiter only runs behind the platform limit layer;
    // the platform limits stay far above what these tests send.
    config.rate_limit.enabled = true;
    config.rate_limit.default_rps = 10_000;
    config.rate_limit.default_burst = 10_000;
    common::test_state_with_composition(
        config,
        orion::extensions::CompositionRoot::community().with_cluster_store(store),
    )
    .await
}

#[tokio::test]
async fn a_dedup_replay_is_refused_through_the_composed_store() {
    let store = Arc::new(CountingStore::default());
    let state = node_over(store.clone()).await;
    assert_eq!(state.cluster.shared.as_ref().unwrap().name(), "fake");
    let app = orion::server::build_router(state);
    create_and_activate_channel_with_config(
        &app,
        "store-dedup",
        simple_log_workflow("Store Dedup WF"),
        json!({"deduplication": {"header": "Idempotency-Key", "window_secs": 300}}),
    )
    .await;

    let payload = json!({"data": {"k": "v"}});
    let first = app
        .clone()
        .oneshot(post_with_idempotency_key(
            "/api/v2/data/store-dedup",
            "store-token",
            payload.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let replay = app
        .clone()
        .oneshot(post_with_idempotency_key(
            "/api/v2/data/store-dedup",
            "store-token",
            payload,
        ))
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::CONFLICT);
    assert_eq!(store.claims_won.load(Ordering::SeqCst), 1);
    assert_eq!(store.claim_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_cached_response_is_served_from_the_composed_store() {
    let store = Arc::new(CountingStore::default());
    let app = orion::server::build_router(node_over(store.clone()).await);
    create_and_activate_channel_with_config(
        &app,
        "store-cache",
        echo_workflow("Store Cache WF"),
        // A namespaced channel reads its counter and entry in one get_many.
        json!({"cache": {
            "enabled": true,
            "ttl_secs": 300,
            "cache_key_fields": ["user_id"],
            "namespaces": ["catalog"]
        }}),
    )
    .await;

    let sets_before = store.sets.load(Ordering::SeqCst);
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/store-cache",
            Some(json!({"data": {"user_id": "u1", "origin": "first"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"]["echo"]["origin"], "first");
    // The store write may complete after the response: wait for it.
    for _ in 0..100 {
        if store.sets.load(Ordering::SeqCst) > sets_before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(store.sets.load(Ordering::SeqCst) > sets_before);

    // Same key (`user_id`), another origin: a hit replays the first body.
    let reads_before = store.get_manys.load(Ordering::SeqCst);
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/store-cache",
            Some(json!({"data": {"user_id": "u1", "origin": "second"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["data"]["echo"]["origin"], "first",
        "the second request is the cached first response: {body}"
    );
    assert!(store.get_manys.load(Ordering::SeqCst) > reads_before);

    // Invalidating the namespace moves its counter in the same store, which
    // retires the entry.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/admin/cache/namespaces/catalog/invalidate",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/store-cache",
            Some(json!({"data": {"user_id": "u1", "origin": "third"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["data"]["echo"]["origin"], "third",
        "an invalidated entry is not served: {body}"
    );
}

#[tokio::test]
async fn a_rate_limited_channel_counts_in_the_composed_store() {
    let store = Arc::new(CountingStore::default());
    let app = orion::server::build_router(node_over(store.clone()).await);
    create_and_activate_channel_with_config(
        &app,
        "store-rl",
        simple_log_workflow("Store RL WF"),
        json!({"rate_limit": {"requests_per_second": 1, "burst": 0}}),
    )
    .await;

    // A window of one: three requests in well under a second span at most
    // two windows, so at least one is refused.
    let mut statuses = Vec::new();
    for _ in 0..3 {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v2/data/store-rl",
                Some(json!({"data": {"x": 1}})),
            ))
            .await
            .unwrap();
        statuses.push(resp.status());
    }
    assert_eq!(store.window_hits.load(Ordering::SeqCst), 3);
    assert_eq!(statuses[0], StatusCode::OK, "{statuses:?}");
    assert!(
        statuses.contains(&StatusCode::TOO_MANY_REQUESTS),
        "{statuses:?}"
    );
}

#[tokio::test]
async fn readyz_reports_the_composed_store_and_fails_without_it() {
    let store = Arc::new(CountingStore::default());
    let app = orion::server::build_router(node_over(store.clone()).await);

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/readyz", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["components"]["cluster_store"],
        json!({"name": "fake", "status": "ok"}),
        "{body}"
    );

    store.down.store(true, Ordering::SeqCst);
    let resp = app
        .oneshot(json_request("GET", "/readyz", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "not_ready");
    assert_eq!(
        body["components"]["cluster_store"],
        json!({"name": "fake", "status": "error"}),
        "{body}"
    );
    assert_eq!(body["components"]["database"], "ok");
}

#[tokio::test]
async fn a_store_that_does_not_answer_at_boot_refuses_the_node() {
    let store = Arc::new(CountingStore::default());
    store.down.store(true, Ordering::SeqCst);
    let mut config = orion::testing::test_config();
    config.cluster.enabled = true;
    let Err(refused) = orion::testing::TestNode::start(
        config,
        orion::extensions::CompositionRoot::community().with_cluster_store(store),
    )
    .await
    else {
        panic!("a node over an unreachable store must not boot");
    };
    let message = refused.to_string();
    assert!(
        message.contains("cluster store 'fake'") && message.contains("down"),
        "{message}"
    );
}
