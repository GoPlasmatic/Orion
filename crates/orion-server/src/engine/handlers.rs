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

/// The known function nearest to `name`, when one is close enough that a
/// typo is the likely explanation — the suggestion the `UNKNOWN_FUNCTION`
/// validation error appends.
///
/// Function names are short (4–15 characters), so the fixed distance-3 window
/// `config/unknown_env.rs` gives env-var overrides would surface suggestions
/// for inputs that are clearly unrelated ("x" is 3 edits from "map"). The
/// window here scales with the shorter name — a third of its length, clamped
/// to 1–3 edits — which covers the realistic typo shapes (a transposed pair,
/// a doubled letter, a missing suffix) and nothing else.
pub fn suggest_known_function(name: &str) -> Option<&'static str> {
    known_functions()
        .map(|candidate| (edit_distance(name, candidate), candidate))
        .filter(|(distance, candidate)| {
            let window = (name.len().min(candidate.len()) / 3).clamp(1, 3);
            *distance <= window
        })
        .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)))
        .map(|(_, candidate)| candidate)
}

/// Levenshtein distance, two-row form — the same shape as
/// `config::unknown_env::edit_distance`. The candidate set is ~27 short
/// names, so this is a few hundred cells per misspelled task name.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(ca != cb);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
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
    "mongo_write",
    "mongo_aggregate",
    "send_email",
    "storage_presign",
    "storage_head",
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
    use ConnectorType::{Cache, Db, Es, Http, Kafka, Smtp, Storage};
    Some(match function {
        "http_call" => &[Http],
        "publish_kafka" => &[Kafka],
        "send_email" => &[Smtp],
        "storage_presign" | "storage_head" => &[Storage],
        "cache_read" | "cache_write" => &[Cache],
        // `db_read`/`db_write` speak raw SQL and the `mongo_*` trio speaks
        // Mongo, but both backends are one `ConnectorConfig::Db` variant
        // distinguished by the connection-string scheme — which the handlers
        // check at call time (`reject_mongo_connector`, `is_mongo`). The type
        // gate stops at `db`.
        "db_read" | "db_write" | "mongo_read" | "mongo_write" | "mongo_aggregate" => &[Db],
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
    matches!(
        function,
        "mongo_read" | "mongo_write" | "mongo_aggregate" | "data_query" | "data_write"
    )
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
    pub smtp_pool_cache: Arc<crate::connector::smtp_pool::SmtpPoolCache>,
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
            cache_pool: state.caches.cache_pool.clone(),
            sql_pool_cache: state.caches.sql_pool_cache.clone(),
            mongo_pool_cache: state.caches.mongo_pool_cache.clone(),
            smtp_pool_cache: state.caches.smtp_pool_cache.clone(),
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
        smtp_pool_cache,
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
        Box::new(functions::jwt_verify::JwtVerifyHandler),
    );

    fns.insert(
        "send_email".to_string(),
        Box::new(functions::send_email::SendEmailHandler {
            registry: registry.clone(),
            smtp_pool: smtp_pool_cache,
        }),
    );

    fns.insert(
        "storage_presign".to_string(),
        Box::new(functions::storage_presign::StoragePresignHandler {
            registry: registry.clone(),
        }),
    );

    fns.insert(
        "storage_head".to_string(),
        Box::new(functions::storage_head::StorageHeadHandler {
            registry: registry.clone(),
            client: client.clone(),
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

    // Register the MongoDB trio (mongo_read, mongo_write, mongo_aggregate)
    fns.insert(
        "mongo_read".to_string(),
        Box::new(functions::mongo_read::MongoReadHandler {
            pool_cache: mongo_pool_cache.clone(),
            registry: registry.clone(),
            limits: query_config.clone(),
        }),
    );
    fns.insert(
        "mongo_write".to_string(),
        Box::new(functions::mongo_write::MongoWriteHandler {
            pool_cache: mongo_pool_cache.clone(),
            registry: registry.clone(),
            write_config: write_config.clone(),
        }),
    );
    fns.insert(
        "mongo_aggregate".to_string(),
        Box::new(functions::mongo_aggregate::MongoAggregateHandler {
            pool_cache: mongo_pool_cache,
            registry: registry.clone(),
            limits: query_config.clone(),
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

    /// The typos the `UNKNOWN_FUNCTION` message exists for: the suggestions
    /// must be the real, registered names they misspell.
    #[test]
    fn suggestion_recovers_common_typos() {
        for (typo, expected) in [
            ("mongo_writes", "mongo_write"),
            ("jwt_verifiy", "jwt_verify"),
            ("cache_readd", "cache_read"),
        ] {
            assert_eq!(
                suggest_known_function(typo),
                Some(expected),
                "'{typo}' should point at '{expected}'"
            );
        }
    }

    /// The window scales with name length, so garbage that is far from every
    /// registered name gets no suggestion rather than a wrong one —
    /// `http_request` is a plausible typo but seven edits from `http_call`.
    #[test]
    fn suggestion_is_silent_when_nothing_is_close() {
        assert_eq!(suggest_known_function("http_request"), None);
        assert_eq!(suggest_known_function("no_such_function_xyz"), None);
        assert_eq!(suggest_known_function("totally_unrelated"), None);
        assert_eq!(suggest_known_function("x"), None);
    }

    /// Suggestions only ever name something the engine can actually run.
    #[test]
    fn suggestion_never_names_an_unknown_function() {
        for typo in ["mongo_writes", "jwt_verifiy", "cache_readd"] {
            if let Some(candidate) = suggest_known_function(typo) {
                assert!(
                    is_known_function(candidate),
                    "'{candidate}' is suggested for '{typo}' but is not itself registered"
                );
            }
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
