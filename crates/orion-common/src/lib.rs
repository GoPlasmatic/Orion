//! The shared primitives of the Orion runtime.
//!
//! Everything here is a leaf the rest of the runtime builds on: the
//! configuration model ([`config`]), the one error type every layer produces
//! ([`errors`]), the crypto and encoding primitives ([`crypto`],
//! [`canonical`], [`config_encryption`], [`signatures`]), the Prometheus
//! recorders ([`metrics`]), the per-request identity and W3C trace
//! propagation ([`request_context`], [`trace_context`]), the bounded egress
//! body reader ([`http_body`]), the node-wide native work scheduler
//! ([`native_work`]) and the build's version ([`version`]).
//!
//! Nothing in this crate names a module above it — the crate boundary is what
//! enforces that. **Nothing here is covered by semver**: the crate exists for
//! `orion-server` and the crates beside it, and changes whenever they need it
//! to.

pub mod canonical;
pub mod config;
pub mod config_encryption;
pub mod crypto;
pub mod errors;
pub mod http_body;
pub mod metrics;
pub mod native_work;
pub mod platform_routes;
pub mod request_context;
pub mod signatures;
pub mod text;
pub mod trace_context;
pub mod version;
