//! The function-name vocabulary a workflow may draw on, and the construction
//! of the handlers behind Orion's own entries.

use std::collections::HashMap;
use std::sync::Arc;

use super::functions;
use crate::connector::{ConnectorRegistry, ConnectorType};

/// Whether a workflow may name `function`.
///
/// This gates workflow **creation** (`validation/workflows.rs`), so a name
/// this rejects is not a warning — the workflow is refused outright. The rule
/// is "would the engine Orion actually builds be able to run this task?", and
/// dataflow-rs 3.1 splits its own built-ins by exactly that question:
///
/// - [`BuiltinKind::SelfContained`] — `map`, `validate`, `filter`, `log`,
///   `parse_*`, `publish_*`. The crate executes these itself; always runnable.
/// - [`BuiltinKind::RequiresHandler`] — `http_call`, `enrich`,
///   `publish_kafka`. These deserialize into a *typed built-in* variant, so
///   `Engine::new` accepts them without complaint, and then dispatch to a
///   handler registered under the same name. Orion registers `http_call` and
///   `publish_kafka`; it does **not** register `enrich`.
/// - Everything else lands in `FunctionConfig::Custom` and needs a handler
///   too — that is [`CUSTOM_HANDLER_FUNCTIONS`].
///
/// So membership is the wrong test and `enrich` is why: it was added to a
/// hand-copied name list to stop create rejecting it (F54), which made every
/// `enrich` workflow activate cleanly and then fail its every request with
/// `FunctionNotFound`. Keying on the kind makes that unexpressible — a
/// `RequiresHandler` name is accepted only if Orion has the handler.
///
/// [`BuiltinKind`]: dataflow_rs::BuiltinKind
/// [`BuiltinKind::SelfContained`]: dataflow_rs::BuiltinKind::SelfContained
/// [`BuiltinKind::RequiresHandler`]: dataflow_rs::BuiltinKind::RequiresHandler
/// [`CUSTOM_HANDLER_FUNCTIONS`]: super::CUSTOM_HANDLER_FUNCTIONS
pub fn is_known_function(function: &str) -> bool {
    match dataflow_rs::builtin_function_kind(function) {
        Some(dataflow_rs::BuiltinKind::SelfContained) => true,
        // `RequiresHandler` and `Custom` alike: only if Orion registered one.
        _ => super::CUSTOM_HANDLER_FUNCTIONS.contains(&function),
    }
}

/// Every function name a workflow may reference, for callers that need the set
/// rather than a membership test — the `/admin/functions` catalogue and the
/// coverage tests that iterate it as an enumeration domain.
pub fn known_functions() -> impl Iterator<Item = &'static str> {
    dataflow_rs::BUILTIN_FUNCTION_NAMES
        .iter()
        .copied()
        .filter(|name| is_known_function(name))
        .chain(
            // `http_call` and `publish_kafka` are on both lists — an upstream
            // built-in name that Orion also supplies the handler for — so take
            // them from the built-in half only.
            super::CUSTOM_HANDLER_FUNCTIONS
                .iter()
                .copied()
                .filter(|name| !dataflow_rs::is_builtin_function(name)),
        )
}

/// Function names that require a connector reference.
pub const CONNECTOR_FUNCTIONS: &[&str] = &[
    "http_call",
    "publish_kafka",
    "db_read",
    "db_write",
    "data_query",
    "data_write",
    "cache_read",
    "cache_write",
    "mongo_read",
];

/// Which connector types a given function can actually run against.
///
/// F52: `ensure_workflow_connectors_exist` checked only that the referenced
/// connector *existed*, so pointing `cache_read` at a `db` connector activated
/// cleanly and then 500'd on the first request — a runtime discovery for
/// something fully determined at authoring time. Every handler already refuses
/// the wrong variant via `require_*_connector`; this is the same knowledge,
/// available before the workflow serves traffic.
///
/// `None` means the function takes no connector. The slices are non-empty and
/// ordered as they should read in an error message.
pub fn required_connector_types(function: &str) -> Option<&'static [ConnectorType]> {
    use ConnectorType::{Cache, Db, Es, Http, Kafka};
    Some(match function {
        "http_call" => &[Http],
        "publish_kafka" => &[Kafka],
        "cache_read" | "cache_write" => &[Cache],
        // `db_read`/`db_write` speak raw SQL and `mongo_read` speaks Mongo, but
        // both backends are one `ConnectorConfig::Db` variant distinguished by
        // the connection-string scheme — which the handlers check at call time
        // (`reject_mongo_connector`, `is_mongo`). The type gate stops at `db`.
        "db_read" | "db_write" | "mongo_read" => &[Db],
        // The portable dialect is the one pair that spans backends.
        "data_query" | "data_write" => &[Db, Es],
        _ => return None,
    })
}

/// Whether `function` needs a `database` key alongside its connector.
///
/// True only for MongoDB, which has no database in its connection string the
/// driver will default to. `mongo_read` declares it required outright;
/// `data_query`/`data_write` cannot, because the same task shape is valid
/// against SQL and Elasticsearch — so for those it is conditional on the
/// connector actually being Mongo, and that is checked at activation rather
/// than at first request (F52).
pub fn requires_mongo_database(function: &str) -> bool {
    matches!(function, "mongo_read" | "data_query" | "data_write")
}

/// Everything the ten custom handlers need to be constructed.
///
/// F44: this was ten positional parameters carrying
/// `#[allow(clippy::too_many_arguments)]`, and the full call was written out
/// twice — once at boot and once in `POST /workflows/{id}/test`, which builds a
/// throwaway engine over the same dependencies. Two call sites with ten
/// same-typed `Arc`s between them is a silent-transposition hazard the compiler
/// cannot see; every field here already lives on `AppState`, so
/// [`HandlerDeps::from_state`] is the only form the second call site needs.
pub struct HandlerDeps<'a> {
    pub registry: Arc<ConnectorRegistry>,
    pub client: reqwest::Client,
    pub engine: Arc<crate::engine::EngineHandle>,
    pub channel_registry: Arc<crate::channel::ChannelRegistry>,
    pub engine_config: &'a crate::config::EngineConfig,
    pub query_config: &'a crate::config::QueryConfig,
    pub write_config: &'a crate::config::WriteConfig,
    pub cache_pool: Arc<crate::connector::cache_backend::CachePool>,
    pub sql_pool_cache: Arc<crate::connector::pool_cache::SqlPoolCache>,
    pub mongo_pool_cache: Arc<crate::connector::mongo_pool::MongoPoolCache>,
}

impl<'a> HandlerDeps<'a> {
    /// Borrow the handler dependencies straight off a live `AppState`.
    ///
    /// The dry-run engine in `POST /workflows/{id}/test` must be built from the
    /// *same* registries and pools as the serving engine — a copy that drifted
    /// would make dry-run results a lie about production.
    pub fn from_state(state: &'a crate::server::state::AppState) -> Self {
        Self {
            registry: state.connector_registry.clone(),
            client: state.http_client.clone(),
            engine: state.engine.clone(),
            channel_registry: state.channel_registry.clone(),
            engine_config: &state.config.engine,
            query_config: &state.config.query,
            write_config: &state.config.write,
            cache_pool: state.cache_pool.clone(),
            sql_pool_cache: state.sql_pool_cache.clone(),
            mongo_pool_cache: state.mongo_pool_cache.clone(),
        }
    }
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
        engine,
        channel_registry,
        engine_config,
        query_config,
        write_config,
        cache_pool,
        sql_pool_cache,
        mongo_pool_cache,
    } = deps;
    let mut fns: HashMap<String, dataflow_rs::BoxedFunctionHandler> = HashMap::new();

    fns.insert(
        "http_call".to_string(),
        Box::new(functions::http_call::HttpCallHandler {
            registry: registry.clone(),
            client: client.clone(),
        }),
    );

    fns.insert(
        "channel_call".to_string(),
        Box::new(functions::channel_call::ChannelCallHandler {
            engine,
            channel_registry,
            max_call_depth: engine_config.max_channel_call_depth,
            default_timeout_ms: engine_config.default_channel_call_timeout_ms,
        }),
    );

    // Register stub publish_kafka (will be replaced by register_kafka_publisher when Kafka is configured)
    fns.insert(
        "publish_kafka".to_string(),
        Box::new(functions::publish_kafka::PublishKafkaHandler {
            registry: registry.clone(),
            producers: None,
        }),
    );

    // Register SQL database handlers (db_read, db_write)
    fns.insert(
        "db_read".to_string(),
        Box::new(functions::db_read::DbReadHandler {
            pool_cache: sql_pool_cache.clone(),
            registry: registry.clone(),
            max_rows: query_config.max_limit as usize,
        }),
    );
    fns.insert(
        "db_write".to_string(),
        Box::new(functions::db_write::DbWriteHandler {
            pool_cache: sql_pool_cache.clone(),
            registry: registry.clone(),
        }),
    );

    // Register the portable query handler (data_query). It renders a
    // backend-neutral filter + envelope to native SQL or a MongoDB find
    // (§ src/query/).
    fns.insert(
        "data_query".to_string(),
        Box::new(functions::data_query::DataQueryHandler {
            pool_cache: sql_pool_cache.clone(),
            mongo_pool_cache: mongo_pool_cache.clone(),
            http_client: client.clone(),
            registry: registry.clone(),
            limits: query_config.clone(),
        }),
    );

    // Register the portable write handler (data_write). It renders a
    // backend-neutral mutation envelope to a native SQL INSERT/UPDATE/DELETE/upsert,
    // a MongoDB write, or an Elasticsearch write (§ src/query/write.rs).
    fns.insert(
        "data_write".to_string(),
        Box::new(functions::data_write::DataWriteHandler {
            pool_cache: sql_pool_cache,
            mongo_pool_cache: mongo_pool_cache.clone(),
            http_client: client.clone(),
            registry: registry.clone(),
            write_config: write_config.clone(),
        }),
    );

    // Register cache handlers (cache_read, cache_write).
    // CachePool routes to the in-memory or Redis backend per connector config.
    fns.insert(
        "cache_read".to_string(),
        Box::new(functions::cache_read::CacheReadHandler {
            cache_pool: cache_pool.clone(),
            registry: registry.clone(),
        }),
    );
    fns.insert(
        "cache_write".to_string(),
        Box::new(functions::cache_write::CacheWriteHandler {
            cache_pool,
            registry: registry.clone(),
        }),
    );

    // Register MongoDB handler (mongo_read)
    fns.insert(
        "mongo_read".to_string(),
        Box::new(functions::mongo_read::MongoReadHandler {
            pool_cache: mongo_pool_cache,
            registry: registry.clone(),
            max_rows: query_config.max_limit as usize,
        }),
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
    fns.insert(
        "publish_kafka".to_string(),
        Box::new(functions::publish_kafka::PublishKafkaHandler {
            registry,
            producers: Some(producers),
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::loader::CUSTOM_HANDLER_FUNCTIONS;
    use dataflow_rs::BuiltinKind;

    /// Every self-contained dataflow-rs built-in is accepted.
    ///
    /// This used to build a probe engine, let it fail, and **string-parse the
    /// built-in list out of the `FunctionNotFound` Display impl** — because the
    /// crate kept `BUILTIN_FUNCTION_NAMES` `pub(crate)` and the error message
    /// was the only public surface that enumerated it. 3.1 publishes the const
    /// and a classifier, and documents that message as explicitly unpinned.
    #[test]
    fn every_self_contained_builtin_is_accepted() {
        let mut checked = 0;
        for name in dataflow_rs::BUILTIN_FUNCTION_NAMES {
            if dataflow_rs::builtin_function_kind(name) == Some(BuiltinKind::SelfContained) {
                assert!(
                    is_known_function(name),
                    "'{name}' runs with no registration, so rejecting it at create \
                     refuses a workflow the engine would happily execute"
                );
                checked += 1;
            }
        }
        assert!(checked >= 8, "implausibly few self-contained built-ins");
    }

    /// A built-in that needs a handler is accepted only if Orion registered
    /// one — and `enrich` is the case that proves it matters.
    ///
    /// `enrich` deserializes into a typed built-in variant, so `Engine::new`
    /// accepts it and `check_custom_inputs` skips it by construction: it never
    /// becomes `FunctionConfig::Custom`. It was added to the old hand-copied
    /// name list to stop create rejecting it, which meant every `enrich`
    /// workflow activated cleanly and then failed *every* request with
    /// `FunctionNotFound`. Nothing registers a handler for it.
    #[test]
    fn a_builtin_needing_a_handler_is_accepted_only_when_one_is_registered() {
        for name in dataflow_rs::BUILTIN_FUNCTION_NAMES {
            if dataflow_rs::builtin_function_kind(name) != Some(BuiltinKind::RequiresHandler) {
                continue;
            }
            assert_eq!(
                is_known_function(name),
                CUSTOM_HANDLER_FUNCTIONS.contains(name),
                "'{name}' needs a registered handler; accepting it without one \
                 green-lights a workflow that 500s on every request"
            );
        }
        assert!(is_known_function("http_call"));
        assert!(is_known_function("publish_kafka"));
        assert!(
            !is_known_function("enrich"),
            "Orion registers no `enrich` handler, so the name must be refused \
             at create rather than at every request"
        );
    }

    #[test]
    fn every_registered_custom_handler_is_accepted() {
        // A name registered as a handler but rejected by the gate refuses a
        // workflow that would work; the reverse — accepted, unregistered, not
        // a self-contained builtin — reaches the engine build and kills it.
        // Both directions must hold.
        for name in CUSTOM_HANDLER_FUNCTIONS {
            assert!(
                is_known_function(name),
                "handler '{name}' is registered but the gate rejects it, \
                 so workflows using it are refused at create"
            );
        }
        assert!(!is_known_function("__not_a_function__"));
    }

    /// F52: the type table and the "needs a connector" list are two views of
    /// the same fact, and an entry missing from either is a runtime-only
    /// discovery. `CONNECTOR_FUNCTIONS` drives the existence check; the table
    /// drives the type check; a function in one and not the other means one of
    /// the two checks silently skips it.
    #[test]
    fn every_connector_function_declares_its_connector_types() {
        for f in CONNECTOR_FUNCTIONS {
            let types = required_connector_types(f).unwrap_or_default();
            assert!(
                !types.is_empty(),
                "'{f}' takes a connector but declares no connector type, so activation \
                 cannot check it (proposal F52)"
            );
        }
        // And nothing else claims one.
        for f in known_functions() {
            if !CONNECTOR_FUNCTIONS.contains(&f) {
                assert!(
                    required_connector_types(f).is_none(),
                    "'{f}' declares connector types but takes no connector"
                );
            }
        }
    }
}
