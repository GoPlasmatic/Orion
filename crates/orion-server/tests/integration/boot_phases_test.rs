//! The boot sequence, phase by phase (`orion::bootstrap::assemble`): the
//! identity a node resolves, what a fresh node reports, and what a restart
//! does with a committed head it cannot fully load.

use std::sync::Arc;

use serde_json::json;

use crate::common;
use orion::config::AppConfig;

/// Boot again over `pool`, as a restarted node would.
async fn restart(
    config: AppConfig,
    pool: orion::storage::DbPool,
) -> Result<orion::bootstrap::Assembled, String> {
    orion::bootstrap::assemble(
        orion::bootstrap::AssembleParams {
            config: Arc::new(config),
            pool,
            metrics_handle: orion::metrics::init_metrics_with_instance(None),
        },
        orion::extensions::CompositionRoot::community(),
    )
    .await
    .map_err(|e| e.to_string())
}

/// A node with no configured identity mints one; a configured one is kept.
#[tokio::test]
async fn the_storage_phase_resolves_the_nodes_identity_once() {
    let minted = orion::bootstrap::open_storage(
        Arc::new(AppConfig::default()),
        common::memory_pool().await,
        None,
    )
    .await
    .unwrap();
    assert!(!minted.config.cluster.instance_id.is_empty());
    assert_eq!(
        minted.cluster.instance_id,
        minted.config.cluster.instance_id
    );

    let mut config = AppConfig::default();
    config.cluster.instance_id = "node-7".into();
    let kept = orion::bootstrap::open_storage(Arc::new(config), common::memory_pool().await, None)
        .await
        .unwrap();
    assert_eq!(kept.config.cluster.instance_id, "node-7");
    assert_eq!(kept.cluster.instance_id, "node-7");
}

/// A cluster store composed on a node whose cluster is disabled is unused,
/// not an error: the node boots on its in-process stores (and logs it).
#[tokio::test]
async fn a_registered_store_with_the_cluster_disabled_still_boots() {
    let store = Arc::new(crate::memory_store::CountingStore::default());
    // Down, so a boot that pinged it would fail.
    store.down.store(true, std::sync::atomic::Ordering::SeqCst);
    let state = common::test_state_with_composition(
        AppConfig::default(),
        orion::extensions::CompositionRoot::community().with_cluster_store(store),
    )
    .await;
    assert!(state.cluster.shared.is_none());
    assert!(state.cluster.shared.is_none());
    assert!(state.ready.load(std::sync::atomic::Ordering::Acquire));
}

/// Cluster mode is not Community's: a Community node with
/// `cluster.enabled = true` and no composed cluster store does not boot,
/// and says what it needs.
#[tokio::test]
async fn community_does_not_boot_a_cluster() {
    let mut config = AppConfig::default();
    config.cluster.enabled = true;
    let Err(refused) = restart(config, common::memory_pool().await).await else {
        unreachable!("Community ships no cluster store");
    };
    assert!(
        refused.contains(orion::cluster::NO_CLUSTER_STORE),
        "{refused}"
    );
}

/// A fresh node serves nothing, says it is ready, and has recorded its own
/// status for its peers to read.
#[tokio::test]
async fn a_fresh_node_is_ready_with_no_head_and_has_recorded_its_status() {
    let state = common::test_state_with_config(AppConfig::default()).await;
    assert!(state.ready.load(std::sync::atomic::Ordering::Acquire));
    assert_eq!(state.deployments.current_head().0, 0);
    assert!(state.runtime.load().workflows().is_empty());

    let nodes = state.repos.runtime_nodes.list_nodes().await.unwrap();
    assert!(
        nodes
            .iter()
            .any(|n| n.node_id == state.config.cluster.instance_id && n.applied_revision == 0),
        "{nodes:?}"
    );
}

/// A committed connection that fails to load stops a restart that asked for
/// that, naming it; without the setting the node starts and reports it.
#[tokio::test]
async fn a_restart_over_a_connection_that_fails_to_load_obeys_the_setting() {
    let (state, pool) = common::test_state_and_pool(AppConfig::default()).await;
    let app = orion::server::build_router(state);
    common::create_connector(
        &app,
        json!({
            "name": "boot-broken",
            "connector_type": "http",
            "config": {
                "url": "https://api.example.com",
                "method": "GET",
                "auth": {"type": "bearer", "token": "env://ORION_BOOT_PHASES_DEFINITELY_UNSET"}
            }
        }),
    )
    .await;

    let mut strict = AppConfig::default();
    strict.engine.fail_on_connector_load_error = true;
    let Err(refusal) = restart(strict, pool.clone()).await else {
        panic!("a strict restart must refuse the head");
    };
    assert!(
        refusal.contains("refused to start") && refusal.contains("boot-broken"),
        "{refusal}"
    );

    let lenient = restart(AppConfig::default(), pool).await.unwrap();
    assert!(lenient.state.deployments.current_head().0 > 0);
    let issues = lenient.state.connector_registry.load_issues().await;
    assert!(
        issues.iter().any(|i| i.connector == "boot-broken"),
        "{issues:?}"
    );
}
