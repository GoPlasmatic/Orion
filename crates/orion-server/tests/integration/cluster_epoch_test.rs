//! What a deployment rebuild leaves alone. The cross-node half (a peer's
//! reconciler taking up a committed revision) lives in tests/cluster.

use crate::common;

/// No reconnect storm: rebuilding the generation for a deployment that does
/// not touch a connection must leave its cached pools open.
///
/// Evicting every SQL and cache pool on every change, whatever it touched,
/// would make one workflow deployment drop every pooled connection on every
/// node in the fleet. A deployment rebuild keys pools on the connection's
/// settings and keeps them; the eviction handler closes what it drops, which
/// is what makes a wrong eviction observable here.
#[tokio::test]
async fn a_definitions_deployment_leaves_cached_pools_open() {
    let db = common::ScratchDir::new("epoch_scope_pools");
    let mut config = orion::config::AppConfig::default();
    config.engine.sqlite_connector_root = db.path().display().to_string();
    let state = common::test_state_with_config(config).await;
    let app = orion::server::build_router(state.clone());
    common::create_connector(
        &app,
        common::db_connector_sqlite(
            "pool-conn",
            &format!("sqlite:{}/app.db?mode=rwc", db.path().display()),
        ),
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

    // A deployment that adds a workflow and a channel, and touches no
    // connection: the generation is rebuilt, the pool is not dropped.
    let before = state.runtime.load().id;
    common::create_and_activate_channel(
        &app,
        "pool-keep-ch",
        common::simple_log_workflow("Pool Keep WF"),
    )
    .await;
    assert!(
        state.runtime.load().id > before,
        "the deployment rebuilt the generation"
    );
    // The evict handler closes on a detached task, so give a wrong eviction
    // the time it would need to show.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !pool.is_closed(),
        "a workflow or channel deployment must not drop the connector pools"
    );
}
