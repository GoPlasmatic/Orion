pub mod cache_backend;
pub mod circuit_breaker;
mod config;
mod lru_cache;
mod masking;
pub mod mongo_pool;
pub mod pool_cache;
pub mod redis_pool;
mod registry;
pub mod secrets;

pub(crate) use pool_access_counter::POOL_ACCESS_COUNTER;

pub use config::{
    AuthConfig, CacheConnectorConfig, ConnectorConfig, ConnectorType, DbConnectorConfig,
    DialectGuards, EsConnectorConfig, HttpConnectorConfig, KafkaConnectorConfig, OperationGates,
    RetryConfig, VALID_CACHE_BACKENDS, VALID_CONNECTOR_TYPES, is_mongo_url,
};
pub(crate) use masking::MASK;
pub use masking::{
    find_masked_value, mask_connector, mask_secrets, redact_url_secrets, unmask_config,
};
pub use registry::ConnectorRegistry;

mod pool_access_counter {
    use std::sync::atomic::AtomicU64;

    /// Shared monotonic counter for LRU tracking across connector pool caches.
    pub(crate) static POOL_ACCESS_COUNTER: AtomicU64 = AtomicU64::new(0);
}
