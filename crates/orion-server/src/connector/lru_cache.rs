//! Generic bounded LRU cache for connection pools / clients — the one LRU
//! the connection layer uses: the connectors' SQL pools
//! ([`super::pool_cache::SqlPoolCache`]) and the v2 foundation providers'
//! live pools both sit on it.
//!
//! Provides a shared, async-safe cache keyed by connector name.  Entries are
//! lazily created via caller-supplied futures and evicted LRU when the cache
//! reaches capacity.
//!
//! The entries are a [`hashlink::LinkedHashMap`] in recency order, so a hit
//! moves its entry to the back and eviction pops the front: O(1) both ways.
//! It replaced a map of access stamps whose eviction scanned every entry for
//! the smallest. Every critical section is synchronous — no `.await` while
//! the map is locked — so a plain mutex serves.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use hashlink::LinkedHashMap;
use sha2::{Digest, Sha256};

trait CacheKeyLookup {
    fn name(&self) -> &str;
    fn config(&self) -> Option<&[u8; 32]>;
}

#[derive(Clone, Eq)]
struct CacheKey {
    name: String,
    config: Option<[u8; 32]>,
}

#[derive(Clone, Copy)]
struct CacheKeyRef<'a> {
    name: &'a str,
    config: Option<[u8; 32]>,
}

impl CacheKeyRef<'_> {
    fn into_owned(self) -> CacheKey {
        CacheKey {
            name: self.name.to_string(),
            config: self.config,
        }
    }
}

impl CacheKeyLookup for CacheKey {
    fn name(&self) -> &str {
        &self.name
    }

    fn config(&self) -> Option<&[u8; 32]> {
        self.config.as_ref()
    }
}

impl CacheKeyLookup for CacheKeyRef<'_> {
    fn name(&self) -> &str {
        self.name
    }

    fn config(&self) -> Option<&[u8; 32]> {
        self.config.as_ref()
    }
}

impl PartialEq for CacheKey {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.config == other.config
    }
}

impl Hash for CacheKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.as_str().hash(state);
        self.config.as_ref().hash(state);
    }
}

impl PartialEq for dyn CacheKeyLookup + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.name() == other.name() && self.config() == other.config()
    }
}

impl Eq for dyn CacheKeyLookup + '_ {}

impl Hash for dyn CacheKeyLookup + '_ {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name().hash(state);
        self.config().hash(state);
    }
}

impl<'a> Borrow<dyn CacheKeyLookup + 'a> for CacheKey {
    fn borrow(&self) -> &(dyn CacheKeyLookup + 'a) {
        self
    }
}

/// Zero-allocation [`std::io::Write`] adapter that feeds serialized JSON bytes
/// directly into a [`Sha256`] digest without materializing an intermediate `Vec<u8>`.
struct Sha256Writer(Sha256);

impl std::io::Write for Sha256Writer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A bounded, async-safe LRU cache.
///
/// `V` must be `Clone` so callers can obtain an owned handle (connection /
/// pool) without holding the lock.
pub struct LruCache<V: Clone> {
    /// Least recently used at the front.
    entries: std::sync::Mutex<LinkedHashMap<CacheKey, V>>,
    flights: std::sync::Mutex<HashMap<CacheKey, std::sync::Arc<tokio::sync::Mutex<()>>>>,
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
    /// A cache with no evict handler. Test-only: every production cache holds
    /// pools, which must be closed when they leave ([`Self::with_evict_handler`]).
    #[cfg(test)]
    pub fn new(max_entries: usize, cache_label: &'static str) -> Self {
        Self {
            entries: std::sync::Mutex::new(LinkedHashMap::new()),
            flights: std::sync::Mutex::new(HashMap::new()),
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
            entries: std::sync::Mutex::new(LinkedHashMap::new()),
            flights: std::sync::Mutex::new(HashMap::new()),
            invalidated: AtomicU64::new(0),
            max_entries,
            cache_label,
            on_evict: Some(std::sync::Arc::new(handler)),
        }
    }

    /// The entry map. A panic while it was held leaves nothing half-written
    /// that matters — every section is one map operation — so a poisoned lock
    /// is taken over rather than propagated.
    fn entries(&self) -> std::sync::MutexGuard<'_, LinkedHashMap<CacheKey, V>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `key`'s value, marked most recently used.
    fn hit(entries: &mut LinkedHashMap<CacheKey, V>, key: &CacheKeyRef<'_>) -> Option<V> {
        entries
            .to_back(key as &dyn CacheKeyLookup)
            .map(|v| v.clone())
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
            CacheKeyRef {
                name: key,
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
        let mut hasher = Sha256Writer(Sha256::new());
        serde_json::to_writer(&mut hasher, config).map_err(|e| {
            crate::errors::OrionError::internal_from("Cannot identify connection settings", e)
        })?;
        self.get_or_create_key(
            CacheKeyRef {
                name,
                config: Some(hasher.0.finalize().into()),
            },
            create_fn,
        )
        .await
    }

    async fn get_or_create_key<F, Fut, E>(&self, key: CacheKeyRef<'_>, create_fn: F) -> Result<V, E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<V, E>>,
    {
        // Fast path: a zero-allocation borrowed lookup.
        if let Some(value) = Self::hit(&mut self.entries(), &key) {
            return Ok(value);
        }

        // TD-08: Per-effective-key single-flight creation with bounded bookkeeping.
        // Concurrent callers for the same `(name, config_digest)` coalesce behind
        // one flight mutex while unrelated connectors proceed independently.
        let owned_key = key.into_owned();
        let flight = {
            let mut flights = self.flights.lock().expect("flights lock");
            flights
                .entry(owned_key.clone())
                .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        struct FlightCleanup<'a, V: Clone> {
            cache: &'a LruCache<V>,
            key: CacheKey,
            flight: std::sync::Arc<tokio::sync::Mutex<()>>,
        }
        impl<V: Clone> Drop for FlightCleanup<'_, V> {
            fn drop(&mut self) {
                if let Ok(mut flights) = self.cache.flights.lock()
                    && let Some(cur) = flights.get(&self.key)
                    && std::sync::Arc::ptr_eq(cur, &self.flight)
                    && std::sync::Arc::strong_count(&self.flight) <= 2
                {
                    flights.remove(&self.key);
                }
            }
        }
        let _cleanup = FlightCleanup {
            cache: self,
            key: owned_key.clone(),
            flight,
        };
        let _flight_guard = _cleanup.flight.lock().await;

        // Re-check in case an earlier holder of this flight already created
        // and inserted the entry.
        let generation = {
            let mut entries = self.entries();
            if let Some(value) = Self::hit(&mut entries, &key) {
                return Ok(value);
            }
            self.invalidated.load(Ordering::Acquire)
        };

        // Create outside the lock
        let value = create_fn().await?;

        // Insert under the lock; if another task raced across an eviction
        // boundary, use theirs and dispose ours.
        let mut entries = self.entries();
        // Eviction also invalidates creations that have not inserted yet.
        // The caller can finish using its value, but it must not refill the
        // cache after a credential rotation or explicit invalidation.
        if self.invalidated.load(Ordering::Acquire) != generation {
            return Ok(value);
        }
        if let Some(winner) = Self::hit(&mut entries, &key) {
            // The race-losing value never enters the cache — hand it to the
            // evict handler (F17) like any other removed value, or a fully
            // connected pool/client silently leaks its connections.
            self.dispose(value);
            return Ok(winner);
        }

        // LRU eviction when at capacity: the front is the least recently used.
        if entries.len() >= self.max_entries
            && let Some((lru_key, lru_value)) = entries.pop_front()
        {
            tracing::info!(
                evicted = %lru_key.name,
                cache = self.cache_label,
                "Pool cache at capacity, evicting least-recently-used entry"
            );
            self.dispose(lru_value);
        }

        entries.insert(owned_key, value.clone());
        Ok(value)
    }

    /// Evict a cached entry (e.g., when connector config changes). The
    /// removed value is handed to the evict handler (F17).
    pub async fn evict(&self, key: &str) {
        let removed = {
            let mut entries = self.entries();
            self.invalidated.fetch_add(1, Ordering::Release);
            if let Ok(mut flights) = self.flights.lock() {
                flights.retain(|k, _| k.name != key);
            }
            let keys: Vec<_> = entries.keys().filter(|k| k.name == key).cloned().collect();
            keys.into_iter()
                .filter_map(|k| entries.remove(&k))
                .collect::<Vec<_>>()
        };
        for value in removed {
            self.dispose(value);
        }
    }

    /// Evict every cached entry. Used by epoch-driven resyncs: a remote node
    /// cannot know which connector changed, and pools rebuild lazily. Every
    /// removed value is handed to the evict handler (F17) — before this,
    /// each resync leaked the old pools' connections.
    pub async fn evict_all(&self) {
        let drained: Vec<V> = {
            let mut entries = self.entries();
            self.invalidated.fetch_add(1, Ordering::Release);
            if let Ok(mut flights) = self.flights.lock() {
                flights.clear();
            }
            entries.drain().map(|(_, value)| value).collect()
        };
        for value in drained {
            self.dispose(value);
        }
    }
}

#[async_trait::async_trait]
impl<V: Clone + Send + Sync + 'static> crate::extensions::ConnectionPoolCache for LruCache<V> {
    fn cache_id(&self) -> &str {
        self.cache_label
    }

    async fn evict(&self, connector_name: &str) {
        LruCache::evict(self, connector_name).await
    }

    async fn evict_all(&self) {
        LruCache::evict_all(self).await
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

    /// TD-08: Concurrent callers for the same key coalesce into a single
    /// creation, so no duplicate pool is opened or disposed on a cold-start burst.
    #[tokio::test]
    async fn single_flight_coalesces_concurrent_misses_and_cleans_up_flights() {
        let created = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        let closed_ref = closed.clone();
        let cache: Arc<LruCache<u32>> =
            Arc::new(LruCache::with_evict_handler(4, "test", move |_v| {
                closed_ref.fetch_add(1, AtomicOrdering::SeqCst);
            }));
        let start_barrier = Arc::new(tokio::sync::Barrier::new(4));

        let mut handles = Vec::new();
        for v in [1u32, 2, 3, 4] {
            let cache = cache.clone();
            let barrier = start_barrier.clone();
            let created = created.clone();
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                let val: Result<u32, ()> = cache
                    .get_or_create("key", || async move {
                        created.fetch_add(1, AtomicOrdering::SeqCst);
                        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
                        Ok(v)
                    })
                    .await;
                val.expect("test")
            }));
        }
        let mut results = Vec::new();
        for h in handles {
            results.push(h.await.expect("test"));
        }

        assert!(
            results.windows(2).all(|w| w[0] == w[1]),
            "all callers must share the single created value"
        );
        assert_eq!(
            created.load(AtomicOrdering::SeqCst),
            1,
            "single-flight must run creator once for a concurrent burst"
        );
        assert_eq!(closed.load(AtomicOrdering::SeqCst), 0);
        assert!(
            cache.flights.lock().expect("flights").is_empty(),
            "idle flight mutexes must be reclaimed after completion"
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
        // Simulate a race: two callers try to create the same key concurrently.
        // Single-flight coalescing ensures only one creator runs and both see the same value;
        // if the first creation fails, a subsequent retry still succeeds.
        let cache = Arc::new(LruCache::new(4, "test"));
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let creations = Arc::new(AtomicUsize::new(0));

        // First verify failed creation allows retry and cleans up flight entry:
        let err_res: Result<String, &str> = cache
            .get_or_create("key1", || async { Err("transient failure") })
            .await;
        assert_eq!(err_res, Err("transient failure"));
        assert!(cache.flights.lock().expect("flights").is_empty());

        let cache1 = cache.clone();
        let barrier1 = barrier.clone();
        let creations1 = creations.clone();
        let h1 = tokio::spawn(async move {
            barrier1.wait().await;
            let val: Result<String, String> = cache1
                .get_or_create("key1", || {
                    let creations1 = creations1.clone();
                    async move {
                        creations1.fetch_add(1, AtomicOrdering::SeqCst);
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        Ok("from_task_1".to_string())
                    }
                })
                .await;
            val.expect("test")
        });

        let cache2 = cache.clone();
        let barrier2 = barrier.clone();
        let creations2 = creations.clone();
        let h2 = tokio::spawn(async move {
            barrier2.wait().await;
            let val: Result<String, String> = cache2
                .get_or_create("key1", || {
                    let creations2 = creations2.clone();
                    async move {
                        creations2.fetch_add(1, AtomicOrdering::SeqCst);
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        Ok("from_task_2".to_string())
                    }
                })
                .await;
            val.expect("test")
        });

        let (v1, v2) = tokio::join!(h1, h2);
        let v1 = v1.expect("test");
        let v2 = v2.expect("test");

        assert_eq!(v1, v2, "both tasks should see the same cached value");
        assert_eq!(creations.load(AtomicOrdering::SeqCst), 1);
    }
}
