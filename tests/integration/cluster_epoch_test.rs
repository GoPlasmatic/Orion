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
    assert_eq!(state.cluster.last_seen_epoch.load(Ordering::Acquire), row.epoch);
    // Breaker epoch untouched by config mutations.
    assert_eq!(row.breaker_epoch, 0);
}
