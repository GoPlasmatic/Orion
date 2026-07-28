//! Multi-node (cluster mode) integration tests — multi-instance-ha M2.
//!
//! Each test builds N full `AppState`s (nodes) sharing one Postgres
//! testcontainer and one Redis testcontainer, with cluster mode on and a
//! fast epoch poll. This binary is separate from `tests/integration`
//! because the storage backend is pinned per process (`DB_BACKEND` OnceLock)
//! and the integration binary pins SQLite; here every test uses Postgres.
//!
//! Containers are per-test on purpose: the outage tests pause/stop theirs,
//! and the shared stores (config_epoch, channels, dedup keys) make each test
//! order-independent even under `--test-threads=1`.
//!
//! Poll-budget policy: every cross-node assertion goes through `eventually`
//! / `eventually_n` with a total budget of at least 25× the slowest involved
//! watcher interval; bare sleeps are allowed only for rate-limit window
//! alignment.
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

/// One cluster node: full `AppState`, its router, and the background-task
/// handles (epoch watcher included) so teardown can stop them.
struct Node {
    state: AppState,
    router: axum::Router,
    handles: orion::bootstrap::TaskHandles,
}

struct ClusterHarness {
    /// The outage tests pause/unpause this.
    pg: testcontainers::ContainerAsync<Postgres>,
    /// Kept live for the test's duration; D15's tests also pause/unpause it.
    redis: testcontainers::ContainerAsync<Redis>,
    redis_url: String,
    nodes: Vec<Node>,
    /// The FASTEST epoch poll interval any node runs at.
    poll: Duration,
}

impl Drop for ClusterHarness {
    fn drop(&mut self) {
        // Abort the epoch watchers: without this, every finished test leaked
        // watchers polling its destroyed Postgres every 200ms (a 3s connect
        // timeout each) for the rest of the serial binary run. The worker and
        // persistence tasks exit on their own when the states drop and their
        // queue senders close.
        for node in &self.nodes {
            for handle in &node.handles.cluster_task_handles {
                handle.abort();
            }
        }
    }
}

/// Back-compat wrapper for the original two-node tests: named accessors over
/// `ClusterHarness` (which `Deref` exposes for `redis` / `redis_url` / `poll`
/// / `pg` access).
struct TwoNodeHarness {
    cluster: ClusterHarness,
    state_a: AppState,
    state_b: AppState,
    node_a: axum::Router,
    node_b: axum::Router,
}

impl std::ops::Deref for TwoNodeHarness {
    type Target = ClusterHarness;
    fn deref(&self) -> &ClusterHarness {
        &self.cluster
    }
}

async fn two_nodes() -> TwoNodeHarness {
    two_nodes_with(|_| {}).await
}

async fn two_nodes_with(customize: impl Fn(&mut orion::config::AppConfig)) -> TwoNodeHarness {
    let cluster = cluster_nodes_with(2, customize, |_, _| {}).await;
    TwoNodeHarness {
        state_a: cluster.nodes[0].state.clone(),
        state_b: cluster.nodes[1].state.clone(),
        node_a: cluster.nodes[0].router.clone(),
        node_b: cluster.nodes[1].router.clone(),
        cluster,
    }
}

/// Retry `check` once per epoch-poll interval (25 tries ≈ 5 s at the harness
/// poll rate); `true` as soon as it passes. The shared retry budget for
/// "node B eventually observes node A's change" assertions.
async fn eventually(poll: Duration, check: impl AsyncFnMut() -> bool) -> bool {
    eventually_n(poll, 25, check).await
}

/// `eventually` with an explicit budget — for tests whose slowest watcher
/// polls slower than the harness default (e.g. the stale-node test).
async fn eventually_n(
    interval: Duration,
    tries: usize,
    mut check: impl AsyncFnMut() -> bool,
) -> bool {
    for _ in 0..tries {
        tokio::time::sleep(interval).await;
        if check().await {
            return true;
        }
    }
    false
}

/// Build an N-node cluster over one Postgres + one Redis container.
/// `shared` customizes the config every node gets; `per_node(i, cfg)` runs
/// after it for node-specific overrides (e.g. a slow epoch poll).
async fn cluster_nodes_with(
    n: usize,
    shared: impl Fn(&mut orion::config::AppConfig),
    per_node: impl Fn(usize, &mut orion::config::AppConfig),
) -> ClusterHarness {
    let pg = Postgres::default().start().await.expect("start postgres");
    let pg_port = pg.get_host_port_ipv4(5432).await.expect("pg port");
    let redis = Redis::default().start().await.expect("start redis");
    let redis_port = redis.get_host_port_ipv4(6379).await.expect("redis port");

    let poll = Duration::from_millis(200);
    let redis_url = format!("redis://127.0.0.1:{redis_port}");
    let mut config = orion::config::AppConfig::default();
    config.storage.url = format!("postgres://postgres:postgres@127.0.0.1:{pg_port}/postgres");
    config.cluster.enabled = true;
    config.cluster.redis_url = redis_url.clone();
    config.cluster.epoch_poll_interval_ms = poll.as_millis() as u64;
    shared(&mut config);

    let mut nodes = Vec::with_capacity(n);
    for i in 0..n {
        let mut cfg = config.clone();
        cfg.cluster.instance_id = format!("node-{}", (b'a' + i as u8) as char);
        per_node(i, &mut cfg);
        let (state, mut handles) = common::test_state_with_handles(cfg).await;
        handles.cluster_task_handles = orion::cluster::start_cluster_tasks(&state);
        nodes.push(Node {
            router: orion::server::build_router(state.clone()),
            state,
            handles,
        });
    }

    ClusterHarness {
        pg,
        redis,
        redis_url,
        nodes,
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

    let entry = repo_a
        .enqueue("trace-x", "orders", "{}", "{}", "boom", 0, 5)
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
    let (a, b) = (a.expect("claim a"), b.expect("claim b"));
    assert_eq!(a.len() + b.len(), 1, "exactly one node must claim the row");
    let loser = if a.is_empty() { "node-a" } else { "node-b" };

    // While the winner's lease is live, the loser must stay locked out.
    let blocked = if a.is_empty() { &repo_a } else { &repo_b };
    assert!(
        blocked
            .claim_pending(loser, 10, 60)
            .await
            .expect("claim under live lease")
            .is_empty(),
        "a live lease must block re-claims"
    );

    // Once the lease expires (a crashed winner), the other node re-claims.
    sqlx::query("UPDATE trace_dlq SET claimed_until = LOCALTIMESTAMP - interval '1 seconds'")
        .execute(pg)
        .await
        .expect("expire lease");
    let reclaimed = blocked
        .claim_pending(loser, 10, 60)
        .await
        .expect("re-claim expired lease");
    assert_eq!(
        reclaimed.len(),
        1,
        "an expired lease must be re-claimable by the other node"
    );
    assert_eq!(reclaimed[0].id, entry.id);
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

/// D15: the shared Redis handle must re-establish itself. A dropped
/// connection used to be permanent — shared dedup (failing open, so
/// duplicates flow silently), the shared response cache, and cluster rate
/// limiting stayed broken on every node until the pods restarted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn cluster_redis_recovers_after_connection_drop() {
    let h = two_nodes().await;

    common::create_and_activate_channel_with_config(
        &h.node_a,
        "reconnect-ch",
        common::simple_log_workflow("Reconnect WF"),
        json!({"deduplication": {"header": "Idempotency-Key", "window_secs": 300}}),
    )
    .await;

    let payload = json!({"data": {"k": "v"}});
    let first_ok = eventually(h.poll, async || {
        let resp = h
            .node_a
            .clone()
            .oneshot(post_with_idempotency_key(
                "/api/v1/data/reconnect-ch",
                "reconnect-token",
                payload.clone(),
            ))
            .await
            .unwrap();
        resp.status() == StatusCode::OK
    })
    .await;
    assert!(first_ok, "node A must serve the channel");

    // Sever every client connection from the server side — the same
    // observable event as a Redis restart, without the port remapping a
    // container restart would cause. SKIPME (default) spares this client.
    let client = redis::Client::open(h.redis_url.as_str()).expect("redis client");
    let mut killer = client
        .get_multiplexed_async_connection()
        .await
        .expect("killer connection");
    let _: i64 = redis::cmd("CLIENT")
        .arg("KILL")
        .arg("TYPE")
        .arg("normal")
        .query_async(&mut killer)
        .await
        .expect("client kill");

    // Dedup must come back on its own. A stale handle never recovers, so the
    // replay would be served as new (dedup fails open) for every poll.
    let deduped = eventually(h.poll, async || {
        let resp = h
            .node_b
            .clone()
            .oneshot(post_with_idempotency_key(
                "/api/v1/data/reconnect-ch",
                "reconnect-token",
                payload.clone(),
            ))
            .await
            .unwrap();
        resp.status() == StatusCode::CONFLICT
    })
    .await;
    assert!(
        deduped,
        "shared dedup must recover after the Redis connection is dropped"
    );
}

/// D15: a node that cannot reach the cluster Redis must leave the LB
/// rotation instead of silently serving with dedup and rate limiting off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn readyz_reflects_cluster_redis_availability() {
    let h = two_nodes().await;

    let readyz = async |node: axum::Router| {
        let resp = node
            .oneshot(json_request("GET", "/readyz", None))
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    };

    let (status, body) = readyz(h.node_a.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["components"]["cluster_redis"], "ok");

    h.redis.pause().await.expect("pause redis");
    let mut unready = (StatusCode::OK, json!(null));
    for _ in 0..5 {
        unready = readyz(h.node_a.clone()).await;
        if unready.0 == StatusCode::SERVICE_UNAVAILABLE {
            break;
        }
    }
    assert_eq!(
        unready.0,
        StatusCode::SERVICE_UNAVAILABLE,
        "unreachable cluster Redis must fail readiness: {}",
        unready.1
    );
    assert_eq!(unready.1["components"]["cluster_redis"], "error");

    h.redis.unpause().await.expect("unpause redis");
    let mut recovered = StatusCode::SERVICE_UNAVAILABLE;
    for _ in 0..10 {
        recovered = readyz(h.node_a.clone()).await.0;
        if recovered == StatusCode::OK {
            break;
        }
        tokio::time::sleep(h.poll).await;
    }
    assert_eq!(
        recovered,
        StatusCode::OK,
        "readiness must return once Redis is reachable again"
    );
}

/// Propagation beyond activation: archive and delete ride the same epoch bus
/// (`audit_and_reload` → bump; watcher → `resync_from_db`). Workflow-status
/// and rollout mutations use the identical path, so channel archive/delete
/// is the representative pin.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn archive_and_delete_propagate_across_nodes() {
    let h = two_nodes().await;

    let (arch_id, _) = common::create_and_activate_channel_full(
        &h.node_a,
        "arch-ch",
        common::simple_log_workflow("Archive WF"),
        json!({}),
    )
    .await;
    let (del_id, _) = common::create_and_activate_channel_full(
        &h.node_a,
        "del-ch",
        common::simple_log_workflow("Delete WF"),
        json!({}),
    )
    .await;

    let payload = json!({"data": {"x": 1}});
    let data = |node: axum::Router, ch: &'static str, payload: serde_json::Value| async move {
        node.oneshot(json_request(
            "POST",
            &format!("/api/v1/data/{ch}"),
            Some(payload),
        ))
        .await
        .unwrap()
        .status()
    };

    // Baseline: node B serves both.
    for ch in ["arch-ch", "del-ch"] {
        let served = eventually(h.poll, async || {
            data(h.node_b.clone(), ch, payload.clone()).await == StatusCode::OK
        })
        .await;
        assert!(served, "node B must serve {ch} before the mutation phase");
    }

    // Archive one; archive-then-delete the other (delete requires archived,
    // same as the single-node admin flow).
    let resp = h
        .node_a
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{arch_id}/status"),
            Some(json!({"status": "archived"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = h
        .node_a
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{del_id}/status"),
            Some(json!({"status": "archived"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = h
        .node_a
        .clone()
        .oneshot(json_request(
            "DELETE",
            &format!("/api/v1/admin/channels/{del_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Node A stops serving immediately (local reload precedes the bump)...
    for ch in ["arch-ch", "del-ch"] {
        assert_eq!(
            data(h.node_a.clone(), ch, payload.clone()).await,
            StatusCode::NOT_FOUND,
            "{ch} must 404 on node A immediately after the mutation"
        );
    }

    // ...and node B within the poll budget.
    for ch in ["arch-ch", "del-ch"] {
        let gone = eventually(h.poll, async || {
            data(h.node_b.clone(), ch, payload.clone()).await == StatusCode::NOT_FOUND
        })
        .await;
        assert!(gone, "node B must stop serving {ch} within the poll budget");
    }
}

/// T6 — connector mutation propagation, the bug this release claims to fix:
/// a connector config updated through node A must take effect on node B,
/// which requires BOTH the connector-registry reload and the
/// `sql_pool_cache.evict_all()` inside `resync_from_db`; without the
/// eviction node B keeps answering from a pool built for the old config.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn connector_update_propagates_to_other_node() {
    use sqlx::Connection;

    let h = two_nodes().await;

    // Two named shared-cache in-memory SQLite DBs (visible to both in-process
    // "nodes"); the keeper connections hold them alive for the whole test.
    let old_url = "sqlite:file:conn-prop-old?mode=memory&cache=shared";
    let new_url = "sqlite:file:conn-prop-new?mode=memory&cache=shared";
    let mut keep_old = sqlx::sqlite::SqliteConnection::connect(old_url)
        .await
        .expect("old db");
    let mut keep_new = sqlx::sqlite::SqliteConnection::connect(new_url)
        .await
        .expect("new db");
    for (conn, val) in [(&mut keep_old, "old"), (&mut keep_new, "new")] {
        sqlx::query("CREATE TABLE t (val TEXT)")
            .execute(&mut *conn)
            .await
            .expect("create");
        sqlx::query("INSERT INTO t VALUES ($1)")
            .bind(val)
            .execute(&mut *conn)
            .await
            .expect("seed");
    }

    common::create_connector(&h.node_a, common::db_connector_sqlite("prop-conn", old_url)).await;
    common::create_and_activate_channel(
        &h.node_a,
        "conn-prop-ch",
        common::workflow_with_tasks(
            "Connector Prop WF",
            json!([{
                "id": "t1",
                "name": "Read val",
                "function": {
                    "name": "db_read",
                    "input": {
                        "connector": "prop-conn",
                        "query": "SELECT val FROM t",
                        "output": "data.read_result"
                    }
                }
            }]),
        ),
    )
    .await;

    let read_val = |node: axum::Router| async move {
        let resp = node
            .oneshot(json_request(
                "POST",
                "/api/v1/data/conn-prop-ch",
                Some(json!({"data": {}})),
            ))
            .await
            .unwrap();
        if resp.status() != StatusCode::OK {
            return None;
        }
        let body = body_json(resp).await;
        body["data"]["read_result"][0]["val"]
            .as_str()
            .map(str::to_string)
    };

    // Baseline: both nodes read "old" through the connector.
    let baseline = eventually(h.poll, async || {
        read_val(h.node_b.clone()).await.as_deref() == Some("old")
    })
    .await;
    assert!(
        baseline,
        "node B must serve the channel with the old config"
    );
    assert_eq!(read_val(h.node_a.clone()).await.as_deref(), Some("old"));

    // Repoint the connector at the new DB via node A.
    let resp = h
        .node_a
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/connectors/prop-conn",
            Some(json!({
                "config": {
                    "type": "db",
                    "connection_string": new_url,
                    "max_connections": 1,
                    "query_timeout_ms": 5000
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "connector update must succeed"
    );

    // Node A serves "new" immediately (local registry reload + pool eviction).
    assert_eq!(read_val(h.node_a.clone()).await.as_deref(), Some("new"));

    // The T6 pin: node B must serve "new" within the poll budget.
    let propagated = eventually(h.poll, async || {
        read_val(h.node_b.clone()).await.as_deref() == Some("new")
    })
    .await;
    assert!(
        propagated,
        "the connector update must propagate to node B (registry reload + pool eviction)"
    );
}

/// Concurrent admin writes on different nodes converge: the epoch bump is an
/// atomic single-row UPDATE and `last_seen_epoch` uses `fetch_max`, so
/// neither node's bump can mask the other's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn concurrent_admin_writes_converge() {
    let h = two_nodes().await;
    let start = h
        .state_a
        .cluster
        .repo
        .get_epoch()
        .await
        .expect("epoch")
        .epoch;

    tokio::join!(
        common::create_and_activate_channel(
            &h.node_a,
            "conc-a",
            common::simple_log_workflow("Conc A WF"),
        ),
        common::create_and_activate_channel(
            &h.node_b,
            "conc-b",
            common::simple_log_workflow("Conc B WF"),
        ),
    );

    // Both nodes must converge on serving BOTH channels.
    let payload = json!({"data": {"x": 1}});
    for (name, ch, node) in [
        ("node A", "conc-b", &h.node_a),
        ("node B", "conc-a", &h.node_b),
        ("node A", "conc-a", &h.node_a),
        ("node B", "conc-b", &h.node_b),
    ] {
        let served = eventually(h.poll, async || {
            node.clone()
                .oneshot(json_request(
                    "POST",
                    &format!("/api/v1/data/{ch}"),
                    Some(payload.clone()),
                ))
                .await
                .unwrap()
                .status()
                == StatusCode::OK
        })
        .await;
        assert!(served, "{name} must serve {ch} after the concurrent writes");
    }

    // Workflow + channel activation each bump: two per pair, four total.
    let end = h
        .state_a
        .cluster
        .repo
        .get_epoch()
        .await
        .expect("epoch")
        .epoch;
    assert!(
        end >= start + 4,
        "expected >= 4 epoch bumps (2 per activation pair), got {start} -> {end}"
    );
}

/// N-generality: nothing in src/cluster assumes two nodes (D4 — no node
/// registry); only the old harness was hard-wired. Three nodes over one
/// Postgres + Redis, activation via node 0, nodes 1 and 2 both serve.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn three_node_activation_propagates() {
    let h = cluster_nodes_with(3, |_| {}, |_, _| {}).await;

    common::create_and_activate_channel(
        &h.nodes[0].router,
        "tri-ch",
        common::simple_log_workflow("Tri WF"),
    )
    .await;

    let payload = json!({"data": {"x": 1}});
    for i in 1..3 {
        let served = eventually(h.poll, async || {
            h.nodes[i]
                .router
                .clone()
                .oneshot(json_request(
                    "POST",
                    "/api/v1/data/tri-ch",
                    Some(payload.clone()),
                ))
                .await
                .unwrap()
                .status()
                == StatusCode::OK
        })
        .await;
        assert!(
            served,
            "node {i} must serve the channel activated via node 0"
        );
        assert!(
            h.nodes[i]
                .state
                .channel_registry
                .get_by_name("tri-ch")
                .await
                .is_some(),
            "node {i}'s registry (not just routing fallback) must have the channel"
        );
    }
}

/// A11 — breaker-reset fan-out, end to end: breaker STATE is node-local by
/// design (D3), but one admin reset call fans out over `breaker_epoch` so
/// every node resets its local copy. Phase 2 pins the documented M2
/// deviation: `breaker_key` is a single last-write-wins slot, so two resets
/// inside one poll window fan out only the LAST key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn breaker_reset_fans_out_across_nodes() {
    use std::sync::atomic::Ordering;

    let h = two_nodes_with(|config| {
        config.engine.circuit_breaker = orion::connector::circuit_breaker::CircuitBreakerConfig {
            enabled: true,
            // One failure trips; a 3600s cooldown prevents half-open
            // flapping mid-test.
            failure_threshold: 1,
            recovery_timeout_secs: 3600,
            max_breakers: 10_000,
        };
    })
    .await;

    let addr = common::start_failing_server().await;
    common::create_http_connector(&h.node_a, "cb-fan", addr).await;
    common::create_and_activate_channel(
        &h.node_a,
        "cb-fan-ch",
        common::failing_http_workflow("CB Fan WF", "cb-fan"),
    )
    .await;

    let data = |node: axum::Router, ch: &'static str| async move {
        node.oneshot(json_request(
            "POST",
            &format!("/api/v1/data/{ch}"),
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap()
        .status()
    };
    let breakers_on = |node: axum::Router| async move {
        let resp = node
            .oneshot(json_request(
                "GET",
                "/api/v1/admin/connectors/circuit-breakers",
                None,
            ))
            .await
            .unwrap();
        body_json(resp).await
    };
    let breaker_state = |body: &serde_json::Value, connector: &str| {
        body["breakers"]
            .as_object()
            .and_then(|m| m.iter().find(|(k, _)| k.contains(connector)))
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
    };

    // Wait until node B serves the channel; that first served request
    // returns 500 from the mock and trips node B's breaker (threshold 1).
    // Then trip node A's — a reset 404s on a node without the key.
    let served = eventually(h.poll, async || {
        data(h.node_b.clone(), "cb-fan-ch").await != StatusCode::NOT_FOUND
    })
    .await;
    assert!(served, "node B must serve the channel");
    assert_eq!(
        data(h.node_a.clone(), "cb-fan-ch").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );

    // Node B's breaker is open: admin view agrees and the data plane sheds.
    let body = breakers_on(h.node_b.clone()).await;
    let (key, state) =
        breaker_state(&body, "cb-fan").unwrap_or_else(|| panic!("no cb-fan breaker in {body}"));
    assert_eq!(state, "open", "node B breaker must be open: {body}");
    assert_eq!(
        data(h.node_b.clone(), "cb-fan-ch").await,
        StatusCode::SERVICE_UNAVAILABLE,
        "an open breaker must shed on the data plane"
    );

    // Reset via node A's admin API → fans out over breaker_epoch.
    let resp = h
        .node_a
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/admin/connectors/circuit-breakers/{key}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "reset via node A must succeed"
    );

    // The fan-out pin: node B's breaker closes within the poll budget...
    let closed = eventually(h.poll, async || {
        breaker_state(&breakers_on(h.node_b.clone()).await, "cb-fan")
            .is_some_and(|(_, s)| s == "closed")
    })
    .await;
    assert!(closed, "node A's reset must close node B's breaker");
    // ...and node B's data plane reaches the upstream again: 500 from the
    // mock, not a 503 shed. (This re-trips node B's breaker — used below.)
    assert_eq!(
        data(h.node_b.clone(), "cb-fan-ch").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );

    // ---- Phase 2: the last-write-wins coalescing pin -------------------
    common::create_http_connector(&h.node_a, "cb-fan2", addr).await;
    common::create_and_activate_channel(
        &h.node_a,
        "cb-fan2-ch",
        common::failing_http_workflow("CB Fan2 WF", "cb-fan2"),
    )
    .await;
    let tripped = eventually(h.poll, async || {
        data(h.node_b.clone(), "cb-fan2-ch").await == StatusCode::INTERNAL_SERVER_ERROR
    })
    .await;
    assert!(tripped, "node B must serve and trip the second channel");
    let (key2, _) = breaker_state(&breakers_on(h.node_b.clone()).await, "cb-fan2")
        .expect("second breaker must exist on node B");

    let e0 = h
        .state_b
        .cluster
        .last_seen_breaker_epoch
        .load(Ordering::Acquire);
    // Two resets landing microseconds apart — inside one poll window.
    // Driven at repo level so the race against node B's 200ms tick is
    // microscopic; going through HTTP would widen it.
    h.state_a
        .cluster
        .repo
        .request_breaker_reset(&key)
        .await
        .expect("reset 1");
    h.state_a
        .cluster
        .repo
        .request_breaker_reset(&key2)
        .await
        .expect("reset 2");

    let consumed = eventually(h.poll, async || {
        h.state_b
            .cluster
            .last_seen_breaker_epoch
            .load(Ordering::Acquire)
            >= e0 + 2
    })
    .await;
    assert!(consumed, "node B's watcher must consume both breaker bumps");

    // PINS the documented M2 deviation: only the LAST key was applied; the
    // first reset was coalesced away. If this fails because BOTH closed,
    // the coalescing was fixed — update the scalability doc and this test
    // together.
    let body = breakers_on(h.node_b.clone()).await;
    assert_eq!(
        body["breakers"][&key2].as_str(),
        Some("closed"),
        "the last reset key must be applied: {body}"
    );
    let key1_state = body["breakers"][&key].as_str().unwrap_or_default();
    assert_eq!(
        key1_state, "open",
        "the coalesced (first) reset is lost — documented last-write-wins: {body}"
    );
}

/// The Redis degradation window, pinned: while the cluster Redis is down,
/// dedup and per-channel rate limits FAIL OPEN (availability over strict
/// idempotency/limits) and `/readyz` withdraws the node so the LB pulls it.
/// Uses a hard container STOP: the port closes and commands fail fast with
/// ECONNREFUSED — the error class the guards fail open on. `pause()` would
/// black-hole instead: the redis `ConnectionManager` has no response
/// timeout, so a paused Redis HANGS the guard rather than erroring.
/// (Post-B4 finding, deliberately not fixed here:
/// `ConnectionManagerConfig::set_response_timeout` would convert the hang
/// class into the error class.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn redis_outage_window_fails_open() {
    let h = two_nodes_with(|config| {
        // The per-channel limiter only mounts when platform limiting is on;
        // keep platform limits far above the per-channel limit under test.
        config.rate_limit.enabled = true;
        config.rate_limit.default_rps = 10_000;
        config.rate_limit.default_burst = 10_000;
    })
    .await;

    common::create_and_activate_channel_with_config(
        &h.node_a,
        "outage-ch",
        common::simple_log_workflow("Outage WF"),
        json!({
            "deduplication": {"header": "Idempotency-Key", "window_secs": 300},
            "rate_limit": {"requests_per_second": 5, "burst": 0}
        }),
    )
    .await;

    // Baseline: node A consumes the key; node B's replay is a 409 through
    // the live shared Redis.
    let payload = json!({"data": {"k": "v"}});
    let first_ok = eventually(h.poll, async || {
        h.node_a
            .clone()
            .oneshot(post_with_idempotency_key(
                "/api/v1/data/outage-ch",
                "outage-token",
                payload.clone(),
            ))
            .await
            .unwrap()
            .status()
            == StatusCode::OK
    })
    .await;
    assert!(first_ok, "node A must serve the channel");
    let replayed = eventually(h.poll, async || {
        h.node_b
            .clone()
            .oneshot(post_with_idempotency_key(
                "/api/v1/data/outage-ch",
                "outage-token",
                payload.clone(),
            ))
            .await
            .unwrap()
            .status()
            == StatusCode::CONFLICT
    })
    .await;
    assert!(
        replayed,
        "shared dedup must 409 the replay while Redis is up"
    );

    h.redis
        .stop_with_timeout(Some(0))
        .await
        .expect("stop redis");

    // PIN 1 — dedup fails open: the duplicate key is accepted on BOTH nodes.
    for (name, node) in [("node A", &h.node_a), ("node B", &h.node_b)] {
        let resp = node
            .clone()
            .oneshot(post_with_idempotency_key(
                "/api/v1/data/outage-ch",
                "outage-token",
                payload.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "{name}: dedup must fail open during the Redis outage"
        );
    }

    // PIN 2 — the 5 rps channel limit is fully bypassed: 30 rapid keyless
    // requests all pass.
    for i in 0..30 {
        let node = if i % 2 == 0 { &h.node_a } else { &h.node_b };
        let resp = node
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/data/outage-ch",
                Some(payload.clone()),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "rate limits must fail open during the Redis outage (request {i})"
        );
    }

    // PIN 3 — containment: /readyz withdraws the node.
    let mut status = StatusCode::OK;
    for _ in 0..5 {
        status = h
            .node_a
            .clone()
            .oneshot(json_request("GET", "/readyz", None))
            .await
            .unwrap()
            .status();
        if status == StatusCode::SERVICE_UNAVAILABLE {
            break;
        }
        tokio::time::sleep(h.poll).await;
    }
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "readyz must withdraw while the cluster Redis is down"
    );

    // No recovery phase on purpose: a container restart can remap the
    // published port while the ConnectionManager holds the original URL.
    // Recovery is already pinned by cluster_redis_recovers_after_connection_drop
    // (CLIENT KILL) and readyz_reflects_cluster_redis_availability
    // (pause/unpause).
}

/// The Postgres outage window + stale-node semantics, pinned in one pause
/// cycle. There is NO fencing anywhere by design (retired HA plan D1): a
/// node whose resync fails keeps serving its last-applied engine, and the
/// containment is `/readyz` withdrawing every node so the LB stops sending
/// traffic. Node B runs a 5s epoch poll so "archive on A, pause before B's
/// next tick" is anchored to an observed tick with a ~25x safety ratio —
/// the guard assertion detects the pathological miss loudly instead of
/// letting the test pass vacuously.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn postgres_outage_stale_node_keeps_serving_and_recovers() {
    use std::sync::atomic::Ordering;

    let h = cluster_nodes_with(
        2,
        |_| {},
        |i, cfg| {
            if i == 1 {
                cfg.cluster.epoch_poll_interval_ms = 5_000;
            }
        },
    )
    .await;
    let node_a = h.nodes[0].router.clone();
    let node_b = h.nodes[1].router.clone();
    let state_b = h.nodes[1].state.clone();

    let (ch_id, _) = common::create_and_activate_channel_full(
        &node_a,
        "stale-ch",
        common::simple_log_workflow("Stale WF"),
        json!({}),
    )
    .await;

    // Baseline: node B serves (budget covers its 5s poll).
    let payload = json!({"data": {"x": 1}});
    let served = eventually_n(Duration::from_millis(500), 30, async || {
        node_b
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/data/stale-ch",
                Some(payload.clone()),
            ))
            .await
            .unwrap()
            .status()
            == StatusCode::OK
    })
    .await;
    assert!(served, "node B must serve the baseline channel");
    let e_b = state_b.cluster.last_seen_epoch.load(Ordering::Acquire);

    // Archive via node A, then immediately pause Postgres — inside node B's
    // ~5s tick window (the archive+pause pair takes tens of milliseconds).
    let resp = node_a
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{ch_id}/status"),
            Some(json!({"status": "archived"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    h.pg.pause().await.expect("pause postgres");
    assert_eq!(
        state_b.cluster.last_seen_epoch.load(Ordering::Acquire),
        e_b,
        "node B ticked inside the archive->pause gap (a ~25x-margin race); re-run"
    );

    // Every outage-window request is bounded by an outer timeout: DB
    // failures are bounded by the 3s pool-acquire timeout, and a hang must
    // fail loudly rather than stall the suite.
    let deadline = Duration::from_secs(15);

    // PIN 1 — availability over consistency: node B keeps serving its
    // last-applied engine (200) even though the channel is archived in the
    // DB. The 200 also pins "trace persistence down != data plane down":
    // the sync trace write fails after ~3s and is swallowed with a warning.
    let resp = tokio::time::timeout(
        deadline,
        node_b.clone().oneshot(json_request(
            "POST",
            "/api/v1/data/stale-ch",
            Some(payload.clone()),
        )),
    )
    .await
    .expect("node B data request must not hang")
    .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the stale node must keep serving during the outage (no fencing, by design)"
    );

    // PIN 2 — the split view: node A already applied the archive → 404.
    let resp = tokio::time::timeout(
        deadline,
        node_a.clone().oneshot(json_request(
            "POST",
            "/api/v1/data/stale-ch",
            Some(payload.clone()),
        )),
    )
    .await
    .expect("node A data request must not hang")
    .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // PIN 3 — containment: /readyz withdraws BOTH nodes; stale serving only
    // happens behind an LB that has not reacted yet.
    for (name, node) in [("node A", &node_a), ("node B", &node_b)] {
        let resp = tokio::time::timeout(
            deadline,
            node.clone().oneshot(json_request("GET", "/readyz", None)),
        )
        .await
        .expect("readyz must not hang")
        .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{name} readyz must withdraw during the outage"
        );
        let body = body_json(resp).await;
        assert_eq!(body["components"]["database"], "error", "{name}: {body}");
    }

    // PIN 4 — admin mutations fail loudly (the repo write fails first).
    let resp = tokio::time::timeout(
        deadline,
        node_a.clone().oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(common::simple_log_workflow("Outage Draft WF")),
        )),
    )
    .await
    .expect("admin mutation must not hang")
    .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "admin writes must fail during the outage"
    );

    // PIN 5 — a failed resync never advances last_seen.
    assert_eq!(state_b.cluster.last_seen_epoch.load(Ordering::Acquire), e_b);

    // Recovery: node B resyncs on its next successful tick and applies the
    // archive; readiness and the epoch bus come back.
    h.pg.unpause().await.expect("unpause postgres");
    let archived = eventually_n(Duration::from_millis(500), 40, async || {
        node_b
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/data/stale-ch",
                Some(payload.clone()),
            ))
            .await
            .unwrap()
            .status()
            == StatusCode::NOT_FOUND
    })
    .await;
    assert!(
        archived,
        "node B must apply the archive once Postgres returns"
    );

    let mut ready = StatusCode::SERVICE_UNAVAILABLE;
    for _ in 0..10 {
        ready = node_a
            .clone()
            .oneshot(json_request("GET", "/readyz", None))
            .await
            .unwrap()
            .status();
        if ready == StatusCode::OK {
            break;
        }
        tokio::time::sleep(h.poll).await;
    }
    assert_eq!(ready, StatusCode::OK, "node A readiness must return");

    common::create_and_activate_channel(
        &node_a,
        "post-outage-ch",
        common::simple_log_workflow("Post Outage WF"),
    )
    .await;
    let served = eventually_n(Duration::from_millis(500), 30, async || {
        node_b
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/data/post-outage-ch",
                Some(payload.clone()),
            ))
            .await
            .unwrap()
            .status()
            == StatusCode::OK
    })
    .await;
    assert!(served, "the epoch bus must be live again after recovery");
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
