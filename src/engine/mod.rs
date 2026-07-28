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
//!
//! Everything public is re-exported here, so `crate::engine::…` paths are
//! unchanged.

pub mod functions;
pub mod handlers;
pub mod loader;
pub mod profile;
pub mod reload;
pub mod runner;
pub mod utils;

pub use handlers::{
    CONNECTOR_FUNCTIONS, HandlerDeps, KNOWN_FUNCTIONS, build_custom_functions,
    register_kafka_publisher, required_connector_types, requires_mongo_database,
};
pub use loader::{CUSTOM_HANDLER_FUNCTIONS, build_engine_workflows, filter_channels};
pub use reload::{
    ReloadOpts, reload_engine, reload_engine_with_opts, resync_from_db,
    spawn_kafka_restart_supervisor,
};
pub use runner::{EngineCallResult, acquire_engine_read, acquire_engine_write, run_for_channel};
