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
//! [`model_admission`] is the worker behind the model routes: it drains the
//! admission queue the node holds, resolving each job's storage connector
//! from the registry and recording the verdict through the repository — the
//! two things `model::admission` deliberately takes as parameters.
//!
//! [`models`] is the generation-side of the model subsystem: the quarantine
//! of a workflow that names a model the generation cannot serve — the same
//! treatment a workflow naming an unavailable plugin function gets — and the
//! preload that warms what a published generation will run. Both need the
//! channel rows, the workflow rows and the node's live components, which is
//! why they are here and not in `model/`.
//!
//! [`reload`] rebuilds that generation, and restarts the Kafka consumer that
//! rides along with it. It was `engine::reload`, and that was the largest of
//! the upward dependency edges in the tree: a module the whole request path
//! sits *below* reached up into `server::state` for `AppState` and into
//! `bootstrap` for `start_kafka_ingest`. It is not an engine concern — it
//! reads the database, rebuilds the channel estate and restarts a Kafka
//! consumer, and the engine is one of the things it swaps. Here it depends
//! downward on all of them.

pub mod authority;
pub mod boot_packages;
pub mod generation;
pub mod limiter_prune;
pub mod load_issues;
pub mod model_admission;
pub mod models;
pub mod reload;
pub mod tasks;

pub use generation::{RuntimeGeneration, RuntimeHandle};
pub use reload::{
    ReloadOpts, reconcile_deployment_revision, reload_engine, reload_engine_with_opts,
    resync_from_db, spawn_kafka_restart_supervisor,
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
/// The `Registry` a generation compiles on before its plugins, from this
/// node's live components ([`crate::engine::serving_registry`]), with the
/// Kafka publisher swapped in when a producer exists. What a reload that
/// changes the plugin set starts from; a reload that keeps it compiles on the
/// running engine's registry instead.
pub fn build_registry(state: &crate::server::state::AppState) -> dataflow_rs::Registry {
    crate::engine::serving_registry(handler_deps(state), state.kafka.producers.clone())
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
        mongo_pool_cache: state.caches.mongo_pool_cache.clone(),
        models: state.models.clone(),
        models_config: &state.config.models,
        providers: state.composition.providers.clone(),
    }
}
