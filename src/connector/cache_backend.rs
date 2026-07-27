//! Unified cache backend abstraction.
//!
//! Provides a [`CacheBackend`] trait with two implementations:
//! - [`MemoryCacheBackend`] — in-process DashMap, used when no Redis connector is configured
//! - [`RedisCacheBackend`] — Redis via multiplexed connection, selected per connector config
//!
//! Both backends are always compiled in. [`CachePool`] dispatches to the
//! correct one based on the connector referenced by each cache call.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;

use crate::connector::CacheConnectorConfig;
use crate::errors::OrionError;

/// Abstraction over cache get/set operations.
///
/// Implemented by both in-memory (DashMap) and Redis backends.
#[async_trait]
pub trait CacheBackend: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<String>, OrionError>;
    async fn set(&self, key: &str, value: &str) -> Result<(), OrionError>;
    async fn set_ex(&self, key: &str, value: &str, ttl_secs: u64) -> Result<(), OrionError>;

    /// Deduplication check-and-insert. Returns `true` if the key is **new**
    /// (not a duplicate), `false` if a duplicate within `window_secs`.
    async fn check_and_insert(&self, key: &str, window_secs: u64) -> Result<bool, OrionError>;
}

// ============================================================
// In-memory backend (DashMap)
// ============================================================

struct MemoryEntry {
    value: String,
    expires_at: Option<Instant>,
    /// Monotonic tick of the last read/write, for approximate-LRU eviction.
    last_access: AtomicU64,
}

/// Fraction of `max_entries` reclaimed by one eviction sweep. Evicting a
/// batch keeps the O(n) sweep amortized to a constant per insert; evicting
/// exactly one entry per insert would rescan the whole map every time.
const EVICTION_BATCH_DIVISOR: usize = 10;

/// In-process cache backed by [`DashMap`], bounded by `max_entries` with
/// approximate-LRU eviction.
///
/// The bound is not decorative: this instance backs the default dedup store,
/// the default response cache, and every `cache_write` to a `backend:
/// "memory"` connector, and `set()` (no TTL) stores entries the expiry sweep
/// can never reclaim — so without it, workflow config alone can OOM the
/// process.
pub struct MemoryCacheBackend {
    entries: DashMap<String, MemoryEntry>,
    /// `0` = unbounded (opt-out).
    max_entries: usize,
    /// Access clock. Ticks are unique per access, which is what lets an
    /// eviction sweep pick a threshold and `retain` against it.
    clock: AtomicU64,
    /// Held by the thread running an eviction sweep; others skip it and
    /// overshoot the bound slightly rather than pile up on the same scan.
    evicting: AtomicBool,
}

impl MemoryCacheBackend {
    /// Create a new in-memory cache with a background cleanup task.
    /// `max_entries` of 0 disables the bound.
    pub fn new(cleanup_interval_secs: u64, max_entries: usize) -> Arc<Self> {
        let store = Arc::new(Self {
            entries: DashMap::new(),
            max_entries,
            clock: AtomicU64::new(0),
            evicting: AtomicBool::new(false),
        });

        let weak = Arc::downgrade(&store);
        tokio::spawn(async move {
            let interval = Duration::from_secs(cleanup_interval_secs.max(1));
            loop {
                tokio::time::sleep(interval).await;
                let Some(store) = weak.upgrade() else {
                    break;
                };
                store.purge_expired();
            }
        });

        store
    }

    fn purge_expired(&self) {
        let now = Instant::now();
        self.entries
            .retain(|_, entry| entry.expires_at.is_none_or(|exp| exp > now));
    }

    fn tick(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::Relaxed)
    }

    fn new_entry(&self, value: &str, expires_at: Option<Instant>) -> MemoryEntry {
        MemoryEntry {
            value: value.to_string(),
            expires_at,
            last_access: AtomicU64::new(self.tick()),
        }
    }

    /// Bring the map back under `max_entries` after an insert. Expired
    /// entries go first (free and always correct); only if that is not
    /// enough are live entries evicted oldest-access-first.
    fn enforce_bound(&self) {
        if self.max_entries == 0 || self.entries.len() <= self.max_entries {
            return;
        }
        if self
            .evicting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        self.purge_expired();
        let len = self.entries.len();
        if len > self.max_entries {
            let batch = (self.max_entries / EVICTION_BATCH_DIVISOR).max(1);
            let target = (len - self.max_entries + batch).min(len);
            // Threshold search over the access ticks only — evicting by key
            // would mean cloning every key in the map.
            let mut ticks: Vec<u64> = self
                .entries
                .iter()
                .map(|e| e.last_access.load(Ordering::Relaxed))
                .collect();
            if target >= 1 && target <= ticks.len() {
                let (_, nth, _) = ticks.select_nth_unstable(target - 1);
                let threshold = *nth;
                self.entries
                    .retain(|_, entry| entry.last_access.load(Ordering::Relaxed) > threshold);
                tracing::warn!(
                    max_entries = self.max_entries,
                    evicted = len.saturating_sub(self.entries.len()),
                    "In-memory cache at capacity, evicted least-recently-used entries"
                );
            }
        }

        self.evicting.store(false, Ordering::Release);
    }
}

#[async_trait]
impl CacheBackend for MemoryCacheBackend {
    async fn get(&self, key: &str) -> Result<Option<String>, OrionError> {
        let Some(entry) = self.entries.get(key) else {
            return Ok(None);
        };
        // Check expiry on read (lazy cleanup)
        if let Some(exp) = entry.expires_at
            && Instant::now() >= exp
        {
            drop(entry); // release read ref before removing
            self.entries.remove(key);
            return Ok(None);
        }
        entry.last_access.store(self.tick(), Ordering::Relaxed);
        Ok(Some(entry.value.clone()))
    }

    async fn set(&self, key: &str, value: &str) -> Result<(), OrionError> {
        // No TTL: `purge_expired` can never reclaim these, so the LRU bound
        // is the only thing standing between `cache_write` and an OOM.
        self.entries
            .insert(key.to_string(), self.new_entry(value, None));
        self.enforce_bound();
        Ok(())
    }

    async fn set_ex(&self, key: &str, value: &str, ttl_secs: u64) -> Result<(), OrionError> {
        self.entries.insert(
            key.to_string(),
            self.new_entry(value, Some(Instant::now() + Duration::from_secs(ttl_secs))),
        );
        self.enforce_bound();
        Ok(())
    }

    async fn check_and_insert(&self, key: &str, window_secs: u64) -> Result<bool, OrionError> {
        use dashmap::mapref::entry::Entry;

        let now = Instant::now();
        let expires_at = now + Duration::from_secs(window_secs);

        let inserted = match self.entries.entry(key.to_string()) {
            Entry::Vacant(vacant) => {
                vacant.insert(self.new_entry("1", Some(expires_at)));
                true // new key
            }
            Entry::Occupied(mut occupied) => {
                // Check if existing entry has expired
                if let Some(exp) = occupied.get().expires_at
                    && now >= exp
                {
                    // Expired — treat as new
                    occupied.insert(self.new_entry("1", Some(expires_at)));
                    true
                } else {
                    false // duplicate
                }
            }
        };
        if inserted {
            self.enforce_bound();
        }
        Ok(inserted)
    }
}

// ============================================================
// Redis backend
// ============================================================

pub struct RedisCacheBackend {
    conn: redis::aio::ConnectionManager,
}

impl RedisCacheBackend {
    pub fn new(conn: redis::aio::ConnectionManager) -> Self {
        Self { conn }
    }
}

#[async_trait]
impl CacheBackend for RedisCacheBackend {
    async fn get(&self, key: &str) -> Result<Option<String>, OrionError> {
        use redis::AsyncCommands;
        let mut conn = self.conn.clone();
        conn.get(key).await.map_err(|e| OrionError::InternalSource {
            context: format!("Redis GET failed for key '{key}'"),
            source: Box::new(e),
        })
    }

    async fn set(&self, key: &str, value: &str) -> Result<(), OrionError> {
        use redis::AsyncCommands;
        let mut conn = self.conn.clone();
        conn.set::<_, _, ()>(key, value)
            .await
            .map_err(|e| OrionError::InternalSource {
                context: format!("Redis SET failed for key '{key}'"),
                source: Box::new(e),
            })
    }

    async fn set_ex(&self, key: &str, value: &str, ttl_secs: u64) -> Result<(), OrionError> {
        use redis::AsyncCommands;
        let mut conn = self.conn.clone();
        conn.set_ex::<_, _, ()>(key, value, ttl_secs)
            .await
            .map_err(|e| OrionError::InternalSource {
                context: format!("Redis SETEX failed for key '{key}'"),
                source: Box::new(e),
            })
    }

    async fn check_and_insert(&self, key: &str, window_secs: u64) -> Result<bool, OrionError> {
        let mut conn = self.conn.clone();
        // SET key "1" NX EX window_secs — atomic check-and-insert
        let result: Option<String> = redis::cmd("SET")
            .arg(key)
            .arg("1")
            .arg("NX")
            .arg("EX")
            .arg(window_secs)
            .query_async(&mut conn)
            .await
            .map_err(|e| OrionError::InternalSource {
                context: format!("Redis SET NX EX failed for key '{key}'"),
                source: Box::new(e),
            })?;
        // Redis returns "OK" if SET succeeded (key was new), nil if key existed
        Ok(result.is_some())
    }
}

// ============================================================
// CachePool — dispatches to the correct backend
// ============================================================

/// Holds both backend implementations and dispatches based on connector config.
pub struct CachePool {
    memory: Arc<MemoryCacheBackend>,
    redis: Arc<super::redis_pool::RedisPoolCache>,
}

impl CachePool {
    pub fn new(
        max_redis_pool_entries: usize,
        cleanup_interval_secs: u64,
        max_memory_cache_entries: usize,
    ) -> Self {
        Self {
            memory: MemoryCacheBackend::new(cleanup_interval_secs, max_memory_cache_entries),
            redis: Arc::new(super::redis_pool::RedisPoolCache::new(
                max_redis_pool_entries,
            )),
        }
    }

    /// Get a cache backend for the given connector.
    pub async fn get_backend(
        &self,
        connector_name: &str,
        config: &CacheConnectorConfig,
    ) -> Result<Arc<dyn CacheBackend>, OrionError> {
        match config.backend.as_str() {
            "memory" => Ok(self.memory.clone() as Arc<dyn CacheBackend>),
            "redis" => {
                let conn = self.redis.get_conn(connector_name, config).await?;
                Ok(Arc::new(RedisCacheBackend::new(conn)))
            }
            other => Err(OrionError::BadRequest(format!(
                "Unknown cache backend '{other}'. Must be 'redis' or 'memory'"
            ))),
        }
    }

    /// Get the shared in-memory backend (used as default for dedup when no connector specified).
    pub fn memory(&self) -> Arc<dyn CacheBackend> {
        self.memory.clone() as Arc<dyn CacheBackend>
    }

    /// Evict a cached Redis connection pool for the named connector.
    pub async fn evict_pool(&self, connector_name: &str) {
        self.redis.evict(connector_name).await;
    }

    /// Evict every cached Redis connection (epoch-driven resync — a remote
    /// node cannot know which connector changed).
    pub async fn evict_all_pools(&self) {
        self.redis.evict_all().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_memory_get_set() {
        let backend = MemoryCacheBackend::new(60, 0);
        assert!(backend.get("k1").await.expect("test").is_none());
        backend.set("k1", "v1").await.expect("test");
        assert_eq!(
            backend.get("k1").await.expect("test"),
            Some("v1".to_string())
        );
    }

    #[tokio::test]
    async fn test_memory_set_ex_expires() {
        let backend = MemoryCacheBackend::new(60, 0);
        backend.set_ex("k1", "v1", 1).await.expect("test");
        assert_eq!(
            backend.get("k1").await.expect("test"),
            Some("v1".to_string())
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(backend.get("k1").await.expect("test").is_none());
    }

    #[tokio::test]
    async fn test_memory_check_and_insert_new() {
        let backend = MemoryCacheBackend::new(60, 0);
        assert!(
            backend
                .check_and_insert("dedup-1", 300)
                .await
                .expect("test")
        );
    }

    #[tokio::test]
    async fn test_memory_check_and_insert_duplicate() {
        let backend = MemoryCacheBackend::new(60, 0);
        assert!(
            backend
                .check_and_insert("dedup-1", 300)
                .await
                .expect("test")
        );
        assert!(
            !backend
                .check_and_insert("dedup-1", 300)
                .await
                .expect("test")
        );
    }

    #[tokio::test]
    async fn test_memory_check_and_insert_expired() {
        let backend = MemoryCacheBackend::new(60, 0);
        assert!(backend.check_and_insert("k", 1).await.expect("test"));
        tokio::time::sleep(Duration::from_secs(2)).await;
        // After expiry, key is treated as new
        assert!(backend.check_and_insert("k", 1).await.expect("test"));
    }

    #[tokio::test]
    async fn test_memory_purge_expired() {
        let backend = MemoryCacheBackend::new(60, 0);
        backend.set_ex("keep", "val", 3600).await.expect("test");
        backend.set_ex("expire", "val", 1).await.expect("test");
        tokio::time::sleep(Duration::from_secs(2)).await;
        backend.purge_expired();
        assert!(backend.get("keep").await.expect("test").is_some());
        assert!(backend.get("expire").await.expect("test").is_none());
    }

    #[tokio::test]
    async fn test_memory_set_overwrites() {
        let backend = MemoryCacheBackend::new(60, 0);
        backend.set("k", "v1").await.expect("test");
        backend.set("k", "v2").await.expect("test");
        assert_eq!(
            backend.get("k").await.expect("test"),
            Some("v2".to_string())
        );
    }

    // ---- N12: bounded memory backend ----

    /// The no-TTL `set()` path is the one `purge_expired` can never reclaim,
    /// so it is the one the bound has to cover.
    #[tokio::test]
    async fn test_memory_set_without_ttl_is_bounded() {
        let backend = MemoryCacheBackend::new(60, 100);
        for i in 0..10_000 {
            backend.set(&format!("k{i}"), "v").await.expect("test");
        }
        assert!(
            backend.entries.len() <= 100,
            "unbounded growth: {} entries",
            backend.entries.len()
        );
    }

    #[tokio::test]
    async fn test_memory_set_ex_is_bounded() {
        let backend = MemoryCacheBackend::new(60, 50);
        for i in 0..2_000 {
            backend
                .set_ex(&format!("k{i}"), "v", 3600)
                .await
                .expect("test");
        }
        assert!(backend.entries.len() <= 50);
    }

    /// Dedup keys are the other unbounded source (one per idempotency key).
    #[tokio::test]
    async fn test_memory_check_and_insert_is_bounded() {
        let backend = MemoryCacheBackend::new(60, 64);
        for i in 0..5_000 {
            backend
                .check_and_insert(&format!("dedup-{i}"), 3600)
                .await
                .expect("test");
        }
        assert!(backend.entries.len() <= 64);
    }

    #[tokio::test]
    async fn test_memory_lru_evicts_coldest_first() {
        let backend = MemoryCacheBackend::new(60, 10);
        for i in 0..10 {
            backend.set(&format!("k{i}"), "v").await.expect("test");
        }
        // A read makes "k0" the most-recently-used, so the next insert must
        // reclaim "k1"/"k2" (the coldest) instead.
        assert!(backend.get("k0").await.expect("test").is_some());
        backend.set("overflow", "v").await.expect("test");

        assert!(
            backend.get("k0").await.expect("test").is_some(),
            "a key read since insertion must outlive colder ones"
        );
        assert!(
            backend.get("k1").await.expect("test").is_none(),
            "the coldest key must be the one evicted"
        );
        assert!(backend.get("overflow").await.expect("test").is_some());
    }

    #[tokio::test]
    async fn test_memory_expired_entries_evicted_before_live_ones() {
        let backend = MemoryCacheBackend::new(3600, 4);
        for i in 0..4 {
            backend
                .set_ex(&format!("gone{i}"), "v", 1)
                .await
                .expect("test");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        backend.set("live", "v").await.expect("test");
        assert_eq!(
            backend.get("live").await.expect("test"),
            Some("v".to_string()),
            "a fresh entry must not be evicted while expired ones remain"
        );
        assert!(backend.entries.len() <= 4);
    }

    #[tokio::test]
    async fn test_memory_zero_max_entries_is_unbounded() {
        let backend = MemoryCacheBackend::new(3600, 0);
        for i in 0..500 {
            backend.set(&format!("k{i}"), "v").await.expect("test");
        }
        assert_eq!(backend.entries.len(), 500);
    }

    #[tokio::test]
    async fn test_memory_bound_holds_under_concurrent_writers() {
        let backend = MemoryCacheBackend::new(3600, 100);
        let mut handles = Vec::new();
        for w in 0..8 {
            let backend = backend.clone();
            handles.push(tokio::spawn(async move {
                for i in 0..500 {
                    backend.set(&format!("w{w}-k{i}"), "v").await.expect("test");
                }
            }));
        }
        for h in handles {
            h.await.expect("test");
        }
        // Concurrent inserts may overshoot briefly (only one thread sweeps),
        // but the map must stay within a small factor of the bound rather
        // than growing to the 4000 keys written.
        assert!(
            backend.entries.len() <= 400,
            "bound not holding: {} entries",
            backend.entries.len()
        );
    }
}
