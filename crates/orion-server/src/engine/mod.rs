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

pub mod functions;
pub mod handlers;
pub mod loader;
pub mod observer;
pub mod operators;
pub mod profile;
pub mod refs;
pub mod reload;
pub mod runner;
pub mod utils;

pub use handlers::{
    CONNECTOR_FUNCTIONS, HandlerDeps, build_custom_functions, is_known_function, known_functions,
    register_kafka_publisher, required_connector_types, requires_mongo_database,
};
pub use loader::{CUSTOM_HANDLER_FUNCTIONS, build_engine_workflows, filter_channels};
pub use observer::MetricsObserver;
pub use refs::{ConnectorRef, channel_call_targets, connector_refs};
pub use reload::{
    ReloadOpts, reload_engine, reload_engine_with_opts, resync_from_db,
    spawn_kafka_restart_supervisor,
};
pub use runner::{EngineCallResult, EngineHandle, TraceCapture, run_for_channel};
