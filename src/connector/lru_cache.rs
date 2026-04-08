//! Generic bounded LRU cache for connection pools / clients.
//!
//! Provides a shared, async-safe cache keyed by connector name.  Entries are
//! lazily created via caller-supplied futures and evicted LRU when the cache
//! reaches capacity.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::RwLock;

use super::POOL_ACCESS_COUNTER;

/// A cached entry with LRU tracking.
struct CacheEntry<V> {
    value: V,
    last_access: AtomicU64,
}

impl<V> CacheEntry<V> {
    fn new(value: V) -> Self {
        Self {
            value,
            last_access: AtomicU64::new(POOL_ACCESS_COUNTER.fetch_add(1, Ordering::Relaxed)),
        }
    }

    fn touch(&self) {
        self.last_access.store(
            POOL_ACCESS_COUNTER.fetch_add(1, Ordering::Relaxed),
            Ordering::Relaxed,
        );
    }
}

/// A bounded, async-safe LRU cache.
///
/// `V` must be `Clone` so callers can obtain an owned handle (connection /
/// pool) without holding the lock.
pub struct LruCache<V: Clone> {
    entries: RwLock<HashMap<String, CacheEntry<V>>>,
    max_entries: usize,
    cache_label: &'static str,
}

impl<V: Clone> LruCache<V> {
    pub fn new(max_entries: usize, cache_label: &'static str) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            max_entries,
            cache_label,
        }
    }

    /// Get an existing entry or create one via `create_fn`.
    ///
    /// Uses a read-lock fast path; falls back to a write lock on miss.
    /// The `create_fn` future runs **outside** the write lock to avoid
    /// blocking other connectors during connection setup.
    pub async fn get_or_create<F, Fut, E>(
        &self,
        key: &str,
        create_fn: F,
    ) -> Result<V, E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<V, E>>,
    {
        // Fast path: read lock
        {
            let entries = self.entries.read().await;
            if let Some(entry) = entries.get(key) {
                entry.touch();
                return Ok(entry.value.clone());
            }
        }

        // Create outside the write lock
        let value = create_fn().await?;

        // Insert under write lock; if another task raced, use theirs
        let mut entries = self.entries.write().await;
        if let Some(existing) = entries.get(key) {
            existing.touch();
            return Ok(existing.value.clone());
        }

        // LRU eviction when at capacity
        if entries.len() >= self.max_entries {
            if let Some(lru_key) = entries
                .iter()
                .min_by_key(|(_, e)| e.last_access.load(Ordering::Relaxed))
                .map(|(k, _)| k.clone())
            {
                tracing::info!(
                    evicted = %lru_key,
                    cache = self.cache_label,
                    "Pool cache at capacity, evicting least-recently-used entry"
                );
                entries.remove(&lru_key);
            }
        }

        entries.insert(key.to_string(), CacheEntry::new(value.clone()));
        Ok(value)
    }

    /// Evict a cached entry (e.g., when connector config changes).
    pub async fn evict(&self, key: &str) {
        self.entries.write().await.remove(key);
    }
}
