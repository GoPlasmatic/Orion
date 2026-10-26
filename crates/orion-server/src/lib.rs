//! Orion — declarative services runtime.
//!
//! This crate is the implementation of the `orion-server` binary: business
//! logic exposed as channels (service endpoints) and workflows (dataflow-rs
//! task pipelines) behind a REST API, shipped as a single binary with an
//! embedded SQLite database (PostgreSQL/MySQL selectable at runtime).
//!
//! # This is not an embedding API
//!
//! The library target exists for three consumers: the `orion-server` binary,
//! the integration test suite, and an edition binary (Orion Enterprise) that
//! runs a node with its own composition: [`run`] with an
//! [`extensions::CompositionBuilder`] carrying its channel drivers, provider
//! backends, pools, package verifier and edition. Everything else is `pub`
//! because those need it, not because it is offered for use.
//!
//! **Nothing here is covered by semver.** Module layout, type names, function
//! signatures and trait shapes change whenever the binary needs them to,
//! including in patch releases. The crate version tracks the *product* — its
//! HTTP API, config surface, workflow JSON contract, metric names and database
//! schema, which is what 1.0 stabilises. It does not describe this Rust surface,
//! and a `1.x` bump is not a promise about any item on this page.
//!
//! If you want to run Orion, use the binary. See the
//! [documentation](https://docs.goplasmatic.io/) for operating it, and
//! `docs/src/api/` for the interfaces that *are* stable.
//!
//! If you are reading this to understand the code, start at
//! [`bootstrap`] (the startup sequence, doc-hidden but public),
//! [`server::routes::data`] (the request path) and [`engine`] (the workflow
//! runtime).

#[doc(hidden)]
pub mod auth;
/// Startup wiring shared by the binary and the test harness. Doc-hidden:
/// reachable so tests exercise the REAL boot path (engine components,
/// fail-fast connector refusal, background tasks) instead of reimplementing
/// it, but not part of the documented 1.0 API surface.
#[doc(hidden)]
pub mod bootstrap;
#[doc(hidden)]
pub use orion_common::canonical;
pub mod channel;
pub mod cluster;
pub use orion_common::config;
pub mod connector;
pub mod cron;
pub use orion_common::crypto;
pub mod definitions;
pub mod engine;
pub use orion_common::errors;
pub mod extensions;
#[doc(hidden)]
pub use orion_common::http_body;
#[doc(hidden)]
pub mod jwt;
pub use orion_common::metrics;
pub mod plugin;
pub(crate) mod query;
pub mod queue;
#[doc(hidden)]
pub use orion_common::request_context;
pub mod runtime;
pub mod server;
#[doc(hidden)]
pub use orion_common::signatures;
#[doc(hidden)]
pub mod sql_lex;
pub mod storage;
#[doc(hidden)]
pub mod test_support;
pub(crate) use orion_common::text;
#[doc(hidden)]
pub use orion_common::trace_context;
pub mod validation;
pub use orion_common::version;

pub use bootstrap::run;
