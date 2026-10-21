//! The construction of the handlers behind Orion's own registry entries.
//!
//! The name vocabulary itself — what a workflow may name, which names take a
//! connector, the "did you mean" suggestion — lives on
//! [`super::FunctionRegistry`]. This module only builds the handlers those
//! entries dispatch to, and `function_schema_test` pins the two to each other
//! against a live engine: every `Source::Orion` entry has a handler here, and
//! every handler registered here has an entry.

use std::collections::HashMap;
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
    pub mongo_pool_cache: Arc<crate::connector::mongo_pool::MongoPoolCache>,
    pub smtp_pool_cache: Arc<crate::connector::smtp_pool::SmtpPoolCache>,
    /// The node's model runtime — the loaded-session cache, the inference
    /// slots, the runtimes — for `model_infer`. `None` with
    /// `models.enabled = false`, which the handler answers as `unavailable`
    /// rather than the build refusing the function.
    pub models: Option<Arc<crate::model::ModelsRuntime>>,
    pub models_config: &'a crate::config::ModelsConfig,
}

/// Register one connector handler, wrapped and keyed by its own name.
///
/// The wrapping is what supplies the prologue → resolve → gate → shell → output
/// sequence, and the key is `H::NAME` rather than a string literal beside it:
/// every other `fns.insert` here spells the function's name twice, once as the
/// map key and once inside the handler, and nothing checked that the two agree.
/// A handler registered under a name it does not answer to is a task that
/// dispatches fine and reports metrics, profile samples and errors under
/// another function's name.
fn register<H: functions::connector_handler::ConnectorHandler>(
    fns: &mut HashMap<String, dataflow_rs_v3::BoxedFunctionHandler>,
    handler: H,
) {
    fns.insert(
        H::NAME.to_string(),
        Box::new(functions::connector_handler::Connector(handler)),
    );
}

/// Build the custom function handlers for the dataflow-rs engine.
///
/// Registers every `Source::Orion` entry of the registry — the connector
/// handlers, `channel_call`, the self-contained trio, `model_infer` — plus a
/// stub `publish_kafka`. Call [`register_kafka_publisher`] afterwards to swap
/// the stub for the real Kafka-backed handler once the producer is
/// initialised.
pub fn build_custom_functions(
    deps: HandlerDeps<'_>,
) -> HashMap<String, dataflow_rs_v3::BoxedFunctionHandler> {
    let HandlerDeps {
        registry,
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
        mongo_pool_cache,
        smtp_pool_cache,
        models,
        models_config,
    } = deps;
    let mut fns: HashMap<String, dataflow_rs_v3::BoxedFunctionHandler> = HashMap::new();

    register(
        &mut fns,
        functions::http_call::HttpCallHandler {
            registry: registry.clone(),
            client: client.clone(),
        },
    );

    fns.insert(
        functions::channel_call::NAME.to_string(),
        Box::new(functions::channel_call::ChannelCallHandler {
            runtime: runtime.clone(),
            exprs,
            max_call_depth: engine_config.max_channel_call_depth,
            default_timeout_ms: engine_config.default_channel_call_timeout_ms,
        }),
    );

    // The other handler that dispatches through the serving generation: it
    // takes the model set and the expression engine off one load, and the
    // loaded sessions and permits off the node.
    let models_config = Arc::new(models_config.clone());
    fns.insert(
        crate::model::handler::NAME.to_string(),
        Box::new(crate::model::ModelInferHandler {
            source: crate::model::ModelSource::Node(runtime),
            host: models.map(|models| {
                Arc::new(crate::model::InferenceHost::node(
                    &models,
                    models_config.clone(),
                    registry.clone(),
                    client.clone(),
                ))
            }),
            config: models_config,
        }),
    );

    // Self-contained (no connector, no deps): digests, MACs, password hashing.
    fns.insert(
        "crypto".to_string(),
        Box::new(functions::crypto::CryptoHandler),
    );

    // Self-contained JWT surfaces (#267): issuance and mid-workflow
    // verification, sharing the channel mode's core and JWKS cache.
    fns.insert(
        "jwt_sign".to_string(),
        Box::new(functions::jwt_sign::JwtSignHandler),
    );
    fns.insert(
        "jwt_verify".to_string(),
        Box::new(functions::jwt_verify::JwtVerifyHandler { jwks }),
    );

    register(
        &mut fns,
        functions::send_email::SendEmailHandler {
            registry: registry.clone(),
            smtp_pool: smtp_pool_cache,
        },
    );

    register(
        &mut fns,
        functions::storage_presign::StoragePresignHandler {
            registry: registry.clone(),
        },
    );

    register(
        &mut fns,
        functions::storage_head::StorageHeadHandler {
            registry: registry.clone(),
            client: client.clone(),
        },
    );

    // Register stub publish_kafka (will be replaced by register_kafka_publisher when Kafka is configured)
    register(
        &mut fns,
        functions::publish_kafka::PublishKafkaHandler {
            registry: registry.clone(),
            producers: None,
        },
    );

    // Register SQL database handlers (db_read, db_write)
    register(
        &mut fns,
        functions::db_read::DbReadHandler {
            pool_cache: sql_pool_cache.clone(),
            registry: registry.clone(),
            max_rows: query_config.max_limit as usize,
        },
    );
    register(
        &mut fns,
        functions::db_write::DbWriteHandler {
            pool_cache: sql_pool_cache.clone(),
            registry: registry.clone(),
        },
    );

    // Register the portable query handler (data_query). It renders a
    // backend-neutral filter + envelope to native SQL or a MongoDB find
    // (§ src/query/).
    register(
        &mut fns,
        functions::data_query::DataQueryHandler {
            pool_cache: sql_pool_cache.clone(),
            mongo_pool_cache: mongo_pool_cache.clone(),
            http_client: client.clone(),
            registry: registry.clone(),
            limits: query_config.clone(),
        },
    );

    // Register the portable write handler (data_write). It renders a
    // backend-neutral mutation envelope to a native SQL INSERT/UPDATE/DELETE/upsert,
    // a MongoDB write, or an Elasticsearch write (§ src/query/write.rs).
    register(
        &mut fns,
        functions::data_write::DataWriteHandler {
            pool_cache: sql_pool_cache,
            mongo_pool_cache: mongo_pool_cache.clone(),
            http_client: client.clone(),
            registry: registry.clone(),
            write_config: write_config.clone(),
            max_returning: query_config.max_limit as usize,
        },
    );

    // Register cache handlers (cache_read, cache_write, cache_delete, cache_incr).
    // CachePool routes to the in-memory or Redis backend per connector config.
    // Wrapped: `Connector<H>` is what supplies the prologue → resolve →
    // gate → shell → output sequence, and an unwrapped handler is not an
    // `AsyncFunctionHandler` at all, so it cannot be registered.
    register(
        &mut fns,
        functions::cache_read::CacheReadHandler {
            cache_pool: cache_pool.clone(),
            registry: registry.clone(),
        },
    );
    register(
        &mut fns,
        functions::cache_write::CacheWriteHandler {
            cache_pool: cache_pool.clone(),
            registry: registry.clone(),
        },
    );
    register(
        &mut fns,
        functions::cache_delete::CacheDeleteHandler {
            cache_pool: cache_pool.clone(),
            registry: registry.clone(),
        },
    );
    register(
        &mut fns,
        functions::cache_incr::CacheIncrHandler {
            cache_pool: cache_pool.clone(),
            registry: registry.clone(),
        },
    );
    fns.insert(
        "cache_invalidate".to_string(),
        Box::new(functions::cache_invalidate::CacheInvalidateHandler {
            channel_loader,
            registry: registry.clone(),
            cache_pool,
        }),
    );

    // Register the MongoDB trio (mongo_read, mongo_write, mongo_aggregate)
    register(
        &mut fns,
        functions::mongo_read::MongoReadHandler {
            pool_cache: mongo_pool_cache.clone(),
            registry: registry.clone(),
            limits: query_config.clone(),
        },
    );
    register(
        &mut fns,
        functions::mongo_write::MongoWriteHandler {
            pool_cache: mongo_pool_cache.clone(),
            registry: registry.clone(),
            write_config: write_config.clone(),
        },
    );
    register(
        &mut fns,
        functions::mongo_aggregate::MongoAggregateHandler {
            pool_cache: mongo_pool_cache,
            registry: registry.clone(),
            limits: query_config.clone(),
        },
    );

    fns
}

/// Register Orion's dataflow-rs 4 functions: the twin of
/// [`build_custom_functions`], function by function in its order, as each
/// gains a v4 port, each under its dataflow-rs 3 handler's name (F48). No
/// serving path calls it before the building cutover.
///
/// `publish_kafka` is registered with no producer, as
/// `build_custom_functions` registers its stub: the producer is swapped in
/// once Kafka is initialised, as [`register_kafka_publisher`] does for
/// dataflow-rs 3 (`Registry::register` replaces a name).
pub fn register_functions(registry: &mut dataflow_rs::Registry, deps: HandlerDeps<'_>) {
    use functions::connector_handler::ConnectorHandler;
    use functions::{
        cache_delete, cache_incr, cache_invalidate, cache_read, cache_write, channel_call, crypto,
        data_query, data_write, db_read, db_write, http_call, jwt_sign, jwt_verify,
        mongo_aggregate, mongo_read, mongo_write, publish_kafka, send_email, storage_head,
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
        mongo_pool_cache,
        smtp_pool_cache,
        models,
        models_config,
    } = deps;

    registry.register(
        <http_call::HttpCallHandler as ConnectorHandler>::NAME,
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
    // sessions and permits off the node, shared with dataflow-rs 3's handler.
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
        <send_email::SendEmailHandler as ConnectorHandler>::NAME,
        send_email::SendEmailFunction {
            registry: connectors.clone(),
            smtp_pool: smtp_pool_cache,
        },
    );
    registry.register(
        <storage_presign::StoragePresignHandler as ConnectorHandler>::NAME,
        storage_presign::StoragePresignFunction {
            registry: connectors.clone(),
        },
    );
    registry.register(
        <storage_head::StorageHeadHandler as ConnectorHandler>::NAME,
        storage_head::StorageHeadFunction {
            registry: connectors.clone(),
            client: client.clone(),
        },
    );
    registry.register(
        <publish_kafka::PublishKafkaHandler as ConnectorHandler>::NAME,
        publish_kafka::PublishKafkaFunction {
            registry: connectors.clone(),
            producers: None,
        },
    );
    registry.register(
        <db_read::DbReadHandler as ConnectorHandler>::NAME,
        db_read::DbReadFunction {
            pool_cache: sql_pool_cache.clone(),
            registry: connectors.clone(),
            max_rows: query_config.max_limit as usize,
        },
    );
    registry.register(
        <db_write::DbWriteHandler as ConnectorHandler>::NAME,
        db_write::DbWriteFunction {
            pool_cache: sql_pool_cache.clone(),
            registry: connectors.clone(),
        },
    );
    registry.register(
        <data_query::DataQueryHandler as ConnectorHandler>::NAME,
        data_query::DataQueryFunction {
            pool_cache: sql_pool_cache.clone(),
            mongo_pool_cache: mongo_pool_cache.clone(),
            http_client: client.clone(),
            registry: connectors.clone(),
            limits: query_config.clone(),
        },
    );
    registry.register(
        <data_write::DataWriteHandler as ConnectorHandler>::NAME,
        data_write::DataWriteFunction {
            pool_cache: sql_pool_cache,
            mongo_pool_cache: mongo_pool_cache.clone(),
            http_client: client,
            registry: connectors.clone(),
            write_config: write_config.clone(),
            max_returning: query_config.max_limit as usize,
        },
    );
    registry.register(
        <cache_read::CacheReadHandler as ConnectorHandler>::NAME,
        cache_read::CacheReadFunction {
            cache_pool: cache_pool.clone(),
            registry: connectors.clone(),
        },
    );
    registry.register(
        <cache_write::CacheWriteHandler as ConnectorHandler>::NAME,
        cache_write::CacheWriteFunction {
            cache_pool: cache_pool.clone(),
            registry: connectors.clone(),
        },
    );
    registry.register(
        <cache_delete::CacheDeleteHandler as ConnectorHandler>::NAME,
        cache_delete::CacheDeleteFunction {
            cache_pool: cache_pool.clone(),
            registry: connectors.clone(),
        },
    );
    registry.register(
        <cache_incr::CacheIncrHandler as ConnectorHandler>::NAME,
        cache_incr::CacheIncrFunction {
            cache_pool: cache_pool.clone(),
            registry: connectors.clone(),
        },
    );
    registry.register(
        cache_invalidate::NAME,
        cache_invalidate::CacheInvalidateFunction {
            channel_loader,
            registry: connectors.clone(),
            cache_pool,
        },
    );
    registry.register(
        <mongo_read::MongoReadHandler as ConnectorHandler>::NAME,
        mongo_read::MongoReadFunction {
            pool_cache: mongo_pool_cache.clone(),
            registry: connectors.clone(),
            limits: query_config.clone(),
        },
    );
    registry.register(
        <mongo_write::MongoWriteHandler as ConnectorHandler>::NAME,
        mongo_write::MongoWriteFunction {
            pool_cache: mongo_pool_cache.clone(),
            registry: connectors.clone(),
            write_config: write_config.clone(),
        },
    );
    registry.register(
        <mongo_aggregate::MongoAggregateHandler as ConnectorHandler>::NAME,
        mongo_aggregate::MongoAggregateFunction {
            pool_cache: mongo_pool_cache,
            registry: connectors,
            limits: query_config.clone(),
        },
    );
}

/// The dataflow-rs 4 `Registry` a generation compiles and runs on, before
/// its plugins: every Orion function ([`register_functions`]), on the
/// process's expression engine (I188, `deps.exprs`), with the real
/// `publish_kafka` when Kafka is initialised ([`register_kafka_function`]),
/// and the per-request profile as its one call layer
/// ([`kit::ProfileLayer`](crate::engine::kit::ProfileLayer), I189).
///
/// The twin of [`build_custom_functions`] plus [`register_kafka_publisher`],
/// which boot and a plugin-changing reload both assemble dataflow-rs 3's
/// handlers from. The generation's plugin functions join it
/// ([`register_plugin_functions`]) before anything compiles, since a
/// workflow links only under the registry it compiled under.
pub fn serving_registry(
    deps: HandlerDeps<'_>,
    producers: Option<Arc<crate::kafka::producer::KafkaProducerCache>>,
) -> dataflow_rs::Registry {
    let mut registry = dataflow_rs::Registry::new(deps.exprs.clone());
    let connectors = deps.registry.clone();
    register_functions(&mut registry, deps);
    if let Some(producers) = producers {
        register_kafka_function(&mut registry, connectors, producers);
    }
    registry.layer(crate::engine::kit::ProfileLayer);
    registry
}

/// Register the real Kafka-backed `publish_kafka` on a dataflow-rs 4
/// `Registry`: [`register_kafka_publisher`]'s twin, replacing the stub
/// [`register_functions`] registers (`Registry::register` replaces a name).
pub fn register_kafka_function(
    registry: &mut dataflow_rs::Registry,
    connectors: Arc<ConnectorRegistry>,
    producers: Arc<crate::kafka::producer::KafkaProducerCache>,
) {
    use functions::connector_handler::ConnectorHandler;
    registry.register(
        <functions::publish_kafka::PublishKafkaHandler as ConnectorHandler>::NAME,
        functions::publish_kafka::PublishKafkaFunction {
            registry: connectors,
            producers: Some(producers),
        },
    );
}

/// Register plugin functions on `registry`, each under the name its
/// manifest gives it (`<plugin>.<label>`): the dataflow-rs 4 twin of adding
/// [`PluginSet::handlers`](crate::plugin::PluginSet::handlers) to a
/// dataflow-rs 3 engine's handler map, fed
/// [`PluginSet::functions`](crate::plugin::PluginSet::functions). A name
/// registered before is replaced, as the map's insert replaced it.
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
/// `validate`, dataflow-rs 3's alias for `validation`, which the compat
/// reader rewrites before any name is looked up. A host registering names
/// from data checks them first, as `register`'s documentation asks.
pub fn is_reserved_function_name(name: &str) -> bool {
    dataflow_rs::def::is_reserved_function(name)
}

/// Register the real Kafka-backed publish_kafka handler.
///
/// Replaces the stub handler (or adds the handler if not yet registered).
pub fn register_kafka_publisher(
    fns: &mut HashMap<String, dataflow_rs_v3::BoxedFunctionHandler>,
    registry: Arc<ConnectorRegistry>,
    producers: Arc<crate::kafka::producer::KafkaProducerCache>,
) {
    register(
        fns,
        functions::publish_kafka::PublishKafkaHandler {
            registry,
            producers: Some(producers),
        },
    );
}
