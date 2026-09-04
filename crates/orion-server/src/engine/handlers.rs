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

/// Everything the ten custom handlers need to be constructed.
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
    /// The instance's JWKS cache — `jwt_verify`'s key source, shared with the
    /// channel `jwt` auth mode so both see one issuer rotation at once.
    pub jwks: Arc<crate::jwt::jwks::JwksCache>,
    pub engine_config: &'a crate::config::EngineConfig,
    pub query_config: &'a crate::config::QueryConfig,
    pub write_config: &'a crate::config::WriteConfig,
    pub cache_pool: Arc<crate::connector::cache_backend::CachePool>,
    pub sql_pool_cache: Arc<crate::connector::pool_cache::SqlPoolCache>,
    pub mongo_pool_cache: Arc<crate::connector::mongo_pool::MongoPoolCache>,
    pub smtp_pool_cache: Arc<crate::connector::smtp_pool::SmtpPoolCache>,
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
    fns: &mut HashMap<String, dataflow_rs::BoxedFunctionHandler>,
    handler: H,
) {
    fns.insert(
        H::NAME.to_string(),
        Box::new(functions::connector_handler::Connector(handler)),
    );
}

/// Build the custom function handlers for the dataflow-rs engine.
///
/// Registers the nine Orion-specific handlers (`http_call`, `channel_call`,
/// `db_read`, `db_write`, `data_query`, `data_write`, `cache_read`,
/// `cache_write`, `mongo_read`) plus a stub `publish_kafka`. Call
/// [`register_kafka_publisher`] afterwards to swap the stub for the real
/// Kafka-backed handler once the producer is initialised.
pub fn build_custom_functions(
    deps: HandlerDeps<'_>,
) -> HashMap<String, dataflow_rs::BoxedFunctionHandler> {
    let HandlerDeps {
        registry,
        client,
        runtime,
        jwks,
        engine_config,
        query_config,
        write_config,
        cache_pool,
        sql_pool_cache,
        mongo_pool_cache,
        smtp_pool_cache,
    } = deps;
    let mut fns: HashMap<String, dataflow_rs::BoxedFunctionHandler> = HashMap::new();

    register(
        &mut fns,
        functions::http_call::HttpCallHandler {
            registry: registry.clone(),
            client: client.clone(),
        },
    );

    fns.insert(
        "channel_call".to_string(),
        Box::new(functions::channel_call::ChannelCallHandler {
            runtime,
            max_call_depth: engine_config.max_channel_call_depth,
            default_timeout_ms: engine_config.default_channel_call_timeout_ms,
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

    // Register cache handlers (cache_read, cache_write).
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
            cache_pool,
            registry: registry.clone(),
        },
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

/// Register the real Kafka-backed publish_kafka handler.
///
/// Replaces the stub handler (or adds the handler if not yet registered).
pub fn register_kafka_publisher(
    fns: &mut HashMap<String, dataflow_rs::BoxedFunctionHandler>,
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
