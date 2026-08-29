//! Process-level runtime concerns: the things that own a *node* rather than a
//! request.
//!
//! [`tasks`] supervises the long-lived background tasks. It lives here rather
//! than in `queue/` because it is not about traces: the trace dispatcher, the
//! persistence workers, the audit writer, the retention jobs and the cluster
//! epoch watcher are owned by four different modules, and "is every one of
//! them still alive?" is a question about the node.
//!
//! [`reload`] rebuilds the serving generation — engine, channel registry, and
//! the Kafka consumer that rides along with them. It was `engine::reload`, and
//! that was the largest of the upward dependency edges in the tree: a module
//! the whole request path sits *below* reached up into `server::state` for
//! `AppState` and into `bootstrap` for `start_kafka_ingest`. It is not an
//! engine concern — it reads the database, republishes the channel registry
//! and restarts a Kafka consumer, and the engine is one of the three things it
//! swaps. Here it depends downward on all of them.

pub mod reload;
pub mod tasks;

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
pub fn handler_deps(state: &crate::server::state::AppState) -> crate::engine::HandlerDeps<'_> {
    crate::engine::HandlerDeps {
        registry: state.connector_registry.clone(),
        client: state.http_client.clone(),
        engine: state.engine.clone(),
        channel_registry: state.channel_registry.clone(),
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
