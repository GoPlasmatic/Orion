//! The function-name vocabulary a workflow may draw on, and the construction
//! of the handlers behind Orion's own entries.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use super::functions;
use crate::connector::{ConnectorRegistry, ConnectorType};

/// Every function name a workflow may reference: dataflow-rs built-ins plus
/// Orion's own handlers.
///
/// This gates workflow **creation** (`validation/workflows.rs`), so a name
/// missing here is not a warning — the workflow is rejected with
/// `unknown_function` even though the engine would run it fine. `enrich` was
/// missing for exactly that reason (F54).
///
/// dataflow-rs keeps its own list `pub(crate)`, so this cannot be derived at
/// compile time. `known_functions_covers_every_dataflow_builtin` derives it at
/// *test* time out of the engine's own `FunctionNotFound` message, which
/// enumerates the built-ins — so a dependency bump that adds or renames one
/// fails the test instead of silently rejecting valid workflows.
pub const KNOWN_FUNCTIONS: &[&str] = &[
    "map",
    // Upstream accepts both spellings, so Orion must too — dropping either
    // would reject a workflow the engine runs.
    "validation",
    "validate",
    "parse_json",
    "parse_xml",
    "publish_json",
    "publish_xml",
    "filter",
    "log",
    "http_call",
    "enrich",
    "publish_kafka",
    "db_read",
    "db_write",
    "data_query",
    "data_write",
    "cache_read",
    "cache_write",
    "mongo_read",
    "channel_call",
];

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
    pub engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
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
            default_limit: query_config.default_limit,
            max_limit: query_config.max_limit,
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
            max_rows: write_config.max_rows,
            allow_unfiltered: write_config.allow_unfiltered,
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

    /// F54: `KNOWN_FUNCTIONS` gates workflow *creation*, so a dataflow-rs
    /// built-in missing from it is rejected with `unknown_function` even
    /// though the engine runs it. `enrich` was missing exactly that way.
    ///
    /// dataflow-rs keeps `BUILTIN_FUNCTION_NAMES` `pub(crate)`, so the list
    /// cannot be imported — but `Engine::new` enumerates it in the
    /// `FunctionNotFound` message raised for an unregistered name. Deriving it
    /// from there means a dependency bump that adds or renames a built-in
    /// fails here instead of silently rejecting valid workflows.
    #[test]
    fn known_functions_covers_every_dataflow_builtin() {
        let workflow = dataflow_rs::Workflow::from_json(
            r#"{"id":"probe","name":"probe","priority":0,"condition":true,
                "tasks":[{"id":"t","name":"t",
                          "function":{"name":"__orion_probe__","input":{}}}]}"#,
        )
        .expect("probe workflow parses");
        let built = dataflow_rs::Engine::new(vec![workflow], std::collections::HashMap::new());
        assert!(
            built.is_err(),
            "an unregistered function must fail the engine build"
        );
        let err = built
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| unreachable!("asserted is_err above"));

        let builtins = err
            .split_once("built-ins: ")
            .map(|(_, rest)| rest.trim_end_matches([')', '.', ' ']))
            .unwrap_or_else(|| {
                unreachable!("dataflow-rs no longer lists its built-ins in FunctionNotFound: {err}")
            });
        let builtins: Vec<&str> = builtins.split(", ").map(str::trim).collect();
        assert!(
            builtins.len() >= 10,
            "parsed an implausible built-in list from {err}: {builtins:?}"
        );

        let missing: Vec<&&str> = builtins
            .iter()
            .filter(|b| !KNOWN_FUNCTIONS.contains(b))
            .collect();
        assert!(
            missing.is_empty(),
            "dataflow-rs built-ins absent from KNOWN_FUNCTIONS: {missing:?} — \
             workflows using them are rejected at create with `unknown_function`"
        );
    }

    #[test]
    fn known_functions_covers_every_registered_custom_handler() {
        // A name registered as a handler but missing from KNOWN_FUNCTIONS is
        // rejected by admin validation even though it would work; the reverse
        // (in KNOWN_FUNCTIONS, unregistered, not a builtin) reaches the engine
        // build and kills it. Both directions must hold.
        for name in CUSTOM_HANDLER_FUNCTIONS {
            assert!(
                KNOWN_FUNCTIONS.contains(name),
                "handler '{name}' is registered but absent from KNOWN_FUNCTIONS, \
                 so workflows using it are rejected at activation"
            );
        }
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
        for f in KNOWN_FUNCTIONS {
            if !CONNECTOR_FUNCTIONS.contains(f) {
                assert!(
                    required_connector_types(f).is_none(),
                    "'{f}' declares connector types but takes no connector"
                );
            }
        }
    }
}
