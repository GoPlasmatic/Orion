//! Configuration correctness across commit, rebuild and connection creation.

use std::sync::atomic::Ordering;
use std::time::Duration;

use orion::config::AppConfig;
use orion::connector::pool_cache::{SqlPool, SqlPoolCache};

use crate::common;

#[tokio::test]
async fn sql_pool_identity_includes_the_connection_settings() {
    let a = common::ScratchDir::new("pool_revision_a");
    let b = common::ScratchDir::new("pool_revision_b");
    let settings = |dir: &common::ScratchDir| {
        serde_json::from_value(
            serde_json::json!({"connection_string": format!("{}?mode=rwc", dir.url())}),
        )
        .expect("settings")
    };
    let cache = SqlPoolCache::new(8);
    let SqlPool::Sqlite(old) = cache
        .get_pool("orders", &settings(&a))
        .await
        .expect("old pool")
    else {
        panic!("sqlite");
    };
    sqlx::query("CREATE TABLE identity_marker (value INTEGER)")
        .execute(&old)
        .await
        .expect("table");
    sqlx::query("INSERT INTO identity_marker VALUES (1)")
        .execute(&old)
        .await
        .expect("marker");
    // No eviction is needed to prevent an old revision supplying a new one.
    let SqlPool::Sqlite(new) = cache
        .get_pool("orders", &settings(&b))
        .await
        .expect("new pool")
    else {
        panic!("sqlite");
    };
    let (tables,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM sqlite_master WHERE name = 'identity_marker'")
            .fetch_one(&new)
            .await
            .expect("new database");
    assert_eq!(
        tables, 0,
        "a different connection string must reach another database"
    );
    let (marker,): (i64,) = sqlx::query_as("SELECT value FROM identity_marker")
        .fetch_one(&old)
        .await
        .expect("old caller can finish");
    assert_eq!(marker, 1);
    cache.evict_all().await;
    old.close().await;
    new.close().await;
}

/// A revision committed while this node cannot read the deployment store
/// reaches its serving generation once the store reads again, with no
/// further edit: the reconciler keeps retrying the gap between the committed
/// head and what it applied, and until then the node keeps serving what it
/// had.
///
/// The commit comes from a second state over the same database — another
/// node, or this node's route whose post-commit rebuild was lost — so this
/// node learns of it only through the reconciler. This node's own loop is
/// slowed to never tick on its own, and its ticks are driven here one at a
/// time: a loop polling on its own could apply the commit before the reads
/// are broken.
#[tokio::test]
async fn a_node_recovers_a_committed_deployment_after_a_failed_reconcile() {
    let db = common::ScratchDir::new("recover_after_commit");
    let mut config = AppConfig::default();
    config.storage.url = format!("{}?mode=rwc", db.url());
    config.cluster.epoch_poll_interval_ms = 3_600_000;
    let (state, pool) = common::test_state_and_pool(config.clone()).await;
    let app = orion::server::build_router(state.clone());
    common::create_and_activate_channel(
        &app,
        "recover-after-commit",
        common::echo_workflow("recover"),
    )
    .await;
    let applied = state.reconciler.local_status().applied_revision;

    // The other writer commits a revision without the channel.
    let other = common::test_state_with_config(config).await;
    let other_app = orion::server::build_router(other.clone());
    common::remove_member(&other_app, "channel", "recover-after-commit").await;
    let (committed, _) = other.deployments.current_head();
    assert!(committed > applied);

    // Break this node's reads of the deployment store before its reconciler
    // runs.
    let orion::storage::DbPool::Sqlite(sqlite) = &pool else {
        panic!("sqlite");
    };
    sqlx::query("ALTER TABLE deployment_revisions RENAME TO temporarily_unavailable_revisions")
        .execute(sqlite)
        .await
        .expect("break deployment reads");
    for _ in 0..3 {
        assert!(
            !orion::cluster::reconcile_v2_state_once(&state).await,
            "a tick that cannot read the store fails"
        );
    }
    assert_eq!(
        state.reconciler.local_status().applied_revision,
        applied,
        "a node that cannot read the store applies nothing"
    );
    assert!(
        state
            .runtime
            .load()
            .channels
            .get_by_name("recover-after-commit")
            .is_some(),
        "and keeps serving what it had"
    );

    sqlx::query("ALTER TABLE temporarily_unavailable_revisions RENAME TO deployment_revisions")
        .execute(sqlite)
        .await
        .expect("restore deployment reads");
    tokio::time::timeout(Duration::from_secs(3), async {
        while state.reconciler.local_status().applied_revision < committed {
            orion::cluster::reconcile_v2_state_once(&state).await;
        }
    })
    .await
    .expect("the next tick applies it without another edit");
    assert!(
        state
            .runtime
            .load()
            .channels
            .get_by_name("recover-after-commit")
            .is_none()
    );
    assert!(!state.reload_degraded.load(Ordering::Acquire));
    state.tasks.shutdown(Duration::from_secs(2)).await;
    other.tasks.shutdown(Duration::from_secs(2)).await;
}

/// A read snapshot over the deployment store holds across a concurrent
/// deployment: a revision committed after the snapshot's first read is
/// invisible to every later read in it, and visible once it is released.
#[tokio::test]
async fn a_definition_read_snapshot_holds_across_a_concurrent_commit() {
    let db = common::ScratchDir::new("definition_snapshot");
    let mut config = AppConfig::default();
    config.storage.url = format!("{}?mode=rwc", db.url());
    let (state, pool) = common::test_state_and_pool(config).await;
    let app = orion::server::build_router(state.clone());
    common::create_and_activate_channel(
        &app,
        "consistent-definitions",
        common::echo_workflow("snapshot"),
    )
    .await;
    let (first_head, _) = state.deployments.current_head();
    let revisions = "SELECT revision FROM deployment_revisions ORDER BY revision";
    let empty = || sea_query_sqlx::SqlxValues(sea_query::Values(Vec::new()));
    let mut snapshot = pool.begin_snapshot_tx().await.expect("snapshot");
    let first: Vec<(i64,)> = snapshot
        .fetch_all_as(revisions, empty())
        .await
        .expect("first snapshot read");
    assert_eq!(first.last(), Some(&(first_head as i64,)));

    common::remove_member(&app, "channel", "consistent-definitions").await;
    let (second_head, _) = state.deployments.current_head();
    assert!(second_head > first_head);

    let pinned: Vec<(i64,)> = snapshot
        .fetch_all_as(revisions, empty())
        .await
        .expect("same snapshot");
    assert_eq!(
        pinned, first,
        "later reads retain the first read's revision"
    );
    snapshot.commit().await.expect("release read snapshot");
    let current: Vec<(i64,)> = pool
        .fetch_all_as(revisions, empty())
        .await
        .expect("current state");
    assert_eq!(current.last(), Some(&(second_head as i64,)));
}
