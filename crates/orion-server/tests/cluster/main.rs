//! Multi-node (cluster mode) integration tests — multi-instance-ha M2.
//!
//! Each test builds N full `AppState`s (nodes) sharing one Postgres
//! testcontainer, with cluster mode on and a fast reconciler poll
//! (`cluster.epoch_poll_interval_ms`). This binary is separate from
//! `tests/integration` by choice: the backend is carried on `DbPool`, so
//! nothing forces the split.
//!
//! Containers are per-test on purpose: the outage tests pause/stop theirs,
//! and the shared stores (deployments, dedup keys) make each test
//! order-independent even under `--test-threads=1`.
//!
//! Poll-budget policy: every cross-node assertion goes through `eventually`
//! / `eventually_n` with a total budget of at least 25× the slowest involved
//! reconciler interval; bare sleeps are allowed only for rate-limit window
//! alignment.
//!
//! Run with: cargo test --test cluster -- --ignored

// Tests unwrap and panic on what they did not expect; the workspace's
// lints against both are for production code.
#![expect(clippy::unwrap_used, clippy::panic)]

#[path = "../integration/common/mod.rs"]
mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tower::ServiceExt;

use common::{body_json, json_request, post_with_idempotency_key};
use orion::runtime::state::AppState;

/// One cluster node: full `AppState`, its router, and the queue drains.
///
/// `_handles` is held, not read: dropping it closes the trace and audit queue
/// senders it owns, so the node's queues would shut down under the test.
/// Teardown stops the *supervised* tasks — the reconciler above all —
/// through `state.tasks`, not through this.
struct Node {
    state: AppState,
    router: axum::Router,
    _handles: orion::bootstrap::TaskHandles,
}

struct ClusterHarness {
    /// The outage tests pause/unpause this.
    pg: testcontainers::ContainerAsync<Postgres>,
    nodes: Vec<Node>,
    /// The FASTEST reconciler poll interval any node runs at.
    poll: Duration,
}

impl Drop for ClusterHarness {
    fn drop(&mut self) {
        // Stop the reconcilers: without this, every finished test leaked
        // loops polling its destroyed Postgres every 200ms (a 3s connect
        // timeout each) for the rest of the serial binary run. The worker and
        // persistence tasks exit on their own when the states drop and their
        // queue senders close.
        //
        // `Drop` cannot await, so this only *signals* the supervisor rather
        // than joining it — which is all that is needed here: a loop wakes
        // at its next tick, sees the signal and returns, instead of running
        // for the rest of the binary. Unlike `JoinHandle::abort()`, a signal
        // never cuts a rebuild mid-publish.
        for node in &self.nodes {
            node.state.tasks.signal_shutdown();
        }
    }
}

/// Back-compat wrapper for the original two-node tests: named accessors over
/// `ClusterHarness` (which `Deref` exposes for `poll` / `pg` access).
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

/// Retry `check` once per reconciler poll interval (25 tries ≈ 5 s at the harness
/// poll rate); `true` as soon as it passes. The shared retry budget for
/// "node B eventually observes node A's change" assertions.
async fn eventually(poll: Duration, check: impl AsyncFnMut() -> bool) -> bool {
    eventually_n(poll, 25, check).await
}

/// `eventually` with an explicit budget — for tests whose slowest reconciler
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

/// The revision `state`'s node has built and serves.
fn applied_revision(state: &AppState) -> u64 {
    state.reconciler.local_status().applied_revision
}

/// POST an empty payload to `channel` on `node`, answering the status.
async fn post_status(node: &axum::Router, channel: &str) -> StatusCode {
    node.clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v2/data/{channel}"),
            Some(json!({"data": {"x": 1}})),
        ))
        .await
        .unwrap()
        .status()
}

/// Build an N-node cluster over one Postgres container, which is all a
/// cluster shares.
/// `shared` customizes the config every node gets; `per_node(i, cfg)` runs
/// after it for node-specific overrides (e.g. a slow reconciler poll).
async fn cluster_nodes_with(
    n: usize,
    shared: impl Fn(&mut orion::config::AppConfig),
    per_node: impl Fn(usize, &mut orion::config::AppConfig),
) -> ClusterHarness {
    let pg = Postgres::default().start().await.expect("start postgres");
    let pg_port = pg.get_host_port_ipv4(5432).await.expect("pg port");

    let poll = Duration::from_millis(200);
    let mut config = orion::config::AppConfig::default();
    config.storage.url = format!("postgres://postgres:postgres@127.0.0.1:{pg_port}/postgres");
    config.cluster.enabled = true;
    config.cluster.epoch_poll_interval_ms = poll.as_millis() as u64;
    shared(&mut config);

    let mut nodes = Vec::with_capacity(n);
    for i in 0..n {
        let mut cfg = config.clone();
        cfg.cluster.instance_id = format!("node-{}", (b'a' + i as u8) as char);
        per_node(i, &mut cfg);
        let (state, handles) = common::test_state_with_handles(cfg).await;
        orion::cluster::start_cluster_tasks(&state);
        nodes.push(Node {
            router: orion::server::build_router(state.clone()),
            state,
            _handles: handles,
        });
    }

    ClusterHarness { pg, nodes, poll }
}

/// A plugin deployed through node A is compiled and served by node B within
/// a few reconciler polls: B takes up the revision carrying it, loads the
/// component from the shared deployment store and rebuilds its engine with
/// the new handler — no request to B's admin API, no shared filesystem.
///
/// Then the same claim under load: deploying a *new revision* of the plugin
/// through A refuses nothing on B, whose engine is rebuilt whole for the
/// changed fingerprint. This is `deploy/ha/plugin-drill.sh`'s second
/// assertion without the containers — the drill measures it through a load
/// balancer across two processes, and this pins it per run of the cluster
/// suite.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn plugin_activation_propagates_across_nodes() {
    let h = two_nodes_with(|c| {
        c.plugins.enabled = true;
        c.plugins.max_timeout_ms = 2_000;
    })
    .await;
    let component = include_bytes!("../fixtures/plugins/fixture.wasm");
    let manifest = include_str!("../fixtures/plugins/fixture-upload.toml");

    common::deploy_plugin(&h.node_a, manifest, component).await;

    // Node B's generation carries the function once its reconciler takes up
    // the revision.
    let loaded = eventually(h.poll, async || {
        h.state_b
            .runtime
            .load()
            .functions
            .contains("test.fixture.wrap")
    })
    .await;
    assert!(
        loaded,
        "node B must load the plugin within a few reconciler polls"
    );
    assert!(h.state_b.runtime.load().plugins.issues.is_empty());

    // And a workflow deployed on A that calls it serves on B.
    common::create_and_activate_channel(
        &h.node_a,
        "plugin-prop",
        json!({
            "name": "Wrap on B",
            "condition": true,
            "tasks": [
                {"id": "parse", "name": "parse", "function": {"name": "parse_json",
                    "input": {"source": "payload", "target": "input"}}},
                {"id": "wrap", "name": "wrap", "function": {"name": "test.fixture.wrap",
                    "input": {"message": {"var": "data.input.msg"}, "output": "data.result"}}}
            ]
        }),
    )
    .await;
    let served = eventually(h.poll, async || {
        let resp = h
            .node_b
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v2/data/plugin-prop",
                Some(json!({"data": {"msg": "hi"}})),
            ))
            .await
            .unwrap();
        if resp.status() != StatusCode::OK {
            return false;
        }
        body_json(resp).await["data"]["result"]["wrapped"]["message"] == "hi"
    })
    .await;
    assert!(served, "node B must serve the plugin-backed channel");

    // The plugin drill's second claim, in process: deploying a NEW revision
    // under steady load produces no refusal on a peer. The peer's whole
    // engine is rebuilt for a changed plugin fingerprint, so this is the
    // window where a request could be admitted against one generation and
    // executed against another — or meet an estate that has neither revision.
    let hammering = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let load = {
        let router = h.node_b.clone();
        let hammering = hammering.clone();
        tokio::spawn(async move {
            let mut codes = Vec::new();
            while hammering.load(std::sync::atomic::Ordering::Relaxed) {
                let resp = router
                    .clone()
                    .oneshot(json_request(
                        "POST",
                        "/api/v2/data/plugin-prop",
                        Some(json!({"data": {"msg": "hi"}})),
                    ))
                    .await
                    .unwrap();
                codes.push(resp.status());
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            codes
        })
    };

    // The same component under a manifest that differs only by a comment: a
    // different definition, so the plugin's next revision, and deploying it
    // rebuilds the engine on every node.
    let revised = format!("{manifest}\n# revision 2\n");
    common::deploy_plugin(&h.node_a, &revised, component).await;
    let (target, _) = h.state_a.deployments.current_head();

    let on_v2 = eventually(h.poll, async || {
        applied_revision(&h.state_b) == target
            && h.state_b.runtime.load().plugins.fingerprint()
                == h.state_a.runtime.load().plugins.fingerprint()
    })
    .await;
    hammering.store(false, std::sync::atomic::Ordering::Relaxed);
    let codes = load.await.unwrap();
    assert!(on_v2, "node B must reach the new revision");

    let refused: Vec<StatusCode> = codes.iter().copied().filter(|c| !c.is_success()).collect();
    assert!(
        refused.is_empty(),
        "a plugin revision must not refuse a request on a peer: {refused:?} of {} were non-2xx",
        codes.len()
    );
}

/// A2: a channel deployed through node A is served by node B within a few
/// reconciler polls, with no request to node B's admin API.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn deployment_propagates_channel_activation_across_nodes() {
    let h = two_nodes().await;

    common::create_and_activate_channel(
        &h.node_a,
        "prop-ch",
        common::simple_log_workflow("Propagation WF"),
    )
    .await;

    // Node B picks the channel up via its reconciler.
    let payload = json!({"data": {"x": 1}});
    let served = eventually(h.poll, async || {
        let resp = h
            .node_b
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v2/data/prop-ch",
                Some(payload.clone()),
            ))
            .await
            .unwrap();
        resp.status() == StatusCode::OK
    })
    .await;
    assert!(
        served,
        "node B must serve the channel within a few reconciler polls"
    );

    // Sanity: node B's registry (not just routing fallbacks) has it.
    assert!(
        h.state_b
            .runtime
            .load()
            .channels
            .get_by_name("prop-ch")
            .is_some()
    );
}

/// A6: with no cache connector named, dedup uses the cluster's shared store —
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
                "/api/v2/data/dedup-cluster-ch",
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
                "/api/v2/data/dedup-cluster-ch",
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
        .enqueue(orion::storage::repositories::trace_dlq::DlqEnqueue {
            trace_id: "trace-x",
            channel: "orders",
            payload_json: "{}",
            metadata_json: "{}",
            error_message: "boom",
            retry_count: 0,
            max_retries: 5,
        })
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
/// combined (a fixed window in the shared database), not 10 per node.
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
            .runtime
            .load()
            .channels
            .get_by_name("rl-cluster-ch")
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
                "/api/v2/data/rl-cluster-ch",
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

/// Propagation beyond activation: deploying without a channel — what
/// archiving or deleting it was in v1 — rides the same path as adding one
/// (an apply on A; B's reconciler takes up the revision). Two removals, in
/// two revisions, so the second is taken up from a peer that already applied
/// the first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn removal_propagates_across_nodes() {
    let h = two_nodes().await;

    common::create_and_activate_channel(
        &h.node_a,
        "arch-ch",
        common::simple_log_workflow("Archive WF"),
    )
    .await;
    common::create_and_activate_channel(
        &h.node_a,
        "del-ch",
        common::simple_log_workflow("Delete WF"),
    )
    .await;

    // Baseline: node B serves both.
    for ch in ["arch-ch", "del-ch"] {
        let served = eventually(h.poll, async || {
            post_status(&h.node_b, ch).await == StatusCode::OK
        })
        .await;
        assert!(served, "node B must serve {ch} before the removal phase");
    }

    common::remove_member(&h.node_a, "channel", "arch-ch").await;
    common::remove_member(&h.node_a, "channel", "del-ch").await;

    // Node A stops serving immediately (its apply rebuilt it inline)...
    for ch in ["arch-ch", "del-ch"] {
        assert_eq!(
            post_status(&h.node_a, ch).await,
            StatusCode::NOT_FOUND,
            "{ch} must 404 on node A immediately after the removal"
        );
    }

    // ...and node B within the poll budget.
    for ch in ["arch-ch", "del-ch"] {
        let gone = eventually(h.poll, async || {
            post_status(&h.node_b, ch).await == StatusCode::NOT_FOUND
        })
        .await;
        assert!(gone, "node B must stop serving {ch} within the poll budget");
    }
}

/// T6 — connection change propagation: a connection repointed through node
/// A (a new connection revision, deployed) must take effect on node B, which
/// requires B to load the new revision into its connector registry and to
/// stop answering from a pool built for the old settings.
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
                "/api/v2/data/conn-prop-ch",
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

    // Repoint the connection at the new DB via node A: its next revision,
    // deployed.
    common::create_connector(&h.node_a, common::db_connector_sqlite("prop-conn", new_url)).await;

    // Node A serves "new" immediately (its apply rebuilt it inline).
    assert_eq!(read_val(h.node_a.clone()).await.as_deref(), Some("new"));

    // The T6 pin: node B must serve "new" within the poll budget.
    let propagated = eventually(h.poll, async || {
        read_val(h.node_b.clone()).await.as_deref() == Some("new")
    })
    .await;
    assert!(propagated, "the connection change must propagate to node B");
}

/// Concurrent deployments on different nodes converge: two applies over the
/// same head race the compare-and-swap, exactly one commits, and the other
/// is refused with `409` rather than silently overwriting it. Retried over
/// the new head, the loser commits too, and both nodes serve both channels —
/// neither node's change can mask the other's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn concurrent_admin_writes_converge() {
    let h = two_nodes().await;

    // Each node uploads its own channel and workflow and merges them over
    // the head it sees — the same head on both, so the applies race.
    let upserts = |name: &str, workflow_id: String, wf: String, ch: String| {
        vec![
            common::deploy::Upsert {
                kind: "workflow",
                id: workflow_id.clone(),
                digest: wf,
                revision: None,
                requires: Vec::new(),
            },
            common::deploy::Upsert {
                kind: "channel",
                id: name.to_string(),
                digest: ch,
                revision: None,
                requires: vec![("workflow", workflow_id)],
            },
        ]
    };
    let mut plans = Vec::new();
    for (node, name) in [(&h.node_a, "conc-a"), (&h.node_b, "conc-b")] {
        let workflow = common::simple_log_workflow(&format!("{name} WF"));
        let workflow_id = common::deploy::workflow_id_of(&workflow);
        let wf = common::deploy::upload(node, common::deploy::workflow_definition(&workflow)).await;
        let ch = common::deploy::upload(
            node,
            common::deploy::channel_definition(&common::sync_http_channel(name, &workflow_id)).0,
        )
        .await;
        plans.push(upserts(name, workflow_id, wf, ch));
    }
    let plan_b = plans.pop().unwrap();
    let plan_a = plans.pop().unwrap();
    let (base_a, members_a) = common::deploy::merged(&h.node_a, plan_a).await;
    let (base_b, members_b) = common::deploy::merged(&h.node_b, plan_b).await;
    assert_eq!(base_a, base_b, "both applies are based on one head");

    let ((status_a, body_a), (status_b, body_b)) = tokio::join!(
        common::deploy::apply(&h.node_a, base_a, members_a),
        common::deploy::apply(&h.node_b, base_b, members_b),
    );
    let mut statuses = [status_a, status_b];
    statuses.sort();
    assert_eq!(
        statuses,
        [StatusCode::ACCEPTED, StatusCode::CONFLICT],
        "exactly one apply wins the compare-and-swap: {body_a} / {body_b}"
    );

    // The loser retries over the head it learns from its reconciler.
    let (loser, name) = if status_a == StatusCode::CONFLICT {
        (&h.node_a, "conc-a")
    } else {
        (&h.node_b, "conc-b")
    };
    let workflow = common::simple_log_workflow(&format!("{name} WF"));
    let retried = eventually(h.poll, async || {
        let wf =
            common::deploy::upload(loser, common::deploy::workflow_definition(&workflow)).await;
        let workflow_id = common::deploy::workflow_id_of(&workflow);
        let ch = common::deploy::upload(
            loser,
            common::deploy::channel_definition(&common::sync_http_channel(name, &workflow_id)).0,
        )
        .await;
        let (base, members) =
            common::deploy::merged(loser, upserts(name, workflow_id, wf, ch)).await;
        common::deploy::apply(loser, base, members).await.0 == StatusCode::ACCEPTED
    })
    .await;
    assert!(
        retried,
        "the refused apply commits once retried over the new head"
    );

    // Both nodes must converge on serving BOTH channels.
    for (node_name, ch, node) in [
        ("node A", "conc-b", &h.node_a),
        ("node B", "conc-a", &h.node_b),
        ("node A", "conc-a", &h.node_a),
        ("node B", "conc-b", &h.node_b),
    ] {
        let served = eventually(h.poll, async || {
            post_status(node, ch).await == StatusCode::OK
        })
        .await;
        assert!(
            served,
            "{node_name} must serve {ch} after the concurrent writes"
        );
    }
}

/// N-generality: nothing in the runtime's `cluster/` assumes two nodes (D4 —
/// no node registry). Three nodes over one Postgres, activation via node 0,
/// nodes 1 and 2 both serve.
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
                    "/api/v2/data/tri-ch",
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
                .runtime
                .load()
                .channels
                .get_by_name("tri-ch")
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
            &format!("/api/v2/data/{ch}"),
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
                "/api/v2/admin/connectors/circuit-breakers",
                None,
            ))
            .await
            .unwrap();
        body_json(resp).await
    };
    let breaker_state = |body: &serde_json::Value, connector: &str| {
        // R17/R18 wrapped every admin 2xx body in `data`; this reader was not
        // updated, and cluster tests are container-gated, so it went unnoticed.
        body["data"]["breakers"]
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
            &format!("/api/v2/admin/connectors/circuit-breakers/{key}"),
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
        body["data"]["breakers"][&key2].as_str(),
        Some("closed"),
        "the last reset key must be applied: {body}"
    );
    let key1_state = body["data"]["breakers"][&key].as_str().unwrap_or_default();
    assert_eq!(
        key1_state, "open",
        "the coalesced (first) reset is lost — documented last-write-wins: {body}"
    );
}

/// The Postgres outage window + stale-node semantics, pinned in one pause
/// cycle. There is NO fencing anywhere by design (retired HA plan D1): a
/// node whose reconciler cannot read the deployment store keeps serving its
/// last-applied generation, and the containment is `/readyz` withdrawing
/// every node so the LB stops sending traffic. Node B runs a 5s reconciler
/// poll so "remove on A, pause before B's next tick" is anchored to an
/// observed tick with a ~25x safety ratio — the guard assertion detects the
/// pathological miss loudly instead of letting the test pass vacuously.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn postgres_outage_stale_node_keeps_serving_and_recovers() {
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

    common::create_and_activate_channel(
        &node_a,
        "stale-ch",
        common::simple_log_workflow("Stale WF"),
    )
    .await;

    // Baseline: node B serves (budget covers its 5s poll).
    let payload = json!({"data": {"x": 1}});
    let served = eventually_n(Duration::from_millis(500), 30, async || {
        node_b
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v2/data/stale-ch",
                Some(payload.clone()),
            ))
            .await
            .unwrap()
            .status()
            == StatusCode::OK
    })
    .await;
    assert!(served, "node B must serve the baseline channel");
    let e_b = applied_revision(&state_b);

    // Remove it via node A, then immediately pause Postgres — inside node
    // B's ~5s tick window (the apply+pause pair takes tens of milliseconds).
    common::remove_member(&node_a, "channel", "stale-ch").await;
    h.pg.pause().await.expect("pause postgres");
    assert_eq!(
        applied_revision(&state_b),
        e_b,
        "node B ticked inside the remove->pause gap (a ~25x-margin race); re-run"
    );

    // Every outage-window request is bounded by an outer timeout: DB
    // failures are bounded by the 3s pool-acquire timeout, and a hang must
    // fail loudly rather than stall the suite.
    let deadline = Duration::from_secs(15);

    // PIN 1 — availability over consistency: node B keeps serving its
    // last-applied generation (200) even though the committed head no
    // longer carries the channel. The 200 also pins "trace persistence down != data plane down":
    // the sync trace write fails after ~3s and is swallowed with a warning.
    let resp = tokio::time::timeout(
        deadline,
        node_b.clone().oneshot(json_request(
            "POST",
            "/api/v2/data/stale-ch",
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

    // PIN 2 — the split view: node A already applied the removal → 404.
    let resp = tokio::time::timeout(
        deadline,
        node_a.clone().oneshot(json_request(
            "POST",
            "/api/v2/data/stale-ch",
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

    // PIN 4 — admin mutations fail loudly: a definition upload is a write to
    // the deployment store, and it fails first.
    let (status, body) = tokio::time::timeout(
        deadline,
        common::deploy::try_upload(
            &node_a,
            common::deploy::workflow_definition(&common::simple_log_workflow("Outage WF")),
        ),
    )
    .await
    .expect("admin mutation must not hang");
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "admin writes must fail during the outage: {body}"
    );
    assert_eq!(body["error"]["code"], "DB_TRANSACTION_ERROR", "{body}");

    // PIN 5 — a failed reconcile never advances the applied revision.
    assert_eq!(applied_revision(&state_b), e_b);

    // Recovery: node B reconciles on its next successful tick and applies
    // the removal; readiness and propagation come back.
    h.pg.unpause().await.expect("unpause postgres");
    let archived = eventually_n(Duration::from_millis(500), 40, async || {
        node_b
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v2/data/stale-ch",
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
        "node B must apply the removal once Postgres returns"
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
                "/api/v2/data/post-outage-ch",
                Some(payload.clone()),
            ))
            .await
            .unwrap()
            .status()
            == StatusCode::OK
    })
    .await;
    assert!(served, "propagation must be live again after recovery");
}

/// T6 — the response cache is a shared store, not per-node memory: a body
/// cached by node A must be replayed by node B. With no connector named on
/// the channel, cluster-mode backend resolution rides the cluster's shared
/// store (`registry.rs` resolves `cluster.default_cache`; the guardrail
/// refuses per-node memory stores outright), and the SHA-256 cache key is
/// process-independent by design, so both nodes agree on the key.
///
/// The proof borrows the single-process `cache_key_fields` mechanism: the
/// key is scoped to `user_id` only, so an `origin` field marks WHICH
/// execution produced the stored body without changing the key. Node B's
/// same-keyed request carries `origin: "node-b"` — an execution would echo
/// it back, a shared-cache hit replays node A's `"node-a"` body.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn response_cache_is_shared_across_nodes() {
    let h = two_nodes().await;

    common::create_and_activate_channel_with_config(
        &h.node_a,
        "cache-cluster-ch",
        common::echo_workflow("Cluster Cache WF"),
        json!({
            "cache": {
                "enabled": true,
                "ttl_secs": 300,
                "cache_key_fields": ["user_id"]
            }
        }),
    )
    .await;

    // Node A executes and populates the shared cache. No `eventually` needed:
    // activation went through node A, whose local reload precedes the reply.
    // The cache store is awaited on the response path (`persist_trace_and_cache`
    // runs before the handler returns), so once this 200 is back the shared
    // entry exists — node B cannot slip in an execute-and-store first.
    let resp = h
        .node_a
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/cache-cluster-ch",
            Some(json!({"data": {"user_id": "u1", "origin": "node-a"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["data"]["echo"]["origin"], "node-a",
        "node A's first request must execute (empty cache): {body}"
    );

    // Same key (`user_id`), different origin, via node B. Non-200 means the
    // channel has not propagated yet; the first 200 must already be the
    // shared-cache hit because the entry predates every node B request.
    let mut served_body = json!(null);
    let served = eventually(h.poll, async || {
        let resp = h
            .node_b
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v2/data/cache-cluster-ch",
                Some(json!({"data": {"user_id": "u1", "origin": "node-b"}})),
            ))
            .await
            .unwrap();
        if resp.status() != StatusCode::OK {
            return false;
        }
        served_body = body_json(resp).await;
        true
    })
    .await;
    assert!(
        served,
        "node B must serve the channel within the poll budget"
    );
    assert_eq!(
        served_body["data"]["echo"]["origin"], "node-a",
        "node B must replay the body node A cached, not execute: {served_body}"
    );

    // Control for the proof mechanism itself: a different user_id is a
    // different key, so node B executes and echoes its own origin — the
    // marker genuinely distinguishes executions from cache hits.
    let resp = h
        .node_b
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/cache-cluster-ch",
            Some(json!({"data": {"user_id": "u2", "origin": "node-b-fresh"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let control = body_json(resp).await;
    assert_eq!(
        control["data"]["echo"]["origin"], "node-b-fresh",
        "a different cache key must execute on node B: {control}"
    );
}

/// One trace-cleanup tick, exactly as `start_retention_job`'s loop runs it:
/// consult the lease gate, run one retention pass only when the lease is
/// held. The production body is interval-driven inside a spawned task
/// (src/queue/mod.rs) with no test entry point, so the test composes the
/// identical sequence from the same two public parts the loop uses.
/// `None` = lease-skip (the tick did not touch the table).
async fn trace_cleanup_tick(
    gate: &orion::cluster::JobLeaseGate,
    repo: &std::sync::Arc<dyn orion::storage::repositories::traces::TraceRepository>,
) -> Option<u64> {
    if !gate.try_acquire("trace_cleanup", 120).await {
        return None;
    }
    Some(repo.delete_older_than(24).await.expect("retention pass"))
}

/// T6 — trace-cleanup single-flight: when two nodes tick at the same moment,
/// exactly one runs the retention pass and the other's tick is a lease-skip.
/// Without the `JobLeaseGate` every replica would issue the same DELETE
/// every tick — invisible via row counts (a second DELETE finds nothing),
/// which is exactly why the pin is the gate outcome, not the table state:
/// the loser must never reach the DELETE at all.
///
/// The harness's real cleanup tasks are running too, but their first
/// interval tick is 3600s out (defaults; the immediate tick is skipped), so
/// the only "trace_cleanup" lease traffic in the test window is ours.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn trace_cleanup_ticks_on_both_nodes_delete_exactly_once() {
    use orion::cluster::JobLeaseGate;

    let h = two_nodes().await;

    // Seed three completed traces, then backdate them past the 24h retention
    // cutoff (the repo always stamps "now"; only these three rows exist yet).
    for _ in 0..3 {
        h.state_a
            .repos
            .traces
            .store_completed(orion::storage::repositories::traces::TraceCompletedRef {
                failed: false,
                channel: "cleanup-ch",
                channel_id: None,
                mode: "sync",
                input_json: None,
                result_json: "{}",
                duration_ms: 1.0,
                task_trace_json: None,
            })
            .await
            .expect("seed expired trace");
    }
    let orion::storage::DbPool::Postgres(pg) = &h.state_a.db_pool else {
        panic!("postgres expected");
    };
    sqlx::query("UPDATE traces SET created_at = LOCALTIMESTAMP - interval '48 hours'")
        .execute(pg)
        .await
        .expect("backdate");
    // One fresh trace that must survive the pass.
    let fresh_id = h
        .state_a
        .repos
        .traces
        .store_completed(orion::storage::repositories::traces::TraceCompletedRef {
            failed: false,
            channel: "cleanup-ch",
            channel_id: None,
            mode: "sync",
            input_json: None,
            result_json: "{}",
            duration_ms: 1.0,
            task_trace_json: None,
        })
        .await
        .expect("seed fresh trace");

    // The same gates bootstrap.rs builds for the cleanup jobs: each node's
    // cluster repo + instance id.
    let gate_a = JobLeaseGate::new(
        h.state_a.cluster.repo.clone(),
        h.state_a.cluster.instance_id.clone(),
    );
    let gate_b = JobLeaseGate::new(
        h.state_b.cluster.repo.clone(),
        h.state_b.cluster.instance_id.clone(),
    );

    // Both nodes tick concurrently. Lease acquisition is one atomic
    // UPDATE/INSERT race in the shared Postgres, so exactly one wins.
    let (a, b) = tokio::join!(
        trace_cleanup_tick(&gate_a, &h.state_a.repos.traces),
        trace_cleanup_tick(&gate_b, &h.state_b.repos.traces),
    );

    assert!(
        a.is_some() ^ b.is_some(),
        "exactly one node must win the trace_cleanup lease (a={a:?}, b={b:?})"
    );
    let deleted = a.or(b).unwrap();
    assert_eq!(
        deleted, 3,
        "the winning tick must delete the three expired traces — once"
    );

    // The loser's skip left the table to the winner: only the fresh row
    // remains (a doubled pass could not double-delete, but a pass that also
    // caught the fresh row, or deleted nothing, shows up here).
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM traces")
        .fetch_one(pg)
        .await
        .expect("count");
    assert_eq!(count, 1, "only the fresh trace must remain");
    let (remaining,): (String,) = sqlx::query_as("SELECT id FROM traces")
        .fetch_one(pg)
        .await
        .expect("remaining id");
    assert_eq!(remaining, fresh_id);

    // Next tick, same order of play as production: the loser is still locked
    // out while the winner's lease is live, and the incumbent renews cheaply.
    let (winner, loser) = if a.is_some() {
        (&gate_a, &gate_b)
    } else {
        (&gate_b, &gate_a)
    };
    assert!(
        !loser.try_acquire("trace_cleanup", 120).await,
        "a live lease must keep the other node skipping"
    );
    assert!(
        winner.try_acquire("trace_cleanup", 120).await,
        "the incumbent must renew its own lease"
    );
}

/// POST with the sticky-rollout identity header the canary test's nodes
/// bucket on.
fn post_with_sticky_caller(uri: &str, caller: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-canary-caller", caller)
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap()
}

/// T6 — sticky rollout across replicas: with a 50/50 canary split between two
/// workflow revisions deployed on both nodes, the SAME sticky identity must
/// get the SAME revision from node A and node B. The bucketing is an unkeyed FNV-1a hash of the
/// configured sticky header's value (`engine/utils.rs`), deliberately
/// process-independent — this is the cross-node pin that design carries.
///
/// The identities are fixed strings so the buckets are deterministic run to
/// run: caller-0..9 hash to buckets {19, 8, 41, 30, 75, 64, 97, 86, 31, 20},
/// which covers both halves of [0, 100) whichever half v2 owns — so the
/// A==B assertion cannot hold vacuously with every identity on one side.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn sticky_rollout_assignment_is_stable_across_nodes() {
    let h = two_nodes_with(|config| {
        config.engine.rollout_sticky_header = "x-canary-caller".to_string();
    })
    .await;

    // v1 and v2 mark themselves in the response body, so "which revision
    // served this" is readable from `data.version`.
    let marker_workflow = |marker: &str| {
        json!({
            "workflow_id": "canary",
            "name": "Canary",
            "condition": true,
            "tasks": [{
                "id": "t1",
                "name": "Mark the version",
                "function": {
                    "name": "map",
                    "input": {"mappings": [{"path": "data.version", "logic": marker}]}
                }
            }]
        })
    };

    // Revision 1 of the workflow at 100% on a channel; then a deployment
    // carrying revisions 1 and 2 side by side, the channel's rollout
    // splitting the buckets between them — all via node A.
    common::create_and_activate_channel(&h.node_a, "canary-ch", marker_workflow("v1")).await;
    let v2 = common::deploy::upload(
        &h.node_a,
        common::deploy::workflow_definition(&marker_workflow("v2")),
    )
    .await;
    let (base, mut members) = common::deploy::head(&h.node_a).await;
    members.push(json!({
        "entity": {"kind": "workflow", "id": "canary", "revision": 2},
        "definition_digest": v2,
        "requires": [],
    }));
    let channel = members
        .iter_mut()
        .find(|m| m["entity"]["kind"] == "channel" && m["entity"]["id"] == "canary-ch")
        .expect("the channel member");
    channel["requires"] = json!([{"kind": "workflow", "id": "canary", "revision": 2}]);
    channel["rollouts"] = json!([
        {"workflow_revision": 1, "start_bucket": 0, "end_bucket": 50},
        {"workflow_revision": 2, "start_bucket": 50, "end_bucket": 100},
    ]);
    let (status, body) = common::deploy::apply(&h.node_a, base, members).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["evaluation"]["converged"], true, "{body}");

    let version_on = |node: axum::Router, caller: String| async move {
        let resp = node
            .oneshot(post_with_sticky_caller(
                "/api/v2/data/canary-ch",
                &caller,
                json!({"data": {}}),
            ))
            .await
            .unwrap();
        if resp.status() != StatusCode::OK {
            return None;
        }
        body_json(resp).await["data"]["version"]
            .as_str()
            .map(str::to_string)
    };

    // Node A applied the split inline; its per-identity answers are the
    // baseline every later request must reproduce.
    let identities: Vec<String> = (0..10).map(|i| format!("caller-{i}")).collect();
    let mut baseline = Vec::with_capacity(identities.len());
    for id in &identities {
        let v = version_on(h.node_a.clone(), id.clone())
            .await
            .unwrap_or_else(|| panic!("node A must serve {id} right after activation"));
        baseline.push(v);
    }
    assert!(
        baseline.iter().any(|v| v == "v1") && baseline.iter().any(|v| v == "v2"),
        "the fixed identity set must land on both sides of the split: {baseline:?}"
    );

    // Node B serves v1-at-100% until its reconciler applies the split (one
    // atomic engine swap), so "answers v2 for an identity node A routes to
    // v2" is exactly "node B has the split".
    let v2_caller = identities[baseline.iter().position(|v| v == "v2").unwrap()].clone();
    let propagated = eventually(h.poll, async || {
        version_on(h.node_b.clone(), v2_caller.clone())
            .await
            .as_deref()
            == Some("v2")
    })
    .await;
    assert!(propagated, "node B must apply the 50/50 split");

    // THE pin, twice over: every identity gets its baseline version from
    // BOTH nodes, and repeatably — same caller, same version, wherever the
    // LB happens to send the request.
    for round in 0..2 {
        for (id, expected) in identities.iter().zip(&baseline) {
            let b = version_on(h.node_b.clone(), id.clone()).await;
            assert_eq!(
                b.as_deref(),
                Some(expected.as_str()),
                "round {round}: node B must serve {id} the version node A assigned"
            );
            let a = version_on(h.node_a.clone(), id.clone()).await;
            assert_eq!(
                a.as_deref(),
                Some(expected.as_str()),
                "round {round}: node A must stay sticky for {id}"
            );
        }
    }
}

/// A rollback committed through node A reaches node B like any other
/// revision: revision 1 serves one behaviour, revision 2 another, and a
/// rollback to 1 — a new revision carrying revision 1's manifest — makes
/// both nodes serve revision 1's behaviour again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn rollback_propagates_across_nodes() {
    let h = two_nodes().await;
    let marker_workflow = |marker: &str| {
        json!({
            "workflow_id": "rollback-wf",
            "name": "Rollback",
            "condition": true,
            "tasks": [{
                "id": "t1",
                "name": "Mark the revision",
                "function": {
                    "name": "map",
                    "input": {"mappings": [{"path": "data.version", "logic": marker}]}
                }
            }]
        })
    };
    let version_on = |node: axum::Router| async move {
        let resp = node
            .oneshot(json_request(
                "POST",
                "/api/v2/data/rollback-ch",
                Some(json!({"data": {}})),
            ))
            .await
            .unwrap();
        if resp.status() != StatusCode::OK {
            return None;
        }
        body_json(resp).await["data"]["version"]
            .as_str()
            .map(str::to_string)
    };

    common::create_and_activate_channel(&h.node_a, "rollback-ch", marker_workflow("first")).await;
    let (first, _) = h.state_a.deployments.current_head();
    let on_first = eventually(h.poll, async || {
        version_on(h.node_b.clone()).await.as_deref() == Some("first")
    })
    .await;
    assert!(on_first, "node B must serve the first revision");

    common::create_and_activate_workflow(&h.node_a, marker_workflow("second")).await;
    let (second, _) = h.state_a.deployments.current_head();
    assert_eq!(
        version_on(h.node_a.clone()).await.as_deref(),
        Some("second")
    );
    let on_second = eventually(h.poll, async || {
        version_on(h.node_b.clone()).await.as_deref() == Some("second")
    })
    .await;
    assert!(on_second, "node B must serve the second revision");

    let resp = h
        .node_a
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/admin/deployments",
            Some(json!({
                "idempotency_key": "rollback-to-first",
                "base_revision": second,
                "rollback_to": first,
                "reason": "cluster rollback test",
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    assert_eq!(body["evaluation"]["converged"], true, "{body}");
    let rolled_back = body["committed_revision"].as_u64().expect("a revision");
    assert!(
        rolled_back > second,
        "a rollback is a new forward revision: {body}"
    );

    assert_eq!(
        version_on(h.node_a.clone()).await.as_deref(),
        Some("first"),
        "node A serves the first revision's behaviour right after the rollback"
    );
    let back = eventually(h.poll, async || {
        applied_revision(&h.state_b) == rolled_back
            && version_on(h.node_b.clone()).await.as_deref() == Some("first")
    })
    .await;
    assert!(
        back,
        "node B must serve the first revision's behaviour again"
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
            .oneshot(json_request(method, "/api/v2/admin/backups", None))
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

/// The deployment store's artifact collector in a cluster: both nodes run
/// the scheduled job, one holds the `deployment_gc` lease and collects, and
/// what it collects is an abandoned upload — never what the deployment both
/// nodes serve names.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn one_node_collects_unreferenced_artifacts_for_the_cluster() {
    let h = two_nodes_with(|config| {
        config.storage.artifact_gc_interval_secs = 1;
        config.storage.artifact_gc_grace_secs = 3600;
    })
    .await;
    common::create_and_activate_channel(&h.node_a, "gc-ch", common::simple_log_workflow("GC"))
        .await;
    let orphan = common::deploy::upload(
        &h.node_a,
        json!({"kind": "workflow", "definition": common::simple_log_workflow("Abandoned")}),
    )
    .await;
    let orion::storage::DbPool::Postgres(pg) = &h.state_a.db_pool else {
        panic!("postgres expected");
    };
    sqlx::query(
        "UPDATE artifact_objects SET created_at = LOCALTIMESTAMP - interval '2 hours' WHERE digest = $1",
    )
    .bind(&orphan)
    .execute(pg)
    .await
    .expect("backdate");

    // Two passes, about a second apart, on whichever node holds the lease.
    let state_of = async |digest: &str| -> String {
        sqlx::query_scalar("SELECT state FROM artifact_objects WHERE digest = $1")
            .bind(digest)
            .fetch_one(pg)
            .await
            .expect("artifact row")
    };
    assert!(
        eventually_n(h.poll, 75, async || state_of(&orphan).await == "deleted").await,
        "the abandoned upload must be collected within a few passes, is '{}'",
        state_of(&orphan).await
    );

    // Exactly one node holds the job; the other's ticks are lease-skips.
    let holders: Vec<String> =
        sqlx::query_scalar("SELECT holder FROM job_leases WHERE job_name = 'deployment_gc'")
            .fetch_all(pg)
            .await
            .expect("leases");
    assert_eq!(holders.len(), 1, "{holders:?}");
    let (holder, other) = if holders[0] == h.state_a.cluster.instance_id {
        (&h.state_a, &h.state_b)
    } else {
        (&h.state_b, &h.state_a)
    };
    let gate = |state: &AppState| {
        orion::cluster::JobLeaseGate::new(
            state.cluster.repo.clone(),
            state.cluster.instance_id.clone(),
        )
    };
    assert!(!gate(other).try_acquire("deployment_gc", 61).await);
    assert!(gate(holder).try_acquire("deployment_gc", 61).await);

    // Every artifact the head names is still active, and both nodes serve.
    let referenced: Vec<String> = sqlx::query_scalar(
        "SELECT definition_digest FROM deployment_members m \
         JOIN deployment_head h ON m.deployment_revision = h.head_revision",
    )
    .fetch_all(pg)
    .await
    .expect("members");
    assert!(!referenced.is_empty());
    for digest in &referenced {
        assert_eq!(state_of(digest).await, "active", "{digest}");
    }
    assert!(
        eventually(h.poll, async || post_status(&h.node_b, "gc-ch").await
            == StatusCode::OK)
        .await
    );
    assert_eq!(post_status(&h.node_a, "gc-ch").await, StatusCode::OK);
}

// ============================================================
// Cron: the multi-node contracts
// ============================================================

/// Deploy a workflow and a cron channel through `node`, returning the
/// channel's id.
async fn activate_cluster_cron_channel(
    node: &axum::Router,
    name: &str,
    workflow: serde_json::Value,
    transport_config: serde_json::Value,
) -> String {
    let workflow_id = common::create_and_activate_workflow(node, workflow).await;
    common::create_cron_channel(node, name, &workflow_id, transport_config).await
}

/// Every occurrence in the shared ledger, read through either node.
async fn cluster_occurrences(node: &axum::Router) -> Vec<serde_json::Value> {
    let resp = node
        .clone()
        .oneshot(common::json_request(
            "GET",
            "/api/v2/admin/cron/occurrences?limit=200",
            None,
        ))
        .await
        .expect("list");
    common::body_json(resp).await["data"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// The property the whole reconciler design rests on, under real concurrency:
/// two nodes reconciling the same schedule against one database produce one
/// occurrence per scheduled instant, not two.
///
/// Both nodes run their own reconciler on the same second. The `JobLeaseGate`
/// makes a duplicate pass *unlikely*; `UNIQUE(channel_id, scheduled_for)` makes
/// a duplicate row *impossible*, and it is the second one this checks — which
/// is why the test does not disable the gate to force the race. Either way the
/// ledger must agree.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn cron_occurrences_are_materialised_once_across_nodes() {
    let h = two_nodes_with(|config| {
        config.cron.poll_interval_ms = 100;
        config.cron.heartbeat_interval_secs = 1;
        config.cron.claim_lease_secs = 30;
    })
    .await;

    activate_cluster_cron_channel(
        &h.node_a,
        "cluster-cron",
        common::simple_log_workflow("Cluster Cron WF"),
        json!({"schedule": "* * * * * *"}),
    )
    .await;

    // Let both nodes converge on the channel and run a few passes each.
    let produced = eventually(h.poll, async || {
        cluster_occurrences(&h.node_a).await.len() >= 3
    })
    .await;
    assert!(
        produced,
        "the schedule must produce occurrences in a cluster"
    );

    let occurrences = cluster_occurrences(&h.node_a).await;
    let mut instants: Vec<&str> = occurrences
        .iter()
        .filter_map(|o| o["scheduled_for"].as_str())
        .collect();
    instants.sort_unstable();
    let unique = instants.iter().collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        instants.len(),
        unique.len(),
        "two reconcilers produced duplicate occurrences for one instant: {instants:?}"
    );

    // Both nodes read the same ledger — it is shared state, not per-node.
    assert_eq!(
        cluster_occurrences(&h.node_b).await.len(),
        occurrences.len(),
        "the occurrence ledger is shared, so both nodes must see the same rows"
    );
}

/// Non-overlap across the cluster, which is the guarantee that cannot be made
/// with an in-process lock.
///
/// Both nodes claim from one ledger, and work that outlasts the interval
/// between occurrences guarantees contention. Under `forbid`, at most one
/// occurrence may be `running` at any instant *whichever node is running it* —
/// and the losers are recorded rather than dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn cron_singleton_holds_across_nodes() {
    let addr = common::start_slow_server(std::time::Duration::from_millis(1500)).await;
    let h = two_nodes_with(|config| {
        config.cron.poll_interval_ms = 100;
        config.cron.heartbeat_interval_secs = 1;
        config.cron.claim_lease_secs = 30;
    })
    .await;

    common::create_http_connector(&h.node_a, "slow-endpoint", addr).await;
    activate_cluster_cron_channel(
        &h.node_a,
        "cluster-singleton",
        common::workflow_with_tasks(
            "Cluster Slow WF",
            json!([{
                "id": "slow",
                "name": "A call that takes a while",
                "function": {
                    "name": "http_call",
                    "input": {
                        "connector": "slow-endpoint",
                        "method": "GET",
                        "path": "/slow",
                        "output": "data.called",
                        "timeout_ms": 5000
                    }
                }
            }]),
        ),
        json!({
            "schedule": "* * * * * *",
            "concurrency": {"policy": "forbid"},
        }),
    )
    .await;

    // Sample the ledger repeatedly: the invariant is about every instant, not
    // about the end state.
    let mut max_running = 0usize;
    let contended = eventually(h.poll, async || {
        let occurrences = cluster_occurrences(&h.node_a).await;
        let running = occurrences
            .iter()
            .filter(|o| o["status"] == "running")
            .count();
        max_running = Ord::max(max_running, running);
        occurrences
            .iter()
            .any(|o| o["status"] == "skipped_singleton")
            && occurrences.iter().any(|o| o["status"] == "completed")
    })
    .await;
    assert!(
        contended,
        "a 1.5s workflow on a per-second schedule must contend within the window"
    );
    assert!(
        max_running <= 1,
        "two occurrences of one singleton key were running at once across the \
         cluster (saw {max_running})"
    );
}

/// The same guarantee with `slots`: two nodes, one key of two slots, and a
/// workflow that outlasts two schedule ticks. At most two run at any instant
/// whichever nodes run them, and the rest are recorded as skips.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn cron_slots_bound_holds_across_nodes() {
    let addr = common::start_slow_server(std::time::Duration::from_millis(2500)).await;
    let h = two_nodes_with(|config| {
        config.cron.poll_interval_ms = 100;
        config.cron.heartbeat_interval_secs = 1;
        config.cron.claim_lease_secs = 30;
    })
    .await;

    common::create_http_connector(&h.node_a, "slow-endpoint", addr).await;
    activate_cluster_cron_channel(
        &h.node_a,
        "cluster-slots",
        common::workflow_with_tasks(
            "Cluster Slotted WF",
            json!([{
                "id": "slow",
                "name": "A call that takes a while",
                "function": {
                    "name": "http_call",
                    "input": {
                        "connector": "slow-endpoint",
                        "method": "GET",
                        "path": "/slow",
                        "output": "data.called",
                        "timeout_ms": 5000
                    }
                }
            }]),
        ),
        json!({
            "schedule": "* * * * * *",
            "concurrency": {"policy": "forbid", "slots": 2},
        }),
    )
    .await;

    // Two slots of 2.5s runs take several ticks to fill: ~20s of budget.
    let mut max_running = 0usize;
    let contended = eventually_n(h.poll, 100, async || {
        let occurrences = cluster_occurrences(&h.node_a).await;
        let running = occurrences
            .iter()
            .filter(|o| o["status"] == "running")
            .count();
        max_running = Ord::max(max_running, running);
        occurrences
            .iter()
            .any(|o| o["status"] == "skipped_singleton")
            && occurrences
                .iter()
                .filter(|o| o["status"] == "completed")
                .count()
                >= 2
    })
    .await;
    assert!(
        contended,
        "a 2.5s workflow on a per-second schedule must fill two slots within the window"
    );
    assert!(
        max_running <= 2,
        "three occurrences of a two-slot key were running at once across the cluster \
         (saw {max_running})"
    );
}

/// A schedule change deployed through one node reaches the other the way
/// every other deployment does — the reconciler has no watcher of its own
/// and needs none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker; run with: cargo test --test cluster -- --ignored"]
async fn cron_schedule_changes_propagate_across_nodes() {
    let h = two_nodes_with(|config| {
        config.cron.poll_interval_ms = 100;
    })
    .await;

    let channel_id = activate_cluster_cron_channel(
        &h.node_a,
        "propagating-cron",
        common::simple_log_workflow("Propagating Cron WF"),
        json!({"schedule": "* * * * * *"}),
    )
    .await;

    // Node B learns about the schedule from the deployment, not through a
    // cron-specific mechanism.
    let seen = eventually(h.poll, async || {
        h.state_b
            .runtime
            .load()
            .channels
            .cron_by_channel_id(&channel_id)
            .is_some()
    })
    .await;
    assert!(
        seen,
        "node B must load the cron channel within a few reconciler polls"
    );

    // Deploying without it on A stops both: the cursor is paused once, in
    // shared state.
    common::remove_member(&h.node_a, "channel", &channel_id).await;

    let stopped = eventually(h.poll, async || {
        h.state_b
            .runtime
            .load()
            .channels
            .cron_by_channel_id(&channel_id)
            .is_none()
    })
    .await;
    assert!(
        stopped,
        "node B must stop scheduling a removed channel within a few reconciler polls"
    );
}
