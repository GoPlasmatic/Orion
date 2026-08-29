pub mod cache_backend;
pub mod circuit_breaker;
mod config;
pub mod kind;
pub(crate) mod lru_cache;
mod masking;
pub mod mongo_pool;
pub mod oauth;
pub mod pool_cache;
pub mod redis_pool;
mod registry;
pub mod secrets;
pub mod sigv4;
pub mod smtp_pool;

pub(crate) use pool_access_counter::POOL_ACCESS_COUNTER;

pub use config::{
    AuthConfig, CacheConnectorConfig, CacheOperationGates, ConnectorConfig, ConnectorType,
    DbConnectorConfig, DialectGuards, EsConnectorConfig, HttpConnectorConfig, HttpOperationGates,
    KafkaConnectorConfig, KafkaOperationGates, OAuth2ClientAuth, OAuth2Config, OAuth2Grant,
    OperationGates, RetryConfig, SmtpAuth, SmtpConnectorConfig, SmtpTls, StorageConnectorConfig,
    StorageOperationGates, StorageProvider, VALID_CACHE_BACKENDS, VALID_CONNECTOR_TYPES,
    VALID_HTTP_METHODS, is_mongo_url,
};
pub use kind::{ConnectorKind, ConnectorTarget, DataBackend, PoolSlot};
pub(crate) use masking::MASK;
pub use masking::{
    find_masked_value, mask_channel_config, mask_connector, mask_secrets, redact_url_secrets,
    redact_url_secrets_or_raw, unmask_channel_config, unmask_config,
};
pub use registry::ConnectorRegistry;
/// A stub `ConnectorRepository` shared by the connector and channel registry
/// tests — the channel registry's reuse cache is keyed on the connector
/// token, so testing it takes a connector *load*.
#[cfg(test)]
pub(crate) use registry::test_support;

mod pool_access_counter {
    use std::sync::atomic::AtomicU64;

    /// Shared monotonic counter for LRU tracking across connector pool caches.
    pub(crate) static POOL_ACCESS_COUNTER: AtomicU64 = AtomicU64::new(0);
}
