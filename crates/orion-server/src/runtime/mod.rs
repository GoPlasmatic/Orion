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
//! it is injected *downward* into `engine`, `channel`, `cron` and `queue`
//! rather than reaching up into any of them.
//!
//! [`reload`] rebuilds that generation. It was `engine::reload`, and that was
//! the largest of the upward dependency edges in the tree: a module the whole
//! request path sits *below* reached up into `server::state` for `AppState`.
//! It is not an engine concern — it reads the database and rebuilds the
//! channel estate, and the engine is one of the things it swaps. Here it
//! depends downward on all of them.

pub mod authority;
pub mod definitions;
pub mod generation;
pub mod limiter_prune;
pub mod load_issues;
pub mod reload;
pub mod tasks;

pub use generation::{RuntimeGeneration, RuntimeHandle};
pub use reload::{reconcile_deployment_revision, reload_engine, resync_from_db};
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
/// The `Registry` a generation compiles on before its plugins, from this
/// node's live components ([`crate::engine::serving_registry`]). What a reload that
/// changes the plugin set starts from; a reload that keeps it compiles on the
/// running engine's registry instead.
pub fn build_registry(state: &crate::server::state::AppState) -> dataflow_rs::Registry {
    crate::engine::serving_registry(handler_deps(state))
}

pub fn handler_deps(state: &crate::server::state::AppState) -> crate::engine::HandlerDeps<'_> {
    crate::engine::HandlerDeps {
        registry: state.connector_registry.clone(),
        client: state.http_client.clone(),
        runtime: state.runtime.clone(),
        exprs: state.exprs.clone(),
        jwks: state.jwks.clone(),
        engine_config: &state.config.engine,
        query_config: &state.config.query,
        write_config: &state.config.write,
        cache_pool: state.caches.cache_pool.clone(),
        channel_loader: state.channel_loader.clone(),
        sql_pool_cache: state.caches.sql_pool_cache.clone(),
        providers: state.composition.providers.clone(),
    }
}
