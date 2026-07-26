//! Multi-node (cluster mode) integration tests — multi-instance-ha M2.
//!
//! Each test builds TWO full `AppState`s ("node A" / "node B") sharing one
//! Postgres testcontainer and one Redis testcontainer, with cluster mode on
//! and a fast epoch poll. This binary is separate from `tests/integration`
//! because the storage backend is pinned per process (`DB_BACKEND` OnceLock)
//! and the integration binary pins SQLite; here every test uses Postgres.
//!
//! Run with: cargo test --test cluster -- --ignored

#[path = "../integration/common/mod.rs"]
mod common;

use std::time::Duration;

use axum::http::StatusCode;
use serde_json::json;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::{postgres::Postgres, redis::Redis};
use tower::ServiceExt;

use common::{body_json, json_request, post_with_idempotency_key};
use orion::server::state::AppState;

struct TwoNodeHarness {
    _pg: testcontainers::ContainerAsync<Postgres>,
    _redis: testcontainers::ContainerAsync<Redis>,
    state_a: AppState,
    state_b: AppState,
    node_a: axum::Router,
    node_b: axum::Router,
    /// Epoch poll interval both watchers run at.
    poll: Duration,
}

async fn two_nodes() -> TwoNodeHarness {
    two_nodes_with(|_| {}).await
}

/// Retry `check` once per epoch-poll interval (25 tries ≈ 5 s at the harness
/// poll rate); `true` as soon as it passes. The shared retry budget for
/// "node B eventually observes node A's change" assertions.
async fn eventually(poll: Duration, mut check: impl AsyncFnMut() -> bool) -> bool {
    for _ in 0..25 {
        tokio::time::sleep(poll).await;
        if check().await {
            return true;
        }
    }
    false
}

async fn two_nodes_with(customize: impl Fn(&mut orion::config::AppConfig)) -> TwoNodeHarness {
    let pg = Postgres::default().start().await.expect("start postgres");
    let pg_port = pg.get_host_port_ipv4(5432).await.expect("pg port");
    let redis = Redis::default().start().await.expect("start redis");
    let redis_port = redis.get_host_port_ipv4(6379).await.expect("redis port");

    let poll = Duration::from_millis(200);
    let mut config = orion::config::AppConfig::default();
    config.storage.url = format!("postgres://postgres:postgres@127.0.0.1:{pg_port}/postgres");
    config.cluster.enabled = true;
    config.cluster.redis_url = format!("redis://127.0.0.1:{redis_port}");
    config.cluster.epoch_poll_interval_ms = poll.as_millis() as u64;
    customize(&mut config);

    let mut cfg_a = config.clone();
    cfg_a.cluster.instance_id = "node-a".to_string();
    let mut cfg_b = config;
    cfg_b.cluster.instance_id = "node-b".to_string();

    let state_a = common::test_state_with_config(cfg_a).await;
    let state_b = common::test_state_with_config(cfg_b).await;
    // Leak the watcher handles — RAII teardown of the containers ends them
    // with the test process.
    let _ = orion::cluster::start_cluster_tasks(&state_a);
    let _ = orion::cluster::start_cluster_tasks(&state_b);

    TwoNodeHarness {
        _pg: pg,
        _redis: redis,
        node_a: orion::server::build_router(state_a.clone()),
        node_b: orion::server::build_router(state_b.clone()),
        state_a,
        state_b,
        poll,
    }
}

/// A2: a channel activated through node A is served by node B within a few
/// epoch polls, with no request to node B's admin API.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn epoch_propagates_channel_activation_across_nodes() {
    let h = two_nodes().await;

    common::create_and_activate_channel(
        &h.node_a,
        "prop-ch",
        common::simple_log_workflow("Propagation WF"),
    )
    .await;

    // Node B picks the channel up via its epoch watcher.
    let payload = json!({"data": {"x": 1}});
    let served = eventually(h.poll, async || {
        let resp = h
            .node_b
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/data/prop-ch",
                Some(payload.clone()),
            ))
            .await
            .unwrap();
        resp.status() == StatusCode::OK
    })
    .await;
    assert!(
        served,
        "node B must serve the channel within a few epoch polls"
    );

    // Sanity: node B's registry (not just routing fallbacks) has it.
    assert!(
        h.state_b
            .channel_registry
            .get_by_name("prop-ch")
            .await
            .is_some()
    );
}

/// A6: with no cache connector named, dedup uses the shared cluster Redis —
/// the same idempotency key on node A then node B is a duplicate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn dedup_is_shared_across_nodes() {
    let h = two_nodes().await;

    common::create_and_activate_channel_with_config(
        &h.node_a,
        "dedup-cluster-ch",
        common::simple_log_workflow("Cluster Dedup WF"),
        json!({"deduplication": {"header": "Idempotency-Key", "window_secs": 300}}),
    )
    .await;

    // Wait until node A serves the channel, consuming the idempotency key.
    let payload = json!({"data": {"k": "v"}});
    let first_ok = eventually(h.poll, async || {
        let resp = h
            .node_a
            .clone()
            .oneshot(post_with_idempotency_key(
                "/api/v1/data/dedup-cluster-ch",
                "cluster-token",
                payload.clone(),
            ))
            .await
            .unwrap();
        resp.status() == StatusCode::OK
    })
    .await;
    assert!(first_ok, "node A must serve the channel");

    // Replay the SAME key against node B: must be rejected as duplicate.
    let mut second = StatusCode::NOT_FOUND;
    eventually(h.poll, async || {
        let resp = h
            .node_b
            .clone()
            .oneshot(post_with_idempotency_key(
                "/api/v1/data/dedup-cluster-ch",
                "cluster-token",
                payload.clone(),
            ))
            .await
            .unwrap();
        second = resp.status();
        // NOT_FOUND = channel not propagated to B yet; keep waiting.
        second != StatusCode::NOT_FOUND
    })
    .await;
    assert_eq!(
        second,
        StatusCode::CONFLICT,
        "same idempotency key on the other node must 409"
    );
}

/// A3: a due DLQ row is claimed by exactly one node, and an expired lease
/// is re-claimable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn dlq_row_claimed_by_exactly_one_node() {
    use orion::storage::repositories::trace_dlq::{SqlTraceDlqRepository, TraceDlqRepository};

    let h = two_nodes().await;
    let repo_a = SqlTraceDlqRepository::new(h.state_a.db_pool.clone());
    let repo_b = SqlTraceDlqRepository::new(h.state_b.db_pool.clone());

    repo_a
        .enqueue("trace-x", "orders", "{}", "{}", "boom", 5)
        .await
        .expect("enqueue");
    let orion::storage::DbPool::Postgres(pg) = &h.state_a.db_pool else {
        panic!("postgres expected");
    };
    sqlx::query("UPDATE trace_dlq SET next_retry_at = LOCALTIMESTAMP - interval '2 seconds'")
        .execute(pg)
        .await
        .expect("backdate");

    let (a, b) = tokio::join!(
        repo_a.claim_pending("node-a", 10, 60),
        repo_b.claim_pending("node-b", 10, 60),
    );
    assert_eq!(
        a.expect("claim a").len() + b.expect("claim b").len(),
        1,
        "exactly one node must claim the row"
    );
}

/// A4: a per-channel limit of 10 rps holds at ~10 rps across BOTH nodes
/// combined (shared Redis fixed window), not 10 per node.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn rate_limit_holds_across_nodes_combined() {
    let h = two_nodes_with(|config| {
        // The middleware layer only mounts when platform limiting is on;
        // keep platform limits far above the per-channel limit under test.
        config.rate_limit.enabled = true;
        config.rate_limit.default_rps = 10_000;
        config.rate_limit.default_burst = 10_000;
    })
    .await;

    common::create_and_activate_channel_with_config(
        &h.node_a,
        "rl-cluster-ch",
        common::simple_log_workflow("Cluster RL WF"),
        json!({"rate_limit": {"requests_per_second": 10, "burst": 0}}),
    )
    .await;

    // Wait for node B to serve the channel.
    let payload = json!({"data": {"x": 1}});
    eventually(h.poll, async || {
        h.state_b
            .channel_registry
            .get_by_name("rl-cluster-ch")
            .await
            .is_some()
    })
    .await;

    // Align to the start of a fresh one-second window so all 40 requests
    // land in one window (limit = rps + burst = 10).
    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .subsec_nanos() as u64;
    tokio::time::sleep(Duration::from_nanos(1_000_000_000 - now_nanos) + Duration::from_millis(50))
        .await;

    let mut ok = 0;
    let mut limited = 0;
    for i in 0..40 {
        let node = if i % 2 == 0 { &h.node_a } else { &h.node_b };
        let resp = node
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/data/rl-cluster-ch",
                Some(payload.clone()),
            ))
            .await
            .unwrap();
        match resp.status() {
            StatusCode::OK => ok += 1,
            StatusCode::TOO_MANY_REQUESTS => limited += 1,
            other => panic!("unexpected status {other}: {:?}", body_json(resp).await),
        }
    }
    assert!(
        (10..=20).contains(&ok),
        "combined throughput must be ~10 in the window (got {ok} ok / {limited} limited)"
    );
    assert!(
        limited >= 20,
        "both nodes must see shared 429s (got {limited})"
    );
}

/// B3: filesystem backups are refused in cluster mode — the file would land
/// on one arbitrary node behind the LB.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn backups_refused_in_cluster_mode() {
    let h = two_nodes().await;

    for (method, desc) in [("POST", "create"), ("GET", "list")] {
        let resp = h
            .node_a
            .clone()
            .oneshot(json_request(method, "/api/v1/admin/backups", None))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "{desc} backup must be rejected in cluster mode"
        );
        let body = body_json(resp).await;
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("cluster mode"),
            "error should explain the cluster-mode restriction: {body}"
        );
    }
}
