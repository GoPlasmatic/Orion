//! The cluster's default dedup store and response cache, over the cluster's
//! [`SharedStateStore`]: the state database's, or the one an edition
//! composed.
//!
//! A cluster needs these shared: a dedup claim made on one node must stop a
//! duplicate delivered to another, and a response cached on one node is the
//! same response every node would build. Each operation maps onto one of the
//! store's atomic primitives, so the guarantees the in-memory backend gives
//! one node hold across the fleet.

use std::sync::Arc;

use async_trait::async_trait;
use orion_extension_api::{SharedStateStore, StoreError, StoreErrorKind};

use crate::connector::cache_backend::CacheBackend;
use crate::errors::OrionError;

/// [`CacheBackend`] over the cluster's shared store.
pub struct SharedCacheBackend {
    store: Arc<dyn SharedStateStore>,
}

impl SharedCacheBackend {
    pub fn new(store: Arc<dyn SharedStateStore>) -> Self {
        Self { store }
    }

    fn err(&self, e: StoreError) -> OrionError {
        to_orion(self.store.name(), &e)
    }
}

/// A store failure as the node reports it. `Invalid` (incrementing text) is
/// the caller's validation error. Anything else is an internal error naming
/// the store, which the caller's `on_backend_error` policy resolves exactly
/// as it resolves a database failure.
pub fn to_orion(name: &str, e: &StoreError) -> OrionError {
    match e.kind() {
        StoreErrorKind::Invalid => OrionError::validation(e.message()),
        _ => OrionError::internal(format!("cluster store '{name}': {}", e.message())),
    }
}

#[async_trait]
impl CacheBackend for SharedCacheBackend {
    async fn get(&self, key: &str) -> Result<Option<String>, OrionError> {
        self.store.get(key).await.map_err(|e| self.err(e))
    }

    async fn set(&self, key: &str, value: &str) -> Result<(), OrionError> {
        self.store
            .set(key, value, None)
            .await
            .map_err(|e| self.err(e))
    }

    async fn set_ex(&self, key: &str, value: &str, ttl_secs: u64) -> Result<(), OrionError> {
        self.store
            .set(key, value, Some(ttl_secs))
            .await
            .map_err(|e| self.err(e))
    }

    async fn remove(&self, key: &str) -> Result<(), OrionError> {
        self.store
            .remove_many(&[key.to_string()])
            .await
            .map(|_| ())
            .map_err(|e| self.err(e))
    }

    async fn get_many(&self, keys: &[String]) -> Result<Vec<Option<String>>, OrionError> {
        self.store.get_many(keys).await.map_err(|e| self.err(e))
    }

    async fn remove_many(&self, keys: &[String]) -> Result<u64, OrionError> {
        self.store.remove_many(keys).await.map_err(|e| self.err(e))
    }

    async fn incr_by(&self, key: &str, by: i64, ttl_secs: Option<u64>) -> Result<i64, OrionError> {
        self.store
            .incr_by(key, by, ttl_secs)
            .await
            .map_err(|e| self.err(e))
    }

    async fn claim_dedup_key(
        &self,
        key: &str,
        owner: &str,
        window_secs: u64,
    ) -> Result<Option<String>, OrionError> {
        self.store
            .claim(key, owner, window_secs)
            .await
            .map_err(|e| self.err(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_invalid_store_error_is_a_validation_error_and_the_rest_name_the_store() {
        assert!(matches!(
            to_orion("redis", &StoreError::invalid("not an integer")),
            OrionError::Validation { ref message, .. } if message == "not an integer"
        ));
        let unavailable = to_orion("redis", &StoreError::unavailable("connection refused"));
        assert!(matches!(unavailable, OrionError::Internal { .. }));
        assert_eq!(
            unavailable.to_string(),
            "cluster store 'redis': connection refused"
        );
    }
}
