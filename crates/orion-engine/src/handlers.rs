//! The construction of the handlers behind Orion's own registry entries.
//!
//! The name vocabulary itself — what a workflow may name, which names take a
//! connector, the "did you mean" suggestion — lives on
//! [`super::FunctionRegistry`]. This module only builds the handlers those
//! entries dispatch to, and `function_schema_test` pins the two to each other
//! against a live engine: every `Source::Orion` entry has a handler here, and
//! every handler registered here has an entry.

use std::sync::Arc;

use super::functions;
use crate::connector::ConnectorRegistry;

/// Everything Orion's own handlers need to be constructed.
///
/// F44: this was ten positional parameters carrying
/// `#[allow(clippy::too_many_arguments)]`, and the full call was written out
/// twice — once at boot and once in `POST /definitions/test`, which builds a
/// throwaway engine over the same dependencies. Two call sites with ten
/// same-typed `Arc`s between them is a silent-transposition hazard the compiler
/// cannot see; every field here already lives on `AppState`, so
/// `runtime::handler_deps` is the only form the second call site needs. That
/// constructor is in `runtime` and not here because it takes an `AppState`,
/// and a module the request path sits below should not know that type.
pub struct HandlerDeps<'a> {
    pub registry: Arc<ConnectorRegistry>,
    pub client: reqwest::Client,
    /// The node `channel_call` dispatches back into: the target channel's
    /// guards and an engine to run it on, both from one generation.
    pub channel_calls: Arc<dyn functions::channel_call::ChannelCallHost>,
    /// The process's expression engine (I188), which the registry is built on.
    pub exprs: dataflow_rs::expr::ExprEngine,
    /// The instance's JWKS cache — `jwt_verify`'s key source, shared with the
    /// channel `jwt` auth mode so both see one issuer rotation at once.
    pub jwks: Arc<crate::jwt::jwks::JwksCache>,
    pub engine_config: &'a crate::config::EngineConfig,
    pub query_config: &'a crate::config::QueryConfig,
    pub write_config: &'a crate::config::WriteConfig,
    pub cache_pool: Arc<crate::connector::cache_backend::CachePool>,
    /// The channel estate, for `cache_invalidate`: it knows which stores a
    /// response cache can resolve to, the cluster's shared one included.
    pub namespaces: Arc<dyn functions::cache_invalidate::NamespaceInvalidator>,
    pub sql_pool_cache: Arc<crate::connector::pool_cache::SqlPoolCache>,
    /// The node's providers and its deployment's authority, for
    /// `connector_call`.
    pub connector_calls: Arc<dyn functions::connector_call::ConnectorCallHost>,
}

/// Register Orion's functions, each under its 1.12 name (F48).
pub fn register_functions(registry: &mut dataflow_rs::Registry, deps: HandlerDeps<'_>) {
    use functions::{
        cache_delete, cache_incr, cache_invalidate, cache_read, cache_write, channel_call, crypto,
        data_query, data_write, db_read, db_write, http_call, jwt_sign, jwt_verify,
    };

    let HandlerDeps {
        registry: connectors,
        client,
        channel_calls,
        exprs: _,
        jwks,
        engine_config,
        query_config,
        write_config,
        cache_pool,
        namespaces,
        sql_pool_cache,
        connector_calls,
    } = deps;

    registry.register(
        http_call::NAME,
        http_call::HttpCallFunction {
            registry: connectors.clone(),
            client: client.clone(),
        },
    );
    // The other function that dispatches back into the node: it runs its
    // child on the serving generation's engine, under the target's guards.
    registry.register(
        channel_call::NAME,
        channel_call::ChannelCallFunction {
            host: channel_calls,
            max_call_depth: engine_config.max_channel_call_depth,
            default_timeout_ms: engine_config.default_channel_call_timeout_ms,
        },
    );
    // Self-contained (no connector, no deps): digests, MACs, password
    // hashing; then the JWT pair, verification sharing the channel mode's
    // JWKS cache.
    registry.register(crypto::NAME, crypto::CryptoFunction);
    registry.register(jwt_sign::NAME, jwt_sign::JwtSignFunction);
    registry.register(jwt_verify::NAME, jwt_verify::JwtVerifyFunction { jwks });
    registry.register(
        db_read::NAME,
        db_read::DbReadFunction {
            pool_cache: sql_pool_cache.clone(),
            registry: connectors.clone(),
            max_rows: query_config.max_limit as usize,
            max_result_bytes: query_config.max_result_bytes,
        },
    );
    registry.register(
        db_write::NAME,
        db_write::DbWriteFunction {
            pool_cache: sql_pool_cache.clone(),
            registry: connectors.clone(),
        },
    );
    registry.register(
        data_query::NAME,
        data_query::DataQueryFunction {
            pool_cache: sql_pool_cache.clone(),
            registry: connectors.clone(),
            limits: query_config.clone(),
        },
    );
    registry.register(
        data_write::NAME,
        data_write::DataWriteFunction {
            pool_cache: sql_pool_cache,
            registry: connectors.clone(),
            write_config: write_config.clone(),
            max_returning: query_config.max_limit as usize,
        },
    );
    registry.register(
        cache_read::NAME,
        cache_read::CacheReadFunction {
            cache_pool: cache_pool.clone(),
            registry: connectors.clone(),
        },
    );
    registry.register(
        cache_write::NAME,
        cache_write::CacheWriteFunction {
            cache_pool: cache_pool.clone(),
            registry: connectors.clone(),
        },
    );
    registry.register(
        cache_delete::NAME,
        cache_delete::CacheDeleteFunction {
            cache_pool: cache_pool.clone(),
            registry: connectors.clone(),
        },
    );
    registry.register(
        cache_incr::NAME,
        cache_incr::CacheIncrFunction {
            cache_pool: cache_pool.clone(),
            registry: connectors.clone(),
        },
    );
    registry.register(
        cache_invalidate::NAME,
        cache_invalidate::CacheInvalidateFunction {
            channel_loader: namespaces,
            cache_pool,
        },
    );
    registry.register(
        functions::connector_call::NAME,
        functions::connector_call::ConnectorCallFunction {
            host: connector_calls,
            registry: connectors,
        },
    );
}

/// The dataflow-rs 4 `Registry` a generation compiles and runs on, before
/// its plugins: every Orion function ([`register_functions`]), on the
/// process's expression engine (I188, `deps.exprs`), with the per-request
/// profile as its one call layer
/// ([`kit::ProfileLayer`](crate::kit::ProfileLayer), I189).
///
/// Boot and every reload assemble it. The generation's plugin functions join it
/// ([`register_plugin_functions`]) before anything compiles, since a
/// workflow links only under the registry it compiled under.
pub fn serving_registry(deps: HandlerDeps<'_>) -> dataflow_rs::Registry {
    let mut registry = dataflow_rs::Registry::new(deps.exprs.clone());
    register_functions(&mut registry, deps);
    registry.layer(crate::kit::ProfileLayer);
    registry
}

/// Register plugin functions on `registry`, each under the name its
/// manifest gives it (`<plugin>.<label>`), fed
/// `PluginSet::functions`. A name
/// registered before is replaced.
///
/// An invalid registration is refused through `Registry::try_register`, so
/// runtime names and schemas cannot panic the node. It is logged and returned.
/// For a reserved built-in name, a task naming it calls the
/// built-in, which dataflow-rs 3 also dispatched in the plugin's place. An
/// uploaded manifest cannot hold one, its functions living under its
/// reverse-domain name; only a stored row that skipped the upload's check
/// can.
pub fn register_plugin_functions<F>(
    registry: &mut dataflow_rs::Registry,
    functions: impl IntoIterator<Item = F>,
) -> Vec<String>
where
    F: NamedFunction,
{
    let mut refused = Vec::new();
    for function in functions {
        let name = function.name().to_string();
        if let Err(error) = registry.try_register(&name, function) {
            tracing::error!(
                function = %name,
                error = %error,
                "Plugin function not registered"
            );
            refused.push(name);
        }
    }
    refused
}

/// A function that knows the name it registers under: a plugin's, whose name
/// its manifest gives.
pub trait NamedFunction: dataflow_rs::Function {
    /// The name the function registers under.
    fn name(&self) -> &str;
}

/// Whether `Registry::register` refuses `name` with a panic (I43, amended
/// by I57): a built-in's ([`dataflow_rs::def::BUILTIN_FUNCTIONS`]) or
/// `validate`, 1.12's alias for `validation`, which the compat
/// reader rewrites before any name is looked up. A host registering names
/// from data checks them first, as `register`'s documentation asks.
pub fn is_reserved_function_name(name: &str) -> bool {
    dataflow_rs::def::is_reserved_function(name)
}
