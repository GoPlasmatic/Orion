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
/// twice — once at boot and once in `POST /workflows/{id}/test`, which builds a
/// throwaway engine over the same dependencies. Two call sites with ten
/// same-typed `Arc`s between them is a silent-transposition hazard the compiler
/// cannot see; every field here already lives on `AppState`, so
/// `runtime::handler_deps` is the only form the second call site needs. That
/// constructor is in `runtime` and not here because it takes an `AppState`,
/// and a module the request path sits below should not know that type.
pub struct HandlerDeps<'a> {
    pub registry: Arc<ConnectorRegistry>,
    pub client: reqwest::Client,
    /// The live serving generation, for the one handler that dispatches back
    /// into the node (`channel_call`): it needs the target channel's guards
    /// and an engine to run it on, and both come off one load.
    pub runtime: Arc<crate::runtime::RuntimeHandle>,
    /// The process's expression engine (I188), for `channel_call`: the
    /// target channel's guards were compiled on it and evaluate on it.
    pub exprs: dataflow_rs::expr::ExprEngine,
    /// The instance's JWKS cache — `jwt_verify`'s key source, shared with the
    /// channel `jwt` auth mode so both see one issuer rotation at once.
    pub jwks: Arc<crate::jwt::jwks::JwksCache>,
    pub engine_config: &'a crate::config::EngineConfig,
    pub query_config: &'a crate::config::QueryConfig,
    pub write_config: &'a crate::config::WriteConfig,
    pub cache_pool: Arc<crate::connector::cache_backend::CachePool>,
    /// The channel loader, for `cache_invalidate`: it knows which stores a
    /// response cache can resolve to, the cluster's shared one included.
    pub channel_loader: Arc<crate::channel::ChannelLoader>,
    pub sql_pool_cache: Arc<crate::connector::pool_cache::SqlPoolCache>,
    /// The node's model runtime — the loaded-session cache, the inference
    /// slots, the runtimes — for `model_infer`. `None` with
    /// `models.enabled = false`, which the handler answers as `unavailable`
    /// rather than the build refusing the function.
    pub models: Option<Arc<crate::model::ModelsRuntime>>,
    pub models_config: &'a crate::config::ModelsConfig,
    /// The node's v2 provider registry (`rust`, `workflow`, `wasm`) for `connector_call`.
    pub providers: crate::extensions::ProviderRegistry,
}

/// Register Orion's functions, each under its 1.12 name (F48).
pub fn register_functions(registry: &mut dataflow_rs::Registry, deps: HandlerDeps<'_>) {
    use functions::{
        cache_delete, cache_incr, cache_invalidate, cache_read, cache_write, channel_call, crypto,
        data_query, data_write, db_read, db_write, http_call, jwt_sign, jwt_verify, storage_head,
        storage_presign,
    };

    let HandlerDeps {
        registry: connectors,
        client,
        runtime,
        exprs,
        jwks,
        engine_config,
        query_config,
        write_config,
        cache_pool,
        channel_loader,
        sql_pool_cache,
        models,
        models_config,
        providers,
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
            runtime: runtime.clone(),
            exprs,
            max_call_depth: engine_config.max_channel_call_depth,
            default_timeout_ms: engine_config.default_channel_call_timeout_ms,
        },
    );
    // The model set comes off one load of the serving generation per call,
    // its adapters compiled on the process's expression engine; the loaded
    // sessions and permits off the node.
    let models_config = Arc::new(models_config.clone());
    registry.register(
        crate::model::handler::NAME,
        crate::model::ModelInferFunction {
            source: crate::model::ModelSource::Node(runtime),
            host: models.map(|models| {
                Arc::new(crate::model::InferenceHost::node(
                    &models,
                    models_config.clone(),
                    connectors.clone(),
                    client.clone(),
                ))
            }),
            config: models_config,
        },
    );
    // Self-contained (no connector, no deps): digests, MACs, password
    // hashing; then the JWT pair, verification sharing the channel mode's
    // JWKS cache.
    registry.register(crypto::NAME, crypto::CryptoFunction);
    registry.register(jwt_sign::NAME, jwt_sign::JwtSignFunction);
    registry.register(jwt_verify::NAME, jwt_verify::JwtVerifyFunction { jwks });
    registry.register(
        storage_presign::NAME,
        storage_presign::StoragePresignFunction {
            registry: connectors.clone(),
        },
    );
    registry.register(
        storage_head::NAME,
        storage_head::StorageHeadFunction {
            registry: connectors.clone(),
            client: client.clone(),
        },
    );
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
            channel_loader,
            cache_pool,
        },
    );
    functions::connector_call::register_connector_call(registry, providers, connectors);
}

/// The dataflow-rs 4 `Registry` a generation compiles and runs on, before
/// its plugins: every Orion function ([`register_functions`]), on the
/// process's expression engine (I188, `deps.exprs`), with the per-request
/// profile as its one call layer
/// ([`kit::ProfileLayer`](crate::engine::kit::ProfileLayer), I189).
///
/// Boot and every reload assemble it. The generation's plugin functions join it
/// ([`register_plugin_functions`]) before anything compiles, since a
/// workflow links only under the registry it compiled under.
pub fn serving_registry(deps: HandlerDeps<'_>) -> dataflow_rs::Registry {
    let mut registry = dataflow_rs::Registry::new(deps.exprs.clone());
    register_functions(&mut registry, deps);
    registry.layer(crate::engine::kit::ProfileLayer);
    registry
}

/// Register plugin functions on `registry`, each under the name its
/// manifest gives it (`<plugin>.<label>`), fed
/// [`PluginSet::functions`](crate::plugin::PluginSet::functions). A name
/// registered before is replaced.
///
/// An invalid registration is refused through `Registry::try_register`, so
/// runtime names and schemas cannot panic the node. It is logged and returned.
/// For a reserved built-in name, a task naming it calls the
/// built-in, which dataflow-rs 3 also dispatched in the plugin's place. An
/// uploaded manifest cannot hold one, its functions living under its
/// reverse-domain name; only a stored row that skipped the upload's check
/// can.
pub fn register_plugin_functions(
    registry: &mut dataflow_rs::Registry,
    functions: impl IntoIterator<Item = crate::plugin::PluginFunction>,
) -> Vec<String> {
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

/// Whether `Registry::register` refuses `name` with a panic (I43, amended
/// by I57): a built-in's ([`dataflow_rs::def::BUILTIN_FUNCTIONS`]) or
/// `validate`, 1.12's alias for `validation`, which the compat
/// reader rewrites before any name is looked up. A host registering names
/// from data checks them first, as `register`'s documentation asks.
pub fn is_reserved_function_name(name: &str) -> bool {
    dataflow_rs::def::is_reserved_function(name)
}
