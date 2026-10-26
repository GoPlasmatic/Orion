//! The cluster's default dedup store and response cache, over the shared
//! state database ([`SqlSharedState`]).
//!
//! A cluster needs these shared: a dedup claim made on one node must stop a
//! duplicate delivered to another, and a response cached on one node is the
//! same response every node would build. Each operation maps onto one of the
//! store's atomic primitives, so the guarantees the in-memory backend gives
//! one node hold across the fleet.

use async_trait::async_trait;

use crate::connector::cache_backend::CacheBackend;
use crate::errors::OrionError;
use crate::storage::repositories::shared_state::SqlSharedState;

/// [`CacheBackend`] over the cluster's shared state database.
pub struct SharedCacheBackend {
    store: SqlSharedState,
}

impl SharedCacheBackend {
    pub fn new(store: SqlSharedState) -> Self {
        Self { store }
    }
}

#[async_trait]
impl CacheBackend for SharedCacheBackend {
    async fn get(&self, key: &str) -> Result<Option<String>, OrionError> {
        self.store.get(key).await
    }

    async fn set(&self, key: &str, value: &str) -> Result<(), OrionError> {
        self.store.set(key, value, None).await
    }

    async fn set_ex(&self, key: &str, value: &str, ttl_secs: u64) -> Result<(), OrionError> {
        self.store.set(key, value, Some(ttl_secs)).await
    }

    async fn remove(&self, key: &str) -> Result<(), OrionError> {
        self.store.remove_many(&[key.to_string()]).await.map(|_| ())
    }

    async fn get_many(&self, keys: &[String]) -> Result<Vec<Option<String>>, OrionError> {
        self.store.get_many(keys).await
    }

    async fn remove_many(&self, keys: &[String]) -> Result<u64, OrionError> {
        self.store.remove_many(keys).await
    }

    async fn incr_by(&self, key: &str, by: i64, ttl_secs: Option<u64>) -> Result<i64, OrionError> {
        self.store.incr_by(key, by, ttl_secs).await
    }

    async fn claim_dedup_key(
        &self,
        key: &str,
        owner: &str,
        window_secs: u64,
    ) -> Result<Option<String>, OrionError> {
        self.store.claim(key, owner, window_secs).await
    }
}
