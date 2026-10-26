//! Connection-pool caches a provider registers so the node can evict them.

use std::any::{Any, TypeId};
use std::sync::Arc;

use async_trait::async_trait;

/// A provider's connection-pool cache, evicted per connector when a
/// connection changes and wholesale on a cluster resync. Implement it with
/// `#[async_trait]`.
#[async_trait]
pub trait ConnectionPoolCache: Send + Sync {
    /// Stable cache identifier, unique on a node (e.g. `"org.orion.postgresql"`).
    fn cache_id(&self) -> &str;

    /// Evict all cached sessions/pools belonging to `connector_name`.
    async fn evict(&self, connector_name: &str);

    /// Evict every cached session/pool in this cache (used on cluster epoch resync).
    async fn evict_all(&self);
}

/// One pool cache as a composition registers it: erased for eviction, and
/// kept typed so the provider that registered it can look it up again as
/// its own type ([`Self::downcast`]).
#[derive(Clone)]
pub struct PoolEntry {
    cache_id: String,
    type_id: TypeId,
    typed: Arc<dyn Any + Send + Sync>,
    cache: Arc<dyn ConnectionPoolCache>,
}

impl std::fmt::Debug for PoolEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolEntry")
            .field("cache_id", &self.cache_id)
            .finish_non_exhaustive()
    }
}

impl PoolEntry {
    /// The entry for `cache`, keyed by its [`ConnectionPoolCache::cache_id`].
    pub fn of<C: ConnectionPoolCache + 'static>(cache: Arc<C>) -> Self {
        Self {
            cache_id: cache.cache_id().to_string(),
            type_id: TypeId::of::<C>(),
            typed: cache.clone(),
            cache,
        }
    }

    pub fn cache_id(&self) -> &str {
        &self.cache_id
    }

    /// The cache, erased.
    pub fn cache(&self) -> &Arc<dyn ConnectionPoolCache> {
        &self.cache
    }

    /// The cache as a `C`, when it is one.
    pub fn downcast<C: Send + Sync + 'static>(&self) -> Option<Arc<C>> {
        if self.type_id != TypeId::of::<C>() {
            return None;
        }
        Arc::downcast::<C>(self.typed.clone()).ok()
    }
}
