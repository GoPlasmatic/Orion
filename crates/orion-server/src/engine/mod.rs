//! The engine layer: what a workflow may call, how the engine is assembled
//! from stored rows, and how one channel's run is driven.
//!
//! F45: this file used to carry all three of those plus the glob matcher, the
//! rollout arithmetic and the lock accessors in one 468-line module. They are
//! now three:
//!
//! - [`handlers`] — the function-name vocabulary and the handler construction
//!   behind Orion's own entries.
//! - [`loader`] — stored channels + workflows → the dataflow-rs workflow set,
//!   with the per-channel quarantine.
//! - [`runner`] — running one channel, and the instrumented lock accessors.
//! - [`observer`] — always-on per-task timing, including the sync built-ins no
//!   host can otherwise reach.
//!
//! Everything public is re-exported here, so `crate::engine::…` paths are
//! unchanged.

/// Where the engine records the **code** of each failed task, for workflows
/// that must branch on *why* a step failed (#280).
///
/// `metadata`, not `data` or `temp_data`, and that placement is the point:
/// `metadata` is stripped from the trace-read projection and never reaches the
/// sync response body, whereas `temp_data` would ride out through the
/// persisted trace's `result_json` and `data` would go straight to the caller.
///
/// The `_orion_` prefix follows the reserved namespace (`_orion_call_depth`,
/// `_orion_call_chain`) rather than the bare `metadata.errors` dataflow-rs
/// would otherwise default to — that name is both a plausible caller-supplied
/// key and outside the namespace Orion documents as its own.
///
/// **The prefix is a convention, not an enforced namespace.** A caller can put
/// `_orion_call_depth` in an envelope today and nothing strips it, so this key
/// is force-cleared at every ingress — see `build_request_metadata` and the
/// Kafka ingress builder — and reset on `channel_call`, where the child would
/// otherwise inherit and report the parent's failures as its own.
///
/// Records carry `{workflow_id, task_id, code, status}` and **never a
/// message**: see the note in `docs/src/reference/errors.md`.
pub const ERROR_CONTEXT_PATH: &str = "metadata._orion_errors";

/// The bare key under `metadata`, for the ingress stamping sites.
pub const ERROR_CONTEXT_KEY: &str = "_orion_errors";

/// Clear the engine-owned error records from a metadata object.
///
/// Lives beside the constant so a new ingress has one call to make rather than
/// a three-line idiom to copy — and so widening this to the whole `_orion_`
/// prefix later is a one-line change here instead of an audit of call sites.
pub fn clear_error_context(metadata: &mut serde_json::Value) {
    if let Some(map) = metadata.as_object_mut() {
        map.remove(ERROR_CONTEXT_KEY);
    }
}

/// The metadata key the `[vars]` config section is stamped under.
///
/// Platform-reserved, in the same sense as `channel` and `cookies`: an
/// operator declares the values and every ingress stamps them, overwriting
/// whatever the caller sent. Envelope mode merges caller-supplied `metadata`
/// wholesale, so without that a request could forge the topic prefix its own
/// run publishes to.
pub const VARS_KEY: &str = "vars";

/// Force `metadata.vars` to the instance's declared vars.
///
/// `None` — the instance declares no vars — *removes* the key rather than
/// writing an empty object. Both halves matter: the removal is what makes the
/// key unforgeable on an instance that declares nothing, and leaving the key
/// absent means a workflow reading `metadata.vars.x` sees the same missing
/// value whether the section is empty or the entry is.
///
/// Vars are stamped rather than held beside the message on purpose. They are
/// deployment configuration, so they *should* appear in the trace — an
/// operator asking "which topic did this run publish to?" is asking to see
/// them. That is the whole distinction between this and
/// [`secrets`], which the engine holds precisely so it
/// cannot record them.
pub fn stamp_vars(metadata: &mut serde_json::Value, vars: Option<&serde_json::Value>) {
    if let Some(map) = metadata.as_object_mut() {
        match vars {
            Some(values) => {
                map.insert(VARS_KEY.to_string(), values.clone());
            }
            None => {
                map.remove(VARS_KEY);
            }
        }
    }
}

pub mod functions;
pub mod handlers;
pub mod loader;
pub mod observer;
pub mod operators;
pub mod profile;
pub mod refs;
pub mod reload;
pub mod runner;
pub mod secrets;
pub mod steps;
pub mod utils;

pub use handlers::{
    CONNECTOR_FUNCTIONS, HandlerDeps, build_custom_functions, is_known_function, known_functions,
    register_kafka_publisher, required_connector_types, requires_mongo_database,
    suggest_known_function,
};
pub use loader::{
    CUSTOM_HANDLER_FUNCTIONS, HandlerScreen, build_engine_workflows, filter_channels,
};
pub use observer::MetricsObserver;
pub use refs::{ConnectorRef, channel_call_targets, connector_refs};
pub use reload::{
    ReloadOpts, reload_engine, reload_engine_with_opts, resync_from_db,
    spawn_kafka_restart_supervisor,
};
pub use runner::{EngineCallResult, EngineHandle, TraceCapture, run_for_channel};
pub use secrets::ResolvedSecrets;
pub use steps::{MAX_STEP_DEPTH, is_group, leaf_tasks, walk_steps};
