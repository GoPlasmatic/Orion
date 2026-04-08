pub mod cache_backend;
pub mod circuit_breaker;
mod masking;
#[cfg(feature = "connectors-mongodb")]
pub mod mongo_pool;
#[cfg(feature = "connectors-sql")]
pub mod pool_cache;
#[cfg(feature = "connectors-redis")]
pub mod redis_pool;
mod registry;
mod types;

#[cfg(any(
    feature = "connectors-sql",
    feature = "connectors-redis",
    feature = "connectors-mongodb"
))]
pub(crate) use pool_access_counter::POOL_ACCESS_COUNTER;

pub use masking::{mask_connector, mask_connector_secrets};
pub use registry::ConnectorRegistry;
pub use types::{
    AuthConfig, CacheConnectorConfig, ConnectorConfig, DbConnectorConfig, HttpConnectorConfig,
    KafkaConnectorConfig, RetryConfig, StorageConnectorConfig, VALID_CACHE_BACKENDS,
    VALID_CONNECTOR_TYPES,
};

#[cfg(any(
    feature = "connectors-sql",
    feature = "connectors-redis",
    feature = "connectors-mongodb"
))]
mod pool_access_counter {
    use std::sync::atomic::AtomicU64;

    /// Shared monotonic counter for LRU tracking across connector pool caches.
    pub(crate) static POOL_ACCESS_COUNTER: AtomicU64 = AtomicU64::new(0);
}
