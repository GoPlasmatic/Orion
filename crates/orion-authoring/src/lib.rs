//! The authoring layer of the Orion runtime: a definition set on disk
//! ([`definitions`] — the cross-reference check, the compile passes, `fmt`,
//! `clippy` and their analysis) and the checks every definition passes before
//! a node accepts it ([`validation`]), shared by the offline tools and the
//! admin API.
//!
//! **Nothing here is covered by semver**: the crate exists for `orion-server`
//! and changes whenever it needs it to.

// The crates below under their `crate::` names, so the authoring paths read
// as they did inside `orion-server`.
pub(crate) use orion_common::{canonical, config, crypto, errors, signatures, text, version};
pub(crate) use orion_engine::{self as engine, connector};
pub(crate) use orion_plugin_host as plugin;
pub(crate) use orion_runtime::{channel, extensions, runtime};
pub(crate) use orion_storage as storage;

pub mod definitions;
pub mod validation;
