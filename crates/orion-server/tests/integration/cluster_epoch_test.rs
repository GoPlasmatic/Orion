//! Epoch-bus send side (multi-instance-ha A2): admin mutations that change
//! the active set must advance the config epoch — even with cluster mode
//! disabled (the counter stays monotonic so enabling cluster later is sane).
//! The receive side (watcher resync on another node) lives in tests/cluster.

use std::sync::atomic::Ordering;

use crate::common;

#[tokio::test]
async fn admin_mutations_advance_config_epoch() {
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());

    let start_epoch = state.cluster.repo.get_epoch().await.expect("epoch").epoch;
    assert_eq!(start_epoch, 0);

    // Workflow activate + channel activate → two audit_and_reload calls.
    common::create_and_activate_channel(&app, "epoch-ch", common::simple_log_workflow("Epoch WF"))
        .await;

    let row = state.cluster.repo.get_epoch().await.expect("epoch");
    assert!(
        row.epoch >= 2,
        "workflow + channel activation must each bump the epoch, got {}",
        row.epoch
    );
    // The mutating node marks its own bumps as already applied.
    assert_eq!(
        state.cluster.last_seen_epoch.load(Ordering::Acquire),
        row.epoch
    );
    // Breaker epoch untouched by config mutations.
    assert_eq!(row.breaker_epoch, 0);
}

/// A channel or workflow mutation must record the scope that says so, and a
/// connector mutation must record its own.
///
/// The scope is what lets a peer size its resync. Before it existed the epoch
/// was a bare counter, so every node answering every bump reloaded all
/// connectors and evicted every cached pool.
#[tokio::test]
async fn a_bump_records_what_changed() {
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());

    common::create_and_activate_channel(&app, "scope-ch", common::simple_log_workflow("Scope WF"))
        .await;
    let row = state.cluster.repo.get_epoch().await.expect("epoch");
    assert_eq!(
        orion::cluster::EpochScope::parse(&row.epoch_scope),
        orion::cluster::EpochScope::Definitions,
        "a channel activation touches no connector"
    );

    common::create_connector(&app, common::cache_connector_memory("scope-conn")).await;
    let row = state.cluster.repo.get_epoch().await.expect("epoch");
    assert_eq!(
        orion::cluster::EpochScope::parse(&row.epoch_scope),
        orion::cluster::EpochScope::Connectors,
    );
}

/// The reconnect storm, gone: a `Definitions` resync must leave a peer's
/// cached connection pools open.
///
/// `resync_from_db` used to reload the connector registry and call
/// `evict_all()` on the SQL, MongoDB and cache pool caches on *every* epoch
/// tick, whatever the originating mutation touched — so one workflow
/// activation dropped every pooled connection on every node in the fleet. The
/// eviction handler closes what it drops, which is what makes it observable
/// here.
#[tokio::test]
async fn a_definitions_resync_leaves_cached_pools_open() {
    let db = common::ScratchDir::new("epoch_scope_pools");
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());
    common::create_connector(
        &app,
        common::db_connector_sqlite("pool-conn", &format!("{}?mode=rwc", db.url())),
    )
    .await;

    let config = state
        .connector_registry
        .get("pool-conn")
        .await
        .expect("the connector is registered");
    let orion::connector::ConnectorConfig::Db(db_config) = config.as_ref() else {
        panic!("expected a db connector");
    };
    let pool = state
        .caches
        .sql_pool_cache
        .get_pool("pool-conn", db_config)
        .await
        .expect("pool opens");
    assert!(!pool.is_closed());

    orion::engine::resync_from_db(&state, orion::cluster::EpochScope::Definitions)
        .await
        .expect("resync");
    assert!(
        !pool.is_closed(),
        "a workflow or channel change must not drop a peer's connector pools"
    );

    // The complement: a connector change still evicts, because the endpoint or
    // the credentials behind a live connection may now be wrong. The evict
    // handler closes on a detached task, so give it a moment.
    orion::engine::resync_from_db(&state, orion::cluster::EpochScope::Connectors)
        .await
        .expect("resync");
    for _ in 0..50 {
        if pool.is_closed() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        pool.is_closed(),
        "a connector change must still drop the cached pools"
    );
}
