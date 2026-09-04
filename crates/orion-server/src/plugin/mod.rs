//! The plugin sandbox: WebAssembly Components behind the `orion:plugin` WIT
//! world, loaded from bytes, described by a manifest, and run as ordinary
//! engine functions.
//!
//! Sits beside `engine` and below `runtime`: it produces
//! [`FunctionEntry`](crate::engine::FunctionEntry) values and
//! `AsyncFunctionHandler` implementations for a generation to carry, and
//! names nothing above them but the task supervisor it hands the ticker to.
//! See `plugin.md` for the design.

pub mod error;
pub mod handler;
pub mod limits;
pub mod loader;
pub mod manifest;
pub mod runtime;
pub mod ticker;

pub use error::{Category, Failure};
pub use handler::PluginFunctionHandler;
pub use limits::{HostState, Limits};
pub use loader::{LoadedPlugin, PluginLoadIssue, PluginSet, load_active};
pub use manifest::{ABI, Manifest};
pub use runtime::{EPOCH_TICK, LoadError, LoadedComponent, WasmRuntime};
