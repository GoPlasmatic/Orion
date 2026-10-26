//! Process-level runtime concerns: the things that own a *node* rather than a
//! request.
//!
//! [`tasks`] supervises the long-lived background tasks. It lives here rather
//! than in `queue/` because it is not about traces: the trace dispatcher, the
//! persistence workers, the audit writer, the retention jobs and the cluster
//! reconciler are owned by four different modules, and "is every one of
//! them still alive?" is a question about the node.
//!
//! [`generation`] is that serving generation — the engine and the channel
//! estate built from the same rows — and the handle that publishes it. It
//! lives here for the same reason [`tasks`] does: it belongs to the node, and
//! it is injected *downward* into `engine`, `channel`, `cron` and `queue`
//! rather than reaching up into any of them.
//!
//! [`reload`] builds that generation from a committed deployment revision.
//! It is not an engine concern — it reads the deployment store and rebuilds
//! the channel estate, and the engine is one of the things it swaps. Here it
//! depends downward on all of them.

pub mod authority;
pub mod definitions;
pub mod deployment_gc;
pub mod execute;
pub mod functions;
pub mod generation;
pub mod limiter_prune;
pub mod load_issues;
pub mod reload;
pub mod tasks;
pub mod workflow_providers;

pub use execute::execute_admitted_on_generation;
pub use generation::{RuntimeGeneration, RuntimeHandle};
pub use reload::{check_candidate, reconcile_deployment_revision};
pub use tasks::{Criticality, Shutdown, TaskGuard, TaskRegistry, TaskReport, TaskState};

/// Borrow the handler dependencies straight off a live `AppState`.
///
/// The dry-run engine in `POST /definitions/test` must be built from the
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
    let hosts = functions::NodeHosts::new(
        state.runtime.clone(),
        state.exprs.clone(),
        state.channel_loader.clone(),
        state.composition.providers().clone(),
    );
    crate::engine::HandlerDeps {
        registry: state.connector_registry.clone(),
        client: state.http_client.clone(),
        channel_calls: hosts.channel_calls,
        exprs: state.exprs.clone(),
        jwks: state.jwks.clone(),
        engine_config: &state.config.engine,
        query_config: &state.config.query,
        write_config: &state.config.write,
        cache_pool: state.caches.cache_pool.clone(),
        namespaces: hosts.namespaces,
        sql_pool_cache: state.caches.sql_pool_cache.clone(),
        connector_calls: hosts.connector_calls,
    }
}
