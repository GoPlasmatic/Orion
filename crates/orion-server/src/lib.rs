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
//! the integration test suite, and an edition binary that runs a node with its
//! own composition: [`run`] with an [`extensions::CompositionBuilder`] carrying
//! its channel drivers, provider backends, pools, package verifier and
//! edition, written against [`extension_api`] (re-exported, so an edition takes
//! the matching version from this crate). Everything else is `pub` because
//! those need it, not because it is offered for use.
//!
//! **The Rust surface is not covered by semver.** Module layout, type names,
//! function signatures and trait shapes change whenever the binary needs them
//! to, including in patch releases; the extension API grows with its first
//! consumer. The crate version tracks the *product* — its HTTP API, config
//! surface, definition contract and metric names, which is what 2.x keeps
//! stable. Pin this crate, and every `orion-*` crate, at an exact version.
//!
//! If you want to run Orion, use the binary. See the
//! [documentation](https://docs.goplasmatic.io/) for operating it, and its
//! versioning policy for what *is* stable.
//!
//! If you are reading this to understand the code, start at
//! [`bootstrap`] (the startup sequence, doc-hidden but public),
//! [`server::routes::data`] (the request path) and [`engine`] (the workflow
//! runtime).

#[doc(hidden)]
pub use orion_runtime::auth;
/// Startup wiring shared by the binary and the test harness. Doc-hidden:
/// reachable so tests exercise the REAL boot path (engine components,
/// fail-fast connector refusal, background tasks) instead of reimplementing
/// it, but not part of the documented 1.0 API surface.
#[doc(hidden)]
pub mod bootstrap;
pub use orion_authoring::definitions;
#[doc(hidden)]
pub use orion_common::canonical;
pub use orion_common::config;
pub use orion_common::crypto;
pub use orion_common::errors;
pub use orion_engine as engine;
pub use orion_engine::connector;
pub use orion_runtime::channel;
pub use orion_runtime::cluster;
pub use orion_runtime::cron;
#[doc(hidden)]
pub use orion_runtime::ingress;
/// The v2 extension boundary, as an edition binary composes a node: the
/// [`extensions::CompositionBuilder`] it hands [`run`], and the frozen
/// [`extensions::Composition`] the node serves with. One path per item; the
/// internals stay reachable to this crate and, for tests, through
/// [`test_support`].
pub mod extensions {
    pub(crate) use orion_runtime::extensions::*;
    pub use orion_runtime::extensions::{Composition, CompositionBuilder, CompositionRoot};

    /// What an edition's provider builds on, so it does not copy Community's
    /// internals: the registrar a [`CompositionBuilder::with_providers`]
    /// setup is handed and the node options on it (the shared HTTP client and
    /// caps), the version-fenced session pool the foundation SQL providers
    /// key on `EffectiveConnectionIdentity`, and the
    /// address checks every egress runs before it connects.
    pub mod kit {
        pub use orion_common::ssrf::{
            is_private_ip, validate_hostport_not_private, validate_url_not_private,
        };
        pub use orion_runtime::extensions::resource_pool::{SessionHandle, VersionedSessionPool};
        pub use orion_runtime::extensions::{FoundationOptions, ProviderRegistrar, SqlLimits};
    }
}
#[doc(hidden)]
pub use orion_common::http_body;
pub use orion_common::metrics;
#[doc(hidden)]
pub use orion_common::request_context;
#[doc(hidden)]
pub use orion_common::signatures;
#[doc(hidden)]
pub use orion_engine::jwt;
pub use orion_http as server;
pub use orion_plugin_host as plugin;
pub use orion_runtime::queue;
pub use orion_runtime::runtime;
#[doc(hidden)]
pub use orion_storage as storage;
#[doc(hidden)]
pub mod test_support;
pub use orion_authoring::validation;
#[doc(hidden)]
pub use orion_common::trace_context;
pub use orion_common::version;

pub use bootstrap::run;

/// The extension surface an edition implements (`ChannelDriver`,
/// `ProviderBackend`, `Registrar`, …), at the version this crate was built
/// against.
pub use orion_extension_api as extension_api;
