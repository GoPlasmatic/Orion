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
    pub async fn get_or_create<F, Fut, E>(&self, key: &str, create_fn: F) -> Result<V, E>
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
        if entries.len() >= self.max_entries
            && let Some(lru_key) = entries
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

        entries.insert(key.to_string(), CacheEntry::new(value.clone()));
        Ok(value)
    }

    /// Evict a cached entry (e.g., when connector config changes).
    pub async fn evict(&self, key: &str) {
        self.entries.write().await.remove(key);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    #[tokio::test]
    async fn test_cache_miss_creates() {
        let cache = LruCache::new(4, "test");
        let val: Result<String, String> = cache
            .get_or_create("key1", || async { Ok("value1".to_string()) })
            .await;
        assert_eq!(val.unwrap(), "value1");
    }

    #[tokio::test]
    async fn test_cache_hit_returns_cached() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let cache = LruCache::new(4, "test");

        let cc = call_count.clone();
        let _: Result<String, String> = cache
            .get_or_create("key1", || {
                let cc = cc.clone();
                async move {
                    cc.fetch_add(1, AtomicOrdering::Relaxed);
                    Ok("value1".to_string())
                }
            })
            .await;

        let cc = call_count.clone();
        let val: Result<String, String> = cache
            .get_or_create("key1", || {
                let cc = cc.clone();
                async move {
                    cc.fetch_add(1, AtomicOrdering::Relaxed);
                    Ok("value2".to_string())
                }
            })
            .await;

        // Should return cached value, create_fn called only once
        assert_eq!(val.unwrap(), "value1");
        assert_eq!(call_count.load(AtomicOrdering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_lru_eviction_at_capacity() {
        let cache = LruCache::new(2, "test");

        let _: Result<String, String> = cache
            .get_or_create("a", || async { Ok("A".to_string()) })
            .await;
        let _: Result<String, String> = cache
            .get_or_create("b", || async { Ok("B".to_string()) })
            .await;
        // Cache is full (a, b). Inserting c should evict a (least recently used).
        let _: Result<String, String> = cache
            .get_or_create("c", || async { Ok("C".to_string()) })
            .await;

        // "a" should have been evicted — create_fn is called again
        let call_count = Arc::new(AtomicUsize::new(0));
        let cc = call_count.clone();
        let val: Result<String, String> = cache
            .get_or_create("a", || {
                let cc = cc.clone();
                async move {
                    cc.fetch_add(1, AtomicOrdering::Relaxed);
                    Ok("A2".to_string())
                }
            })
            .await;
        assert_eq!(val.unwrap(), "A2");
        assert_eq!(call_count.load(AtomicOrdering::Relaxed), 1);

        // "b" or "c" should still be cached
        let cc2 = Arc::new(AtomicUsize::new(0));
        let cc2_ref = cc2.clone();
        let _: Result<String, String> = cache
            .get_or_create("c", || {
                let cc2_ref = cc2_ref.clone();
                async move {
                    cc2_ref.fetch_add(1, AtomicOrdering::Relaxed);
                    Ok("C2".to_string())
                }
            })
            .await;
        assert_eq!(
            cc2.load(AtomicOrdering::Relaxed),
            0,
            "c should still be cached"
        );
    }

    #[tokio::test]
    async fn test_touch_updates_lru_order() {
        let cache = LruCache::new(2, "test");

        let _: Result<String, String> = cache
            .get_or_create("a", || async { Ok("A".to_string()) })
            .await;
        let _: Result<String, String> = cache
            .get_or_create("b", || async { Ok("B".to_string()) })
            .await;

        // Touch "a" so "b" becomes the LRU entry
        let _: Result<String, String> = cache
            .get_or_create("a", || async { Ok("should not be called".to_string()) })
            .await;

        // Insert "c" — should evict "b" (the LRU), not "a"
        let _: Result<String, String> = cache
            .get_or_create("c", || async { Ok("C".to_string()) })
            .await;

        // "a" should still be cached
        let cc = Arc::new(AtomicUsize::new(0));
        let cc_ref = cc.clone();
        let val: Result<String, String> = cache
            .get_or_create("a", || {
                let cc_ref = cc_ref.clone();
                async move {
                    cc_ref.fetch_add(1, AtomicOrdering::Relaxed);
                    Ok("A2".to_string())
                }
            })
            .await;
        assert_eq!(val.unwrap(), "A");
        assert_eq!(
            cc.load(AtomicOrdering::Relaxed),
            0,
            "a should still be cached"
        );
    }

    #[tokio::test]
    async fn test_evict_removes_entry() {
        let cache = LruCache::new(4, "test");

        let _: Result<String, String> = cache
            .get_or_create("key1", || async { Ok("value1".to_string()) })
            .await;

        cache.evict("key1").await;

        // After eviction, create_fn should be called again
        let call_count = Arc::new(AtomicUsize::new(0));
        let cc = call_count.clone();
        let val: Result<String, String> = cache
            .get_or_create("key1", || {
                let cc = cc.clone();
                async move {
                    cc.fetch_add(1, AtomicOrdering::Relaxed);
                    Ok("value2".to_string())
                }
            })
            .await;
        assert_eq!(val.unwrap(), "value2");
        assert_eq!(call_count.load(AtomicOrdering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_race_uses_existing() {
        // Simulate a race: two callers try to create the same key.
        // The second caller should find the first's value already inserted.
        let cache = Arc::new(LruCache::new(4, "test"));
        let barrier = Arc::new(tokio::sync::Barrier::new(2));

        let cache1 = cache.clone();
        let barrier1 = barrier.clone();
        let h1 = tokio::spawn(async move {
            let val: Result<String, String> = cache1
                .get_or_create("key1", || {
                    let barrier1 = barrier1.clone();
                    async move {
                        barrier1.wait().await;
                        Ok("from_task_1".to_string())
                    }
                })
                .await;
            val.unwrap()
        });

        let cache2 = cache.clone();
        let barrier2 = barrier.clone();
        let h2 = tokio::spawn(async move {
            let val: Result<String, String> = cache2
                .get_or_create("key1", || {
                    let barrier2 = barrier2.clone();
                    async move {
                        barrier2.wait().await;
                        Ok("from_task_2".to_string())
                    }
                })
                .await;
            val.unwrap()
        });

        let (v1, v2) = tokio::join!(h1, h2);
        let v1 = v1.unwrap();
        let v2 = v2.unwrap();

        // Both tasks should return the same value (whichever won the race)
        assert_eq!(v1, v2, "both tasks should see the same cached value");
    }
}
