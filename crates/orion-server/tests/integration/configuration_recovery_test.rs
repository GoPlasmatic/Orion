//! Configuration correctness across commit, reload and connection creation.

use std::sync::atomic::Ordering;
use std::time::Duration;

use orion::cluster::EpochScope;
use orion::config::AppConfig;
use orion::connector::pool_cache::{SqlPool, SqlPoolCache};
use orion::storage::repositories::audit_logs::AuditEvent;

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

#[tokio::test]
async fn a_single_node_recovers_a_committed_change_after_a_failed_reload() {
    let mut config = AppConfig::default();
    config.cluster.epoch_poll_interval_ms = 10;
    let (state, pool) = common::test_state_and_pool(config).await;
    let app = orion::server::build_router(state.clone());
    let (channel, _) = common::create_and_activate_channel_full(
        &app,
        "recover-after-commit",
        common::echo_workflow("recover"),
        serde_json::json!({}),
    )
    .await;
    let before = state.cluster.last_seen_epoch.load(Ordering::Acquire);
    let mut write = state
        .repos
        .audited(AuditEvent {
            principal: "test".to_string(),
            action: "status_archived".to_string(),
            resource_type: "channel".to_string(),
            resource_id: channel.clone(),
            details: None,
        })
        .await
        .expect("begin");
    state
        .repos
        .channels
        .archive_tx(write.tx(), &channel)
        .await
        .expect("archive");
    let epoch = write
        .commit_published(Some(EpochScope::Definitions))
        .await
        .expect("commit")
        .expect("epoch");
    // Simulate losing the route's post-commit work: only the durable epoch
    // remains to tell the reconciler what still needs to reach the engine.
    assert_eq!(epoch, before + 1);
    assert!(
        state
            .runtime
            .load()
            .channels
            .get_by_name("recover-after-commit")
            .is_some()
    );
    let orion::storage::DbPool::Sqlite(sqlite) = &pool else {
        panic!("sqlite");
    };
    sqlx::query("ALTER TABLE workflows RENAME TO temporarily_unavailable_workflows")
        .execute(sqlite)
        .await
        .expect("break definition reads");
    orion::cluster::start_cluster_tasks(&state);
    tokio::time::timeout(Duration::from_secs(3), async {
        while !state.reload_degraded.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("reload fails visibly");
    assert_eq!(
        state.cluster.last_seen_epoch.load(Ordering::Acquire),
        before
    );
    sqlx::query("ALTER TABLE temporarily_unavailable_workflows RENAME TO workflows")
        .execute(sqlite)
        .await
        .expect("restore definition reads");
    tokio::time::timeout(Duration::from_secs(3), async {
        while state.cluster.last_seen_epoch.load(Ordering::Acquire) < epoch {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("reconciler retries without another edit");
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
}

#[tokio::test]
async fn a_definition_read_snapshot_holds_across_a_concurrent_commit() {
    let db = common::ScratchDir::new("definition_snapshot");
    let mut config = AppConfig::default();
    config.storage.url = format!("{}?mode=rwc", db.url());
    let (state, pool) = common::test_state_and_pool(config).await;
    let app = orion::server::build_router(state.clone());
    let (channel, workflow) = common::create_and_activate_channel_full(
        &app,
        "consistent-definitions",
        common::echo_workflow("snapshot"),
        serde_json::json!({}),
    )
    .await;
    let mut snapshot = pool.begin_snapshot_tx().await.expect("snapshot");
    let empty = || sea_query_sqlx::SqlxValues(sea_query::Values(Vec::new()));
    let first: Vec<(String,)> = snapshot
        .fetch_all_as(
            "SELECT channel_id FROM channels WHERE status = 'active'",
            empty(),
        )
        .await
        .expect("first snapshot read");
    assert_eq!(first, vec![(channel.clone(),)]);
    let mut change = state
        .repos
        .audited(AuditEvent {
            principal: "test".to_string(),
            action: "archive".to_string(),
            resource_type: "channel".to_string(),
            resource_id: channel,
            details: None,
        })
        .await
        .expect("writer");
    state
        .repos
        .workflows
        .archive_tx(change.tx(), &workflow)
        .await
        .expect("archive workflow");
    change
        .commit_published(Some(EpochScope::Definitions))
        .await
        .expect("commit concurrent change");
    let pinned: Vec<(String,)> = snapshot
        .fetch_all_as(
            "SELECT workflow_id FROM workflows WHERE status = 'active'",
            empty(),
        )
        .await
        .expect("same snapshot");
    assert_eq!(
        pinned,
        vec![(workflow,)],
        "later reads retain the first read's revision"
    );
    snapshot.commit().await.expect("release read snapshot");
    assert!(
        state
            .repos
            .workflows
            .list_active()
            .await
            .expect("current state")
            .is_empty()
    );
}
