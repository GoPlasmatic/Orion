//! Process-level runtime concerns: the things that own a *node* rather than a
//! request.
//!
//! [`tasks`] supervises the long-lived background tasks. It lives here rather
//! than in `queue/` because it is not about traces: the trace dispatcher, the
//! persistence workers, the audit writer, the retention jobs and the cluster
//! epoch watcher are owned by four different modules, and "is every one of
//! them still alive?" is a question about the node.
//!
//! [`generation`] is that serving generation — the engine and the channel
//! estate built from the same rows — and the handle that publishes it. It
//! lives here for the same reason [`tasks`] does: it belongs to the node, and
//! it is injected *downward* into `engine`, `channel`, `kafka` and `queue`
//! rather than reaching up into any of them.
//!
//! [`reload`] rebuilds that generation, and restarts the Kafka consumer that
//! rides along with it. It was `engine::reload`, and that was the largest of
//! the upward dependency edges in the tree: a module the whole request path
//! sits *below* reached up into `server::state` for `AppState` and into
//! `bootstrap` for `start_kafka_ingest`. It is not an engine concern — it
//! reads the database, rebuilds the channel estate and restarts a Kafka
//! consumer, and the engine is one of the things it swaps. Here it depends
//! downward on all of them.

pub mod generation;
pub mod reload;
pub mod tasks;

pub use generation::{RuntimeGeneration, RuntimeHandle};
pub use reload::{
    ReloadOpts, reload_engine, reload_engine_with_opts, resync_from_db,
    spawn_kafka_restart_supervisor,
};
pub use tasks::{Criticality, Shutdown, TaskGuard, TaskRegistry, TaskReport, TaskState};

/// Borrow the handler dependencies straight off a live `AppState`.
///
/// The dry-run engine in `POST /workflows/{id}/test` must be built from the
/// *same* registries and pools as the serving engine — a copy that drifted
/// would make dry-run results a lie about production.
///
/// This was `HandlerDeps::from_state` in `engine::handlers`, which meant the
/// engine — a module every request path sits below — named `AppState`. The
/// mapping is a runtime concern: it says which of the node's live components
/// a handler gets, and `engine` only has to accept them.
/// Every handler an engine built from this node's live components carries:
/// Orion's own, with the Kafka publisher swapped in when a producer exists.
///
/// The one assembly boot, reload and the test endpoint share. A reload that
/// must rebuild the engine — the plugin set changed, so `with_new_workflows`
/// cannot carry the old handler map across — used to have no way to
/// reproduce what boot registered; this is that way.
pub fn build_handlers(
    state: &crate::server::state::AppState,
) -> std::collections::HashMap<String, dataflow_rs::BoxedFunctionHandler> {
    let mut fns = crate::engine::build_custom_functions(handler_deps(state));
    if let Some(producers) = &state.kafka.producers {
        crate::engine::register_kafka_publisher(
            &mut fns,
            state.connector_registry.clone(),
            producers.clone(),
        );
    }
    fns
}

pub fn handler_deps(state: &crate::server::state::AppState) -> crate::engine::HandlerDeps<'_> {
    crate::engine::HandlerDeps {
        registry: state.connector_registry.clone(),
        client: state.http_client.clone(),
        runtime: state.runtime.clone(),
        jwks: state.jwks.clone(),
        engine_config: &state.config.engine,
        query_config: &state.config.query,
        write_config: &state.config.write,
        cache_pool: state.caches.cache_pool.clone(),
        sql_pool_cache: state.caches.sql_pool_cache.clone(),
        mongo_pool_cache: state.caches.mongo_pool_cache.clone(),
        smtp_pool_cache: state.caches.smtp_pool_cache.clone(),
    }
}
