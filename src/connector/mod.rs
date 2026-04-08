pub mod cache_backend;
pub mod circuit_breaker;
mod lru_cache;
mod masking;
pub mod mongo_pool;
pub mod pool_cache;
pub mod redis_pool;
mod registry;
mod types;

pub(crate) use pool_access_counter::POOL_ACCESS_COUNTER;

pub use masking::{mask_connector, mask_connector_secrets};
pub use registry::ConnectorRegistry;
pub use types::{
    AuthConfig, CacheConnectorConfig, ConnectorConfig, DbConnectorConfig, HttpConnectorConfig,
    KafkaConnectorConfig, RetryConfig, StorageConnectorConfig, VALID_CACHE_BACKENDS,
    VALID_CONNECTOR_TYPES,
};

mod pool_access_counter {
    use std::sync::atomic::AtomicU64;

    /// Shared monotonic counter for LRU tracking across connector pool caches.
    pub(crate) static POOL_ACCESS_COUNTER: AtomicU64 = AtomicU64::new(0);
}
