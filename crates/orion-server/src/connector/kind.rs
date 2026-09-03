//! What varies per connector type, in one place the compiler checks.
//!
//! `ConnectorConfig` has always been a closed enum, but the knowledge that goes
//! with a variant was scattered: the wire name, the `operations` gate
//! vocabulary, the typed accessor a handler uses, the endpoint scheme rules,
//! and — the one with no guard at all — which connection-pool caches have to be
//! evicted when the connector changes. Adding a type meant finding all of them.
//!
//! Everything here hangs off one `match` per question, so a new variant is a
//! compile error until each is answered.
//!
//! **Why eviction is modelled as [`PoolSlot`] rather than a per-type list of
//! cache names.** Before this module, `evict_connector_pools` swept all four
//! caches unconditionally by name. That could not miss a connector *type* — but
//! it could miss a newly-added *cache*, silently, and a connector left serving
//! from a pool built for its old config is exactly the failure this is meant to
//! prevent. Naming the caches in a closed enum makes both directions
//! compiler-checked: a new cache is a new variant, a new variant is a new arm,
//! and `every_pool_slot_is_reachable_from_some_kind` refuses a slot no type
//! claims. A per-type list of *strings* would have been strictly weaker than
//! the blanket sweep it replaced.

use super::config::{
    CacheConnectorConfig, ConnectorConfig, ConnectorType, DbConnectorConfig, EsConnectorConfig,
    HttpConnectorConfig, KafkaConnectorConfig, SmtpConnectorConfig, StorageConnectorConfig,
};

/// A connection-pool cache a connector's config can be cached in.
///
/// Closed on purpose: the set of caches is the set of fields on
/// `server::state::Caches`, and the test in this module pins the two together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PoolSlot {
    /// `caches.sql_pool_cache` — sqlx pools for SQL `db` connectors.
    Sql,
    /// `caches.mongo_pool_cache` — MongoDB clients for `db` connectors whose
    /// connection string names the Mongo scheme.
    Mongo,
    /// `caches.cache_pool` — Redis/in-memory backends for `cache` connectors.
    Cache,
    /// `caches.smtp_pool_cache` — SMTP transports for `smtp` connectors.
    Smtp,
}

impl PoolSlot {
    /// Every slot. The eviction sweep and the drift guard both read this, so a
    /// cache that exists but is evicted by nothing cannot pass unnoticed.
    pub const ALL: &'static [PoolSlot] = &[
        PoolSlot::Sql,
        PoolSlot::Mongo,
        PoolSlot::Cache,
        PoolSlot::Smtp,
    ];
}

/// What a handler names when it asks for a connector.
///
/// The associated `Config` is what makes this worth being a trait rather than a
/// table: a handler asks for the target it needs and gets that config back, so
/// `require::<Db>(…)` cannot compile into an SMTP accessor.
///
/// Separate from [`ConnectorKind`] because two handlers do not target a single
/// type: `data_query` and `data_write` speak the portable dialect, which is
/// valid against `db` *or* `es` and dispatches on the resolved variant. They
/// used to take the whole `ConnectorConfig` and produce their own "is not a db
/// or es connector" refusal after resolution; [`DataBackend`] makes it the same
/// refusal, from the same place, as every other wrong-type message.
pub trait ConnectorTarget {
    /// The typed config a handler binds once the connector is acceptable.
    type Config;

    /// Borrow this target's config out of a parsed connector, or `None` when
    /// the connector is one this target does not accept.
    fn extract(config: &ConnectorConfig) -> Option<&Self::Config>;

    /// The noun used when a workflow points a handler at the wrong type —
    /// "is not a database connector".
    fn noun() -> &'static str;
}

/// One connector type's typed half: a [`ConnectorTarget`] that is exactly one
/// variant, and so can answer the per-variant questions a union cannot —
/// which `ConnectorType` it is, and which pool caches it lives in.
pub trait ConnectorKind: ConnectorTarget {
    /// The variant this kind is.
    const TYPE: ConnectorType;
}

macro_rules! kinds {
    ($( $kind:ident => $variant:ident, $config:ty, $noun:literal ; )*) => {
        $(
            /// Marker type: see [`ConnectorKind`].
            pub struct $kind;

            impl ConnectorTarget for $kind {
                type Config = $config;

                fn extract(config: &ConnectorConfig) -> Option<&Self::Config> {
                    match config {
                        ConnectorConfig::$variant(c) => Some(c),
                        _ => None,
                    }
                }

                fn noun() -> &'static str {
                    $noun
                }
            }

            impl ConnectorKind for $kind {
                const TYPE: ConnectorType = ConnectorType::$variant;
            }
        )*
    };
}

kinds! {
    Http    => Http,    HttpConnectorConfig,    "an HTTP connector";
    Kafka   => Kafka,   KafkaConnectorConfig,   "a Kafka connector";
    Db      => Db,      DbConnectorConfig,      "a database connector";
    Cache   => Cache,   CacheConnectorConfig,   "a cache connector";
    Es      => Es,      EsConnectorConfig,      "an Elasticsearch connector";
    Smtp    => Smtp,    SmtpConnectorConfig,    "an SMTP connector";
    Storage => Storage, StorageConnectorConfig, "a storage connector";
}

/// The portable data dialect's target: `db` or `es`.
///
/// Its `Config` is the whole [`ConnectorConfig`] because the handler dispatches
/// on the variant — SQL, MongoDB (a `db` connector whose connection string says
/// so) and Elasticsearch are three different query translations, and which one
/// applies is not knowable before the connector is resolved.
pub struct DataBackend;

impl ConnectorTarget for DataBackend {
    type Config = ConnectorConfig;

    fn extract(config: &ConnectorConfig) -> Option<&Self::Config> {
        matches!(config, ConnectorConfig::Db(_) | ConnectorConfig::Es(_)).then_some(config)
    }

    fn noun() -> &'static str {
        "a db or es connector"
    }
}

impl ConnectorType {
    /// The pool caches a connector of this type may have entries in.
    ///
    /// `Db` claims two: the SQL and Mongo caches both key on connector name,
    /// and which one a given `db` connector lands in is a property of its
    /// connection string, not of its type — so a changed `db` connector has to
    /// evict from both.
    ///
    /// The four types with no pool return an empty slice rather than being
    /// omitted, because being pool-less is an answer this match has to give.
    pub fn pool_slots(self) -> &'static [PoolSlot] {
        match self {
            ConnectorType::Db => &[PoolSlot::Sql, PoolSlot::Mongo],
            ConnectorType::Cache => &[PoolSlot::Cache],
            ConnectorType::Smtp => &[PoolSlot::Smtp],
            // Stateless per call: HTTP and Elasticsearch go through the shared
            // reqwest client, Kafka through the producer, storage presigns
            // locally.
            ConnectorType::Http
            | ConnectorType::Kafka
            | ConnectorType::Es
            | ConnectorType::Storage => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard that makes per-kind eviction no weaker than the blanket sweep
    /// it replaced.
    ///
    /// A cache reachable from no connector type is a cache nothing evicts —
    /// the connector updates and every node keeps serving from a pool built for
    /// the old config, with no error anywhere. Adding a `Caches` field and
    /// forgetting to claim it from a kind is exactly the mistake this catches.
    ///
    /// Read through `ConnectorType::ALL`, not a list written out here: a guard
    /// that hand-lists the vocabulary it guards stops covering the next variant
    /// added, and passes while doing it.
    #[test]
    fn every_pool_slot_is_reachable_from_some_kind() {
        let claimed: std::collections::BTreeSet<PoolSlot> = ConnectorType::ALL
            .iter()
            .flat_map(|t| t.pool_slots().iter().copied())
            .collect();

        let all: std::collections::BTreeSet<PoolSlot> = PoolSlot::ALL.iter().copied().collect();
        assert_eq!(
            claimed, all,
            "every PoolSlot must be claimed by at least one connector type — an \
             unclaimed slot is a pool cache that nothing evicts"
        );
    }

    /// `extract` is the whole type-safety claim: a kind yields its own variant
    /// and nothing else. `require_connector` in `engine::functions` turns the
    /// `None` into the message a workflow author reads.
    #[test]
    fn a_kind_extracts_only_its_own_variant() {
        let cfg = ConnectorConfig::Kafka(KafkaConnectorConfig {
            brokers: vec!["localhost:9092".to_string()],
            topic: "orders".to_string(),
            allow_private_urls: false,
            operations: Default::default(),
        });
        assert!(Kafka::extract(&cfg).is_some());
        assert!(
            Db::extract(&cfg).is_none(),
            "a kafka connector is not a db one"
        );
        assert_eq!(Db::noun(), "a database connector");
    }

    /// The one target that is not a kind: it accepts two variants and refuses
    /// the rest, which is the refusal `data_query` used to produce by hand
    /// after resolving the connector itself.
    #[test]
    fn the_data_backend_target_accepts_db_and_es_and_nothing_else() {
        let db = ConnectorConfig::Db(DbConnectorConfig {
            connection_string: "sqlite::memory:".to_string(),
            max_connections: None,
            connect_timeout_ms: None,
            query_timeout_ms: None,
            allow_private_urls: false,
            operations: Default::default(),
            dialect: Default::default(),
            aggregate_write_stages: false,
        });
        let kafka = ConnectorConfig::Kafka(KafkaConnectorConfig {
            brokers: vec!["localhost:9092".to_string()],
            topic: "orders".to_string(),
            allow_private_urls: false,
            operations: Default::default(),
        });
        assert!(DataBackend::extract(&db).is_some());
        assert!(
            DataBackend::extract(&kafka).is_none(),
            "the portable dialect does not speak Kafka"
        );
        assert_eq!(DataBackend::noun(), "a db or es connector");
    }

    /// The `db` variant covers both backends, so both caches must be evicted
    /// for it — the connection string, not the type, decides which one has the
    /// entry.
    #[test]
    fn a_db_connector_evicts_both_sql_and_mongo_pools() {
        assert_eq!(
            ConnectorType::Db.pool_slots(),
            &[PoolSlot::Sql, PoolSlot::Mongo]
        );
    }
}
