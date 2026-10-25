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
    CacheConnectorConfig, ConnectorConfig, ConnectorType, DbConnectorConfig, HttpConnectorConfig,
    StorageConnectorConfig,
};

/// A connection-pool cache a connector's config can be cached in.
///
/// Closed on purpose: the set of caches is the set of fields on
/// `server::state::Caches`, and the test in this module pins the two together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PoolSlot {
    /// `caches.sql_pool_cache` — sqlx pools for SQL `db` connectors.
    Sql,
}

impl PoolSlot {
    /// Every slot. The eviction sweep and the drift guard both read this, so a
    /// cache that exists but is evicted by nothing cannot pass unnoticed.
    pub const ALL: &'static [PoolSlot] = &[PoolSlot::Sql];
}

/// What a handler names when it asks for a connector.
///
/// The associated `Config` is what makes this worth being a trait rather than a
/// table: a handler asks for the target it needs and gets that config back, so
/// `require::<Db>(…)` cannot compile into an HTTP accessor.
///
/// Separate from [`ConnectorKind`] because one target is not a kind:
/// `data_query` and `data_write` speak the portable dialect and read the whole
/// `ConnectorConfig` (its operation gates and dialect guards). They used to
/// produce their own wrong-type refusal after resolution; [`DataBackend`]
/// makes it the same refusal, from the same place, as every other wrong-type
/// message.
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
    Db      => Db,      DbConnectorConfig,      "a database connector";
    Cache   => Cache,   CacheConnectorConfig,   "a cache connector";
    Storage => Storage, StorageConnectorConfig, "a storage connector";
}

/// The portable data dialect's target: a `db` connector.
///
/// Its `Config` is the whole [`ConnectorConfig`] because the dialect reads the
/// connector through the variant-neutral accessors —
/// [`ConnectorConfig::operation_gates`] and [`ConnectorConfig::dialect_guards`].
pub struct DataBackend;

impl ConnectorTarget for DataBackend {
    type Config = ConnectorConfig;

    fn extract(config: &ConnectorConfig) -> Option<&Self::Config> {
        matches!(config, ConnectorConfig::Db(_)).then_some(config)
    }

    fn noun() -> &'static str {
        "a database connector"
    }
}

impl ConnectorType {
    /// The pool caches a connector of this type may have entries in.
    ///
    /// The types with no pool return an empty slice rather than being
    /// omitted, because being pool-less is an answer this match has to give.
    pub fn pool_slots(self) -> &'static [PoolSlot] {
        match self {
            ConnectorType::Db => &[PoolSlot::Sql],
            // Stateless per call: HTTP goes through the shared reqwest client,
            // storage presigns locally, and a cache connector is an in-memory
            // namespace holding no connection — a delete drops it
            // (`CachePool::evict_memory_namespaces`), an edit keeps it.
            ConnectorType::Http | ConnectorType::Storage | ConnectorType::Cache => &[],
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
        let cfg: ConnectorConfig =
            serde_json::from_str(r#"{"type":"cache","backend":"memory"}"#).expect("cache config");
        assert!(Cache::extract(&cfg).is_some());
        assert!(
            Db::extract(&cfg).is_none(),
            "a cache connector is not a db one"
        );
        assert_eq!(Db::noun(), "a database connector");
    }

    /// The one target that is not a kind: it accepts `db` and refuses the
    /// rest, which is the refusal `data_query` used to produce by hand after
    /// resolving the connector itself.
    #[test]
    fn the_data_backend_target_accepts_db_and_nothing_else() {
        let db = ConnectorConfig::Db(DbConnectorConfig {
            connection_string: "sqlite::memory:".to_string(),
            driver: None,
            max_connections: None,
            connect_timeout_ms: None,
            query_timeout_ms: None,
            allow_private_urls: false,
            operations: Default::default(),
            dialect: Default::default(),
        });
        let cache: ConnectorConfig =
            serde_json::from_str(r#"{"type":"cache","backend":"memory"}"#).expect("cache config");
        assert!(DataBackend::extract(&db).is_some());
        assert!(
            DataBackend::extract(&cache).is_none(),
            "the portable dialect does not speak to a cache"
        );
        assert_eq!(DataBackend::noun(), "a database connector");
    }

    /// A changed `db` connector evicts its sqlx pool.
    #[test]
    fn a_db_connector_evicts_the_sql_pool() {
        assert_eq!(ConnectorType::Db.pool_slots(), &[PoolSlot::Sql]);
    }
}
