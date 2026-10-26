//! The serving node of the Orion runtime: the channel estate and its ingress
//! guards, the published runtime generation and the deployment builds that
//! produce it, the cron scheduler, the trace and audit queues, cluster
//! coordination, and the v2 extension boundary (composition, providers, the
//! deployment store). [`runtime::state::AppState`] is the node, as every
//! transport above it is handed.
//!
//! **Nothing here is covered by semver**: the crate exists for `orion-server`
//! and changes whenever it needs it to.

// The crates below under their `crate::` names, so the node's paths read as
// they did inside `orion-server`.
pub(crate) use orion_common::{
    canonical, config, crypto, errors, http_body, metrics, request_context, trace_context, version,
};
pub(crate) use orion_engine::{self as engine, connector, jwt};
pub(crate) use orion_plugin_host as plugin;
pub(crate) use orion_storage as storage;

#[doc(hidden)]
pub mod auth;
pub mod channel;
pub mod cluster;
pub mod cron;
#[doc(hidden)]
pub mod extensions;
pub mod queue;
pub mod runtime;
