//! Orion — declarative services runtime.
//!
//! This crate is the implementation of the `orion-server` binary: business
//! logic exposed as channels (service endpoints) and workflows (dataflow-rs
//! task pipelines) behind a REST API, shipped as a single binary with an
//! embedded SQLite database (PostgreSQL/MySQL selectable at runtime).
//!
//! The library target exists for the binary and the integration test suite.
//! It is not a supported embedding API: module layout and signatures may
//! change between releases without semver ceremony. If you want to run
//! Orion, use the binary — see the
//! [documentation](https://goplasmatic.github.io/Orion/) for operating it.
#![warn(clippy::unwrap_used, clippy::panic)]

/// Startup wiring shared by the binary and the test harness. Doc-hidden:
/// reachable so tests exercise the REAL boot path (engine components,
/// fail-fast connector refusal, background tasks) instead of reimplementing
/// it, but not part of the documented 1.0 API surface.
#[doc(hidden)]
pub mod bootstrap;
pub mod channel;
pub mod cluster;
pub mod config;
pub mod connector;
pub mod engine;
pub mod errors;
pub mod kafka;
pub mod metrics;
pub(crate) mod query;
pub mod queue;
pub mod server;
pub mod storage;
pub mod validation;
