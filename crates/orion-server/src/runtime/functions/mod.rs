//! The hosts the engine's node-facing functions dispatch into.
//!
//! Three of Orion's functions reach back into the node that runs them:
//! `channel_call` (another channel, under its guards, on the same
//! generation), `connector_call` (the composed providers, under the
//! deployment's authority) and `cache_invalidate` (every store a channel's
//! response cache can be in). The engine defines each as a function over a
//! host trait; this module is where those traits meet the serving runtime.

pub mod channel_call;
pub mod connector_call;

use std::sync::Arc;

use dataflow_rs::program::{CompileOptions, Compiler};

use crate::engine::functions::cache_invalidate::NamespaceInvalidator;
use crate::engine::functions::channel_call::ChannelCallHost;
use crate::engine::functions::connector_call::ConnectorCallHost;
use crate::engine::kit::OfflineDeps;
use crate::errors::OrionError;

pub use channel_call::GenerationChannels;
pub use connector_call::{ACTIVE_RUN_CONTEXT, ProviderCalls};

/// The three hosts, over one node's live components.
pub struct NodeHosts {
    pub channel_calls: Arc<dyn ChannelCallHost>,
    pub namespaces: Arc<dyn NamespaceInvalidator>,
    pub connector_calls: Arc<dyn ConnectorCallHost>,
}

impl NodeHosts {
    /// The hosts of a node serving `runtime`'s generations with these
    /// channels and providers.
    pub fn new(
        runtime: Arc<crate::runtime::RuntimeHandle>,
        exprs: dataflow_rs::expr::ExprEngine,
        channel_loader: Arc<crate::channel::ChannelLoader>,
        providers: crate::extensions::ProviderRegistry,
    ) -> Self {
        Self {
            channel_calls: Arc::new(GenerationChannels { runtime, exprs }),
            namespaces: channel_loader,
            connector_calls: Arc::new(ProviderCalls { providers }),
        }
    }
}

/// [`OfflineDeps`] with the hosts of an empty Community node: no channels, a
/// channel loader with nothing loaded, and the Community providers, so a
/// `connector_call` to a built-in provider is checked at compile as a node
/// would check it.
pub fn offline_deps() -> OfflineDeps {
    let runtime = Arc::new(crate::runtime::RuntimeHandle::new(
        Arc::new(crate::channel::ChannelSnapshot::empty()),
        crate::engine::FunctionRegistry::builtin().clone(),
    ));
    let providers = crate::extensions::CompositionRoot::community()
        .build_in_memory()
        .unwrap_or_else(|e| unreachable!("community composes: {e}"))
        .providers()
        .clone();
    let hosts = NodeHosts::new(
        runtime,
        crate::engine::kit::test_exprs(),
        Arc::new(crate::channel::ChannelLoader::new()),
        providers,
    );
    OfflineDeps::new().with_hosts(hosts.channel_calls, hosts.namespaces, hosts.connector_calls)
}

/// The registry an offline run compiles on: every Orion function over inert
/// dependencies, the expressions reading `secrets`, and the `plugins` loaded
/// from their components.
///
/// # Errors
///
/// A validation error when the expressions do not build, or a plugin function
/// is refused registration.
pub fn offline_compiler(
    secrets: &crate::engine::ResolvedSecrets,
    plugins: Vec<crate::plugin::PluginFunction>,
) -> Result<Compiler, OrionError> {
    let exprs = crate::engine::operators::orion_exprs(secrets, 0)
        .map_err(|e| OrionError::validation(format!("the offline expression engine: {e}")))?;
    let offline = offline_deps();
    let mut deps = offline.deps();
    deps.exprs = exprs;
    let mut registry = crate::engine::serving_registry(deps);
    let refused = crate::engine::register_plugin_functions(&mut registry, plugins);
    if !refused.is_empty() {
        return Err(OrionError::validation(format!(
            "plugin function(s) could not be registered: {}",
            refused.join(", ")
        )));
    }
    Ok(registry.compiler(CompileOptions::default()))
}
