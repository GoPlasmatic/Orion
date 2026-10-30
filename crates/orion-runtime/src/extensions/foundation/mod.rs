//! Community Edition foundation Direct Rust connector backends.
//!
//! Implements the five foundational Direct Rust (`ImplementationKind::Rust`)
//! connector providers maintained in the public Community repository:
//! - [`HttpProviderBackend`] (`"org.orion.http"`)
//! - [`PostgresProviderBackend`] (`"org.orion.postgresql"`)
//! - [`MysqlProviderBackend`] (`"org.orion.mysql"`) — MySQL and MariaDB
//! - [`SqliteProviderBackend`] (`"org.orion.sqlite"`) — enforces approved-root
//!   path sandboxing and rejects path traversal or Orion's own config DB
//! - [`MemoryCacheProviderBackend`] (`"org.orion.memory-cache"`) — bounded,
//!   TTL-aware, connection-partitioned in-memory cache

mod http;
mod memory_cache;
mod sql;

#[cfg(test)]
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
#[cfg(test)]
use std::time::Instant;

use serde_json::Value;
#[cfg(test)]
use serde_json::json;

use super::resource_pool::VersionedSessionPool;
use super::{
    AttemptContext, ConnectionPoolCache, ConnectorError, ConnectorOutput, Digest, EffectStatus,
    EffectiveConnectionIdentity, ImplementationKind, ProviderBackend, ProviderId, ProviderRef,
    codes,
};

pub use http::HttpProviderBackend;
#[cfg(test)]
use http::{HttpAuth, authorize, http_auth};
pub use memory_cache::{MemoryCacheProviderBackend, PartitionedMemoryCache};
#[cfg(test)]
use sql::SQL_STATEMENT_KEYS;
pub use sql::{
    MysqlDialect, MysqlProviderBackend, PostgresDialect, PostgresProviderBackend, RemoteSqlDialect,
    RemoteSqlProviderBackend, SqliteProviderBackend,
};

/// Canonical driver IDs for the five Community foundation Direct Rust providers.
pub const DRIVER_HTTP: &str = "org.orion.http";
pub const DRIVER_POSTGRESQL: &str = "org.orion.postgresql";
pub const DRIVER_MYSQL: &str = "org.orion.mysql";
pub const DRIVER_SQLITE: &str = "org.orion.sqlite";
pub const DRIVER_MEMORY_CACHE: &str = "org.orion.memory-cache";

/// All Community foundation Direct Rust driver identifiers.
pub const COMMUNITY_RUST_DRIVERS: &[&str] = &[
    DRIVER_HTTP,
    DRIVER_POSTGRESQL,
    DRIVER_MYSQL,
    DRIVER_SQLITE,
    DRIVER_MEMORY_CACHE,
];

/// The connector type a built-in driver serves the built-in task functions
/// as (`http_call`, `db_read`/`data_*`, `cache_*`), and its connection's
/// configuration in that type's shape. `None` for a driver that serves only
/// `connector_call`.
pub fn builtin_connector(driver_id: &str, mut config: Value) -> Option<(&'static str, Value)> {
    let connector_type = match driver_id {
        DRIVER_HTTP => "http",
        DRIVER_POSTGRESQL | DRIVER_MYSQL | DRIVER_SQLITE => "db",
        DRIVER_MEMORY_CACHE => {
            // The one backend this driver has, so a connection need not say so.
            if let Some(object) = config.as_object_mut() {
                object
                    .entry("backend")
                    .or_insert_with(|| Value::String("memory".to_string()));
            }
            "cache"
        }
        _ => return None,
    };
    Some((connector_type, config))
}

/// The contract generation the built-in drivers' identities name. It moves
/// only when a driver's operations change incompatibly, never with the
/// build, so a connection revision naming a built-in provider stays valid
/// across upgrades.
const BUILTIN_PROVIDER_CONTRACT: u32 = 1;

/// The publisher a built-in driver's [`ProviderRef`] names.
pub const BUILTIN_PUBLISHER: &str = "org.orion";

/// Compute the deterministic synthetic package digest for a compiled-in `rust`
/// driver (`sha256("orion-builtin-provider:{driver_id}@1")`).
pub fn synthetic_provider_digest(driver_id: &str) -> Digest {
    crate::crypto::sha256_digest(
        format!("orion-builtin-provider:{driver_id}@{BUILTIN_PROVIDER_CONTRACT}").as_bytes(),
    )
}

/// Build the synthetic [`ProviderRef`] for a compiled-in `rust` driver.
pub fn synthetic_provider_ref(driver_id: &str) -> ProviderRef {
    ProviderRef {
        publisher: BUILTIN_PUBLISHER.to_string(),
        provider_id: ProviderId::from(driver_id),
        version: crate::version::ORION_VERSION.to_string(),
        digest: synthetic_provider_digest(driver_id),
    }
}

/// Each built-in driver and the digest it answers to, computed once.
static BUILTIN_DIGESTS: LazyLock<Vec<(&'static str, Digest)>> = LazyLock::new(|| {
    COMMUNITY_RUST_DRIVERS
        .iter()
        .map(|driver| (*driver, synthetic_provider_digest(driver)))
        .collect()
});

/// The package digest a built-in driver answers to: a connection revision
/// names a provider by digest, and a built-in driver has no uploaded
/// package, so this stable digest of its id and contract stands in for one.
/// `None` for an id this build does not ship.
///
/// The table is Community's: a `rust` backend an edition composes beyond
/// `COMMUNITY_RUST_DRIVERS` has no built-in digest, so a connection
/// reaches it through an uploaded `rust` package naming its id.
pub fn builtin_provider_digest(driver_id: &str) -> Option<Digest> {
    BUILTIN_DIGESTS
        .iter()
        .find(|(driver, _)| *driver == driver_id)
        .map(|(_, digest)| digest.clone())
}

/// The built-in driver whose [`builtin_provider_digest`] is `digest`.
pub fn builtin_driver(digest: &str) -> Option<&'static str> {
    BUILTIN_DIGESTS
        .iter()
        .find(|(_, d)| d == digest)
        .map(|(driver, _)| *driver)
}

/// The synthetic [`ProviderRef`] of the built-in driver `digest` names:
/// [`BUILTIN_PUBLISHER`], the driver id, and this build's version.
pub fn builtin_provider_ref(digest: &Digest) -> Option<ProviderRef> {
    builtin_driver(digest).map(|driver| ProviderRef {
        publisher: BUILTIN_PUBLISHER.to_string(),
        provider_id: ProviderId::from(driver),
        version: crate::version::ORION_VERSION.to_string(),
        digest: digest.clone(),
    })
}

/// What the foundation providers are bounded by. Built from the node's
/// config at boot; [`Default`] is that config's defaults.
#[derive(Debug, Clone)]
pub struct FoundationOptions {
    /// The client every outbound HTTP call is made on. The node passes its
    /// shared one: SSRF-pinned DNS, so the address the guard vetted is the
    /// one connected to, redirects off, and the global timeout.
    pub http_client: reqwest::Client,
    /// Cap on one HTTP response body, read while streaming.
    pub http_max_response_bytes: usize,
    /// The row and byte caps a SQL query is held to.
    pub sql: SqlLimits,
    /// The directory file-backed SQLite connections resolve beneath.
    pub sqlite_root: PathBuf,
    /// Optional path to Orion's own file-backed SQLite state database so
    /// SQLite connections cannot open it or its WAL/SHM/journal sidecars.
    pub sqlite_state_db: Option<PathBuf>,
    /// Maximum concurrent session pools retained per remote SQL provider
    /// (`org.orion.postgresql`, `org.orion.mysql`).
    pub max_pool_sessions: usize,
    /// Maximum concurrent session pools retained by `org.orion.sqlite`.
    pub sqlite_max_sessions: usize,
    /// Maximum entries per connection partition in `org.orion.memory-cache`.
    pub memory_cache_max_entries_per_partition: usize,
    /// Maximum connection partitions in `org.orion.memory-cache`.
    pub memory_cache_max_partitions: usize,
    /// Maximum serialized byte size of a single value in `org.orion.memory-cache`.
    pub memory_cache_max_value_bytes: usize,
}

impl Default for FoundationOptions {
    fn default() -> Self {
        Self {
            http_client: default_http_client(),
            http_max_response_bytes: 10 * 1024 * 1024,
            sql: SqlLimits::default(),
            sqlite_root: PathBuf::from("./data/connectors/sqlite"),
            sqlite_state_db: None,
            max_pool_sessions: 16,
            sqlite_max_sessions: 8,
            memory_cache_max_entries_per_partition: 1024,
            memory_cache_max_partitions: 256,
            memory_cache_max_value_bytes: 1024 * 1024,
        }
    }
}

/// A client built the way the node builds its shared one, for a provider
/// constructed without the node's.
fn default_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(crate::version::USER_AGENT)
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(Arc::new(orion_common::ssrf::PinnedDnsResolver))
        .build()
        .unwrap_or_default()
}

/// The caps one SQL query is held to, as `db_read` is: `query.max_limit`
/// rows and `query.max_result_bytes` of decoded result (`0` = unlimited).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlLimits {
    pub max_rows: usize,
    pub max_result_bytes: usize,
}

impl Default for SqlLimits {
    fn default() -> Self {
        Self {
            max_rows: 1000,
            max_result_bytes: 16 * 1024 * 1024,
        }
    }
}

/// Refuse `input` naming a connection target. A connection's target, and
/// any credential it needs, come from its governed connection revision; a
/// target in the input would let a workflow, or the message it renders,
/// aim the provider anywhere.
pub(super) fn refuse_input_target(input: &Value, keys: &[&str]) -> Result<(), ConnectorError> {
    match keys.iter().find(|key| input.get(**key).is_some()) {
        Some(key) => Err(ConnectorError::forbidden(
            codes::CONNECTION_FORBIDDEN,
            "validation",
            format!("input.{key} is not accepted: the target comes from the connection revision"),
            EffectStatus::NotStarted,
        )),
        None => Ok(()),
    }
}

/// The connection revision `identity` names, or the active one for its id.
pub(super) fn governed_connection(
    store: Option<&super::deployment_store::ImmutableDeploymentStore>,
    identity: &EffectiveConnectionIdentity,
) -> Option<std::sync::Arc<super::deployment_store::ConnectionRevisionRow>> {
    let store = store?;
    store
        .get_connection(&identity.connection_id, identity.revision)
        .or_else(|| store.get_active_connection(&identity.connection_id))
}

/// The connection string a governed revision names: its
/// `connection_string` secret locator resolved, or `public_config`'s, which
/// holds no credential (the revision's own screen refused one).
pub(super) async fn governed_connection_string(
    store: Option<&super::deployment_store::ImmutableDeploymentStore>,
    identity: &EffectiveConnectionIdentity,
    driver: &str,
) -> Result<Option<String>, ConnectorError> {
    let Some(conn) = governed_connection(store, identity) else {
        return Ok(None);
    };
    match governed_secret(&conn, "connection_string", identity, driver).await? {
        Some(secret) => Ok(Some(secret)),
        None => Ok(conn
            .public_config
            .get("connection_string")
            .and_then(Value::as_str)
            .map(str::to_string)),
    }
}

/// What a governed revision's secret locator for `path` resolves to, or
/// `None` when it names none there. Resolved per use, never cached here.
pub(super) async fn governed_secret(
    conn: &super::deployment_store::ConnectionRevisionRow,
    path: &str,
    identity: &EffectiveConnectionIdentity,
    driver: &str,
) -> Result<Option<String>, ConnectorError> {
    let Some(reference) = conn.secret_locators.get(path) else {
        return Ok(None);
    };
    crate::connector::secrets::resolve_secret_string(reference, path)
        .await
        .map(Some)
        .map_err(|e| {
            ConnectorError::unavailable(
                format!(
                    "{driver}: connection '{}' secret '{path}' did not resolve: {e}",
                    identity.connection_id
                ),
                EffectStatus::NotStarted,
            )
        })
}

/// Register the five Community foundation Direct Rust providers and their
/// session/cache pools, bound to the node's deployment store and bounded by
/// its options. What [`super::CompositionBuilder::community`] composes.
pub fn register_community_foundation(r: &mut super::ProviderRegistrar<'_>) {
    let options = r.options().clone();
    let store = r.deployments().clone();
    let http =
        Arc::new(HttpProviderBackend::with_deployment_store(store.clone()).with_options(&options));
    let pg = Arc::new(
        PostgresProviderBackend::with_deployment_store(options.max_pool_sessions, store.clone())
            .with_limits(options.sql),
    );
    let mysql = Arc::new(
        MysqlProviderBackend::with_deployment_store(options.max_pool_sessions, store.clone())
            .with_limits(options.sql),
    );
    let sqlite = Arc::new(
        SqliteProviderBackend::with_deployment_store(
            options.sqlite_root.clone(),
            options.sqlite_max_sessions,
            store,
        )
        .with_state_db(options.sqlite_state_db.clone())
        .with_limits(options.sql),
    );
    let mem_cache = Arc::new(MemoryCacheProviderBackend::with_limits(
        options.memory_cache_max_entries_per_partition,
        options.memory_cache_max_partitions,
        options.memory_cache_max_value_bytes,
    ));

    r.pool(pg.clone());
    r.pool(mysql.clone());
    r.pool(sqlite.clone());
    r.pool(mem_cache.store());

    r.provider(http);
    r.provider(pg);
    r.provider(mysql);
    r.provider(sqlite);
    r.provider(mem_cache);
}

#[cfg(test)]
#[expect(clippy::unwrap_used)]
#[path = "../foundation_tests.rs"]
mod tests;
