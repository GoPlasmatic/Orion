pub mod cache_backend;
pub mod cache_namespace;
pub mod circuit_breaker;
mod config;
pub mod endpoints;
pub mod kind;
pub mod lru_cache;
mod masking;
pub mod oauth;
pub mod pool_cache;
mod registry;
pub mod secrets;
pub mod sql_decode;
pub mod sql_encode;

pub use config::{
    AuthConfig, CacheConnectorConfig, CacheOperationGates, ConnectorConfig, ConnectorType,
    DbConnectorConfig, DbDriver, DialectGuards, HttpConnectorConfig, HttpOperationGates,
    OAuth2ClientAuth, OAuth2Config, OAuth2Grant, OperationGates, RetryConfig, VALID_CACHE_BACKENDS,
    VALID_CONNECTOR_TYPES, VALID_HTTP_METHODS, VariantParse,
};
pub use kind::{ConnectorKind, ConnectorTarget, DataBackend, PoolSlot};
pub use masking::MASK;
pub use masking::{
    find_masked_value, mask_channel_config, mask_secrets, redact_url_secrets,
    redact_url_secrets_or_raw,
};
/// Stub connector rows shared by the connector and channel loader tests —
/// the channel loader's reuse cache is keyed on the connector token, so
/// testing it takes a connector *load*.
#[cfg(any(test, feature = "test-support"))]
pub use registry::test_support;
pub use registry::{
    ConnectorRegistry, ConnectorSnapshot, PINNED_CONNECTORS, ResolveIssue, StoredConnector,
    resolve_connector_config,
};
