//! Generic bounded LRU cache for connection pools / clients.
//!
//! Provides a shared, async-safe cache keyed by connector name.  Entries are
//! lazily created via caller-supplied futures and evicted LRU when the cache
//! reaches capacity.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use super::POOL_ACCESS_COUNTER;

#[derive(Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    name: String,
    config: Option<[u8; 32]>,
}

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
    entries: RwLock<HashMap<CacheKey, CacheEntry<V>>>,
    invalidated: AtomicU64,
    max_entries: usize,
    cache_label: &'static str,
    /// F17: called with every removed value — explicit evict, evict_all, and
    /// capacity eviction — so pool-like values can be closed instead of
    /// leaking their TCP connections until the last Arc clone drops
    /// (under load: indefinitely). Handlers must not block; spawn.
    on_evict: Option<std::sync::Arc<dyn Fn(V) + Send + Sync>>,
}

impl<V: Clone> LruCache<V> {
    pub fn new(max_entries: usize, cache_label: &'static str) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            invalidated: AtomicU64::new(0),
            max_entries,
            cache_label,
            on_evict: None,
        }
    }

    /// A cache whose removed values are handed to `handler` (F17).
    pub fn with_evict_handler(
        max_entries: usize,
        cache_label: &'static str,
        handler: impl Fn(V) + Send + Sync + 'static,
    ) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            invalidated: AtomicU64::new(0),
            max_entries,
            cache_label,
            on_evict: Some(std::sync::Arc::new(handler)),
        }
    }

    fn dispose(&self, value: V) {
        if let Some(handler) = &self.on_evict {
            handler(value);
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
        self.get_or_create_key(
            CacheKey {
                name: key.to_string(),
                config: None,
            },
            create_fn,
        )
        .await
    }

    /// Connection settings are part of identity: an old caller may finish
    /// opening a connection after an edit, but cannot supply a new caller's
    /// pool. Only the connector name, never this digest or its input, is logged.
    pub async fn get_or_create_versioned<F, Fut, C>(
        &self,
        name: &str,
        config: &C,
        create_fn: F,
    ) -> Result<V, crate::errors::OrionError>
    where
        C: serde::Serialize,
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<V, crate::errors::OrionError>>,
    {
        let bytes = serde_json::to_vec(config).map_err(|e| {
            crate::errors::OrionError::internal_from("Cannot identify connection settings", e)
        })?;
        self.get_or_create_key(
            CacheKey {
                name: name.to_string(),
                config: Some(Sha256::digest(bytes).into()),
            },
            create_fn,
        )
        .await
    }

    async fn get_or_create_key<F, Fut, E>(&self, key: CacheKey, create_fn: F) -> Result<V, E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<V, E>>,
    {
        // Fast path: read lock
        let generation = {
            let entries = self.entries.read().await;
            if let Some(entry) = entries.get(&key) {
                entry.touch();
                return Ok(entry.value.clone());
            }
            self.invalidated.load(Ordering::Relaxed)
        };

        // Create outside the write lock
        let value = create_fn().await?;

        // Insert under write lock; if another task raced, use theirs
        let mut entries = self.entries.write().await;
        // Eviction also invalidates creations that have not inserted yet.
        // The caller can finish using its value, but it must not refill the
        // cache after a credential rotation or explicit invalidation.
        if self.invalidated.load(Ordering::Relaxed) != generation {
            return Ok(value);
        }
        if let Some(existing) = entries.get(&key) {
            existing.touch();
            let winner = existing.value.clone();
            // The race-losing value never enters the cache — hand it to the
            // evict handler (F17) like any other removed value, or a fully
            // connected pool/client silently leaks its connections.
            self.dispose(value);
            return Ok(winner);
        }

        // LRU eviction when at capacity
        if entries.len() >= self.max_entries
            && let Some(lru_key) = entries
                .iter()
                .min_by_key(|(_, e)| e.last_access.load(Ordering::Relaxed))
                .map(|(k, _)| k.clone())
        {
            tracing::info!(
                evicted = %lru_key.name,
                cache = self.cache_label,
                "Pool cache at capacity, evicting least-recently-used entry"
            );
            if let Some(entry) = entries.remove(&lru_key) {
                self.dispose(entry.value);
            }
        }

        entries.insert(key, CacheEntry::new(value.clone()));
        Ok(value)
    }

    /// Evict a cached entry (e.g., when connector config changes). The
    /// removed value is handed to the evict handler (F17).
    pub async fn evict(&self, key: &str) {
        let removed = {
            let mut entries = self.entries.write().await;
            self.invalidated.fetch_add(1, Ordering::Relaxed);
            let keys: Vec<_> = entries.keys().filter(|k| k.name == key).cloned().collect();
            keys.into_iter()
                .filter_map(|k| entries.remove(&k))
                .collect::<Vec<_>>()
        };
        for entry in removed {
            self.dispose(entry.value);
        }
    }

    /// Evict every cached entry. Used by epoch-driven resyncs: a remote node
    /// cannot know which connector changed, and pools rebuild lazily. Every
    /// removed value is handed to the evict handler (F17) — before this,
    /// each resync leaked the old pools' connections.
    pub async fn evict_all(&self) {
        let drained: Vec<V> = {
            let mut entries = self.entries.write().await;
            self.invalidated.fetch_add(1, Ordering::Relaxed);
            entries.drain().map(|(_, e)| e.value).collect()
        };
        for value in drained {
            self.dispose(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    #[tokio::test]
    async fn an_eviction_invalidates_a_creation_that_has_not_finished() {
        for all in [false, true] {
            let cache = Arc::new(LruCache::new(4, "test"));
            let (started, ready) = tokio::sync::oneshot::channel();
            let (finish, resume) = tokio::sync::oneshot::channel();
            let old_cache = cache.clone();
            let old = tokio::spawn(async move {
                old_cache
                    .get_or_create("db", || async move {
                        started.send(()).expect("reader waits");
                        resume.await.expect("creator released");
                        Ok::<_, ()>(1)
                    })
                    .await
                    .expect("old caller finishes")
            });
            ready.await.expect("creation started");
            if all {
                cache.evict_all().await;
            } else {
                cache.evict("db").await;
            }
            finish.send(()).expect("creator waits");
            assert_eq!(old.await.expect("joined"), 1);
            assert_eq!(
                cache.get_or_create("db", || async { Ok::<_, ()>(2) }).await,
                Ok(2)
            );
        }
    }

    #[tokio::test]
    async fn connection_revisions_do_not_supply_each_others_values() {
        let cache = Arc::new(LruCache::new(4, "test"));
        let (started, ready) = tokio::sync::oneshot::channel();
        let (finish, resume) = tokio::sync::oneshot::channel();
        let old_cache = cache.clone();
        let old = tokio::spawn(async move {
            old_cache
                .get_or_create_versioned("db", &"old-secret", || async move {
                    started.send(()).expect("reader waits");
                    resume.await.expect("creator released");
                    Ok(1)
                })
                .await
                .expect("old connection")
        });
        ready.await.expect("creation started");
        assert_eq!(
            cache
                .get_or_create_versioned("db", &"new-secret", || async { Ok(2) })
                .await
                .expect("new connection"),
            2
        );
        finish.send(()).expect("creator waits");
        assert_eq!(old.await.expect("joined"), 1);
        assert_eq!(
            cache
                .get_or_create_versioned("db", &"new-secret", || async { Ok(3) })
                .await
                .expect("new cache hit"),
            2
        );
        cache.evict("db").await;
        assert_eq!(
            cache
                .get_or_create_versioned("db", &"old-secret", || async { Ok(4) })
                .await
                .expect("evicted old revision"),
            4
        );
        assert_eq!(
            cache
                .get_or_create_versioned("db", &"new-secret", || async { Ok(5) })
                .await
                .expect("evicted new revision"),
            5
        );
    }

    /// F17: every removal path must hand the value to the evict handler.
    #[tokio::test]
    async fn evict_handler_fires_on_every_removal_path() {
        let closed = Arc::new(AtomicUsize::new(0));
        let closed_ref = closed.clone();
        let cache: LruCache<u32> = LruCache::with_evict_handler(2, "test", move |_v| {
            closed_ref.fetch_add(1, AtomicOrdering::SeqCst);
        });

        let create = |v: u32| async move { Ok::<u32, ()>(v) };
        cache.get_or_create("a", || create(1)).await.expect("test");
        cache.get_or_create("b", || create(2)).await.expect("test");

        // Explicit evict.
        cache.evict("a").await;
        assert_eq!(closed.load(AtomicOrdering::SeqCst), 1);

        // Capacity eviction: cache is at max 2 after c + d insert once.
        cache.get_or_create("c", || create(3)).await.expect("test");
        cache.get_or_create("d", || create(4)).await.expect("test");
        assert_eq!(
            closed.load(AtomicOrdering::SeqCst),
            2,
            "LRU eviction must dispose"
        );

        // evict_all drains the remaining two.
        cache.evict_all().await;
        assert_eq!(closed.load(AtomicOrdering::SeqCst), 4);
    }

    /// F17, insert-race branch: the loser's freshly created value never
    /// enters the cache, so it too must reach the evict handler — it is a
    /// fully connected pool.
    #[tokio::test]
    async fn evict_handler_fires_for_race_losing_value() {
        let closed = Arc::new(AtomicUsize::new(0));
        let closed_ref = closed.clone();
        let cache: Arc<LruCache<u32>> =
            Arc::new(LruCache::with_evict_handler(4, "test", move |_v| {
                closed_ref.fetch_add(1, AtomicOrdering::SeqCst);
            }));
        let barrier = Arc::new(tokio::sync::Barrier::new(2));

        let mut handles = Vec::new();
        for v in [1u32, 2u32] {
            let cache = cache.clone();
            let barrier = barrier.clone();
            handles.push(tokio::spawn(async move {
                let val: Result<u32, ()> = cache
                    .get_or_create("key", || async move {
                        barrier.wait().await;
                        Ok(v)
                    })
                    .await;
                val.expect("test")
            }));
        }
        let (a, b) = (
            handles.remove(0).await.expect("test"),
            handles.remove(0).await.expect("test"),
        );

        assert_eq!(a, b, "both callers must see the winner's value");
        assert_eq!(
            closed.load(AtomicOrdering::SeqCst),
            1,
            "the race loser's value must be disposed, not dropped"
        );
    }

    #[tokio::test]
    async fn test_cache_miss_creates() {
        let cache = LruCache::new(4, "test");
        let val: Result<String, String> = cache
            .get_or_create("key1", || async { Ok("value1".to_string()) })
            .await;
        assert_eq!(val.expect("test"), "value1");
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
        assert_eq!(val.expect("test"), "value1");
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
        assert_eq!(val.expect("test"), "A2");
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
        assert_eq!(val.expect("test"), "A");
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
        assert_eq!(val.expect("test"), "value2");
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
            val.expect("test")
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
            val.expect("test")
        });

        let (v1, v2) = tokio::join!(h1, h2);
        let v1 = v1.expect("test");
        let v2 = v2.expect("test");

        // Both tasks should return the same value (whichever won the race)
        assert_eq!(v1, v2, "both tasks should see the same cached value");
    }
}
