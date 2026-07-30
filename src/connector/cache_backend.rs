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
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
// TTL clock. `tokio::time::Instant` is a transparent wrapper over
// `std::time::Instant` in production builds (the mock clock only exists
// under the dev-only `test-util` feature), but lets TTL tests drive expiry
// deterministically with a paused clock instead of real sleeps.
use tokio::time::Instant;

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

    /// Delete `key`. Deleting a key that is not present is not an error.
    async fn remove(&self, key: &str) -> Result<(), OrionError>;

    /// Atomically claim a deduplication key on behalf of `owner`.
    ///
    /// * `Ok(None)` — the key was free (or its window had expired) and is now
    ///   held by `owner` for `window_secs`.
    /// * `Ok(Some(holder))` — the key is already held, and `holder` is the
    ///   `owner` string the current holder claimed it with.
    ///
    /// Returning the holder rather than a bare "is duplicate" boolean is what
    /// lets a redelivering transport tell *its own unfinished claim* from a
    /// genuinely earlier delivery: a Kafka record replayed because its offset
    /// never committed claims with the same owner token it used last time,
    /// and must be processed, not skipped (see `check_deduplication` in
    /// [`crate::channel::guards`]).
    async fn claim_dedup_key(
        &self,
        key: &str,
        owner: &str,
        window_secs: u64,
    ) -> Result<Option<String>, OrionError>;
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
/// One instance exists **per namespace** (S19): the built-in dedup store,
/// the built-in response cache, and every `(purpose, connector)` use of a
/// `backend: "memory"` connector each get their own, so `max_entries`
/// (`engine.max_memory_cache_entries`) is a per-namespace budget — each
/// instance carries its own bound and cleanup task, and the process-wide
/// worst case is `max_entries × number of namespaces`, not one shared
/// budget. The bound is not decorative: `set()` (no TTL) stores entries the
/// expiry sweep can never reclaim, so without it workflow config alone can
/// OOM the process.
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

    async fn remove(&self, key: &str) -> Result<(), OrionError> {
        self.entries.remove(key);
        Ok(())
    }

    async fn claim_dedup_key(
        &self,
        key: &str,
        owner: &str,
        window_secs: u64,
    ) -> Result<Option<String>, OrionError> {
        use dashmap::mapref::entry::Entry;

        let now = Instant::now();
        let expires_at = now + Duration::from_secs(window_secs);

        let holder = match self.entries.entry(key.to_string()) {
            Entry::Vacant(vacant) => {
                vacant.insert(self.new_entry(owner, Some(expires_at)));
                None // claimed
            }
            Entry::Occupied(mut occupied) => {
                // Check if existing entry has expired
                if let Some(exp) = occupied.get().expires_at
                    && now >= exp
                {
                    // Expired — the window has passed, so the key is free
                    occupied.insert(self.new_entry(owner, Some(expires_at)));
                    None
                } else {
                    Some(occupied.get().value.clone())
                }
            }
        };
        if holder.is_none() {
            self.enforce_bound();
        }
        Ok(holder)
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

    async fn remove(&self, key: &str) -> Result<(), OrionError> {
        use redis::AsyncCommands;
        let mut conn = self.conn.clone();
        conn.del::<_, ()>(key)
            .await
            .map_err(|e| OrionError::InternalSource {
                context: format!("Redis DEL failed for key '{key}'"),
                source: Box::new(e),
            })
    }

    async fn claim_dedup_key(
        &self,
        key: &str,
        owner: &str,
        window_secs: u64,
    ) -> Result<Option<String>, OrionError> {
        use redis::AsyncCommands;
        let mut conn = self.conn.clone();
        // Two rounds, because SET NX and GET are two commands and the key can
        // expire between them: a failed SET followed by a nil GET means the
        // holder's window ended in that gap, i.e. nobody holds the key and the
        // claim should simply be retried. Two rounds is enough in practice;
        // losing both is treated as "claimed", because processing a message
        // twice is the transport's documented guarantee while skipping one is
        // silent loss.
        for _ in 0..2 {
            // SET key owner NX EX window_secs — atomic claim
            let claimed: Option<String> = redis::cmd("SET")
                .arg(key)
                .arg(owner)
                .arg("NX")
                .arg("EX")
                .arg(window_secs)
                .query_async(&mut conn)
                .await
                .map_err(|e| OrionError::InternalSource {
                    context: format!("Redis SET NX EX failed for key '{key}'"),
                    source: Box::new(e),
                })?;
            // Redis returns "OK" if SET succeeded (key was free), nil if held
            if claimed.is_some() {
                return Ok(None);
            }
            let holder: Option<String> =
                conn.get(key)
                    .await
                    .map_err(|e| OrionError::InternalSource {
                        context: format!("Redis GET failed for key '{key}'"),
                        source: Box::new(e),
                    })?;
            if let Some(holder) = holder {
                return Ok(Some(holder));
            }
        }
        tracing::warn!(
            key = %key,
            "Deduplication key expired between claim attempts twice; treating the message as new"
        );
        Ok(None)
    }
}

// ============================================================
// CachePool — dispatches to the correct backend
// ============================================================

/// What a cache backend is being used *for*. In-memory backends are
/// namespaced by purpose and connector name (S19): before this, every
/// `backend: "memory"` connector AND both internal stores shared one
/// instance, so a workflow `cache_write` with a crafted `dedup:{channel}:…`
/// key could manufacture a 409 for a real request or forge a cached
/// response, two memory connectors silently shared one keyspace, and the
/// LRU bound was one shared budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CachePurpose {
    /// Workflow-facing `cache_read` / `cache_write`.
    Workflow,
    /// The channel deduplication (idempotency) store.
    Dedup,
    /// The channel response cache.
    ResponseCache,
}

impl CachePurpose {
    /// Stable namespace segment. Purpose-only for the built-in default
    /// store, `purpose:connector` for a named connector — the two can never
    /// collide because connector names are non-empty.
    fn as_str(self) -> &'static str {
        match self {
            CachePurpose::Workflow => "workflow",
            CachePurpose::Dedup => "dedup",
            CachePurpose::ResponseCache => "response_cache",
        }
    }
}

/// Human label for log lines and quarantine reasons.
impl std::fmt::Display for CachePurpose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CachePurpose::Workflow => "workflow cache",
            CachePurpose::Dedup => "deduplication",
            CachePurpose::ResponseCache => "response cache",
        })
    }
}

/// Holds both backend implementations and dispatches based on connector config.
pub struct CachePool {
    /// One in-memory instance per namespace (see [`CachePurpose`]), created
    /// lazily and kept for the life of the process — like the single shared
    /// instance before it, entries survive connector reloads and engine
    /// reloads, and only a restart clears them. Distinct instances rather
    /// than key prefixes so each namespace also gets its own LRU budget: a
    /// hot workflow cache can no longer evict dedup entries.
    memory: DashMap<String, Arc<MemoryCacheBackend>>,
    cleanup_interval_secs: u64,
    max_memory_cache_entries: usize,
    redis: Arc<super::redis_pool::RedisPoolCache>,
}

impl CachePool {
    pub fn new(
        max_redis_pool_entries: usize,
        cleanup_interval_secs: u64,
        max_memory_cache_entries: usize,
    ) -> Self {
        Self {
            memory: DashMap::new(),
            cleanup_interval_secs,
            max_memory_cache_entries,
            redis: Arc::new(super::redis_pool::RedisPoolCache::new(
                max_redis_pool_entries,
            )),
        }
    }

    /// The in-memory instance for `namespace`, created on first use.
    ///
    /// Read-locked fast path: the namespace set is tiny and fully populated
    /// after the first request per connector, and `entry` takes an exclusive
    /// shard lock, which would serialize concurrent cache reads on what is
    /// almost always a hit.
    fn memory_namespace(&self, namespace: &str) -> Arc<dyn CacheBackend> {
        if let Some(existing) = self.memory.get(namespace) {
            return existing.value().clone() as Arc<dyn CacheBackend>;
        }
        self.memory
            .entry(namespace.to_string())
            .or_insert_with(|| {
                MemoryCacheBackend::new(self.cleanup_interval_secs, self.max_memory_cache_entries)
            })
            .clone() as Arc<dyn CacheBackend>
    }

    /// Get a cache backend for the given connector, scoped to `purpose`.
    ///
    /// Memory backends are isolated per `(purpose, connector)`. Redis
    /// backends are not: Redis is an external store the operator points
    /// connectors at, its data must survive restarts and be shared across
    /// nodes, and workflows legitimately read keys written by other systems
    /// through it. Pointing a workflow cache connector at the same Redis
    /// database as a channel's dedup store therefore shares a keyspace —
    /// use separate databases (`redis://host/0`, `/1`, …) to isolate them.
    pub async fn get_backend(
        &self,
        purpose: CachePurpose,
        connector_name: &str,
        config: &CacheConnectorConfig,
    ) -> Result<Arc<dyn CacheBackend>, OrionError> {
        match config.backend.as_str() {
            "memory" => {
                Ok(self.memory_namespace(&format!("{}:{connector_name}", purpose.as_str())))
            }
            "redis" => {
                let conn = self.redis.get_conn(connector_name, config).await?;
                Ok(Arc::new(RedisCacheBackend::new(conn)))
            }
            other => Err(OrionError::BadRequest(format!(
                "Unknown cache backend '{other}'. Must be 'redis' or 'memory'"
            ))),
        }
    }

    /// The built-in in-memory backend for `purpose` — the default when a
    /// channel's dedup or response-cache config names no connector.
    pub fn default_memory(&self, purpose: CachePurpose) -> Arc<dyn CacheBackend> {
        self.memory_namespace(purpose.as_str())
    }

    /// Evict a cached Redis connection pool for the named connector.
    /// In-memory namespaces are deliberately left alone: the namespace key
    /// is stable, so a reloaded connector reattaches to its existing
    /// entries, exactly as the pre-S19 shared instance did.
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

    #[tokio::test(start_paused = true)]
    async fn test_memory_set_ex_expires() {
        let backend = MemoryCacheBackend::new(60, 0);
        backend.set_ex("k1", "v1", 1).await.expect("test");
        assert_eq!(
            backend.get("k1").await.expect("test"),
            Some("v1".to_string())
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(backend.get("k1").await.expect("test").is_none());
    }

    #[tokio::test]
    async fn test_memory_claim_dedup_key_new() {
        let backend = MemoryCacheBackend::new(60, 0);
        assert_eq!(
            backend
                .claim_dedup_key("dedup-1", "owner-a", 300)
                .await
                .expect("test"),
            None
        );
    }

    /// A second claimant is told **who** holds the key, not merely that
    /// somebody does: that is what lets a redelivering transport recognise
    /// its own unfinished claim instead of refusing its own message.
    #[tokio::test]
    async fn test_memory_claim_dedup_key_reports_the_holder() {
        let backend = MemoryCacheBackend::new(60, 0);
        assert_eq!(
            backend
                .claim_dedup_key("dedup-1", "owner-a", 300)
                .await
                .expect("test"),
            None
        );
        assert_eq!(
            backend
                .claim_dedup_key("dedup-1", "owner-b", 300)
                .await
                .expect("test"),
            Some("owner-a".to_string())
        );
    }

    #[tokio::test]
    async fn test_memory_remove_frees_a_claim() {
        let backend = MemoryCacheBackend::new(60, 0);
        backend
            .claim_dedup_key("dedup-1", "owner-a", 300)
            .await
            .expect("test");
        backend.remove("dedup-1").await.expect("test");
        assert_eq!(
            backend
                .claim_dedup_key("dedup-1", "owner-b", 300)
                .await
                .expect("test"),
            None,
            "a released key must be claimable again"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_memory_claim_dedup_key_expired() {
        let backend = MemoryCacheBackend::new(60, 0);
        assert_eq!(
            backend
                .claim_dedup_key("k", "owner-a", 1)
                .await
                .expect("test"),
            None
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        // After expiry, key is free again
        assert_eq!(
            backend
                .claim_dedup_key("k", "owner-b", 1)
                .await
                .expect("test"),
            None
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_memory_purge_expired() {
        let backend = MemoryCacheBackend::new(60, 0);
        backend.set_ex("keep", "val", 3600).await.expect("test");
        backend.set_ex("expire", "val", 1).await.expect("test");
        tokio::time::advance(Duration::from_secs(2)).await;
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
    async fn test_memory_claim_dedup_key_is_bounded() {
        let backend = MemoryCacheBackend::new(60, 64);
        for i in 0..5_000 {
            backend
                .claim_dedup_key(&format!("dedup-{i}"), "owner", 3600)
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

    #[tokio::test(start_paused = true)]
    async fn test_memory_expired_entries_evicted_before_live_ones() {
        let backend = MemoryCacheBackend::new(3600, 4);
        for i in 0..4 {
            backend
                .set_ex(&format!("gone{i}"), "v", 1)
                .await
                .expect("test");
        }
        tokio::time::advance(Duration::from_secs(2)).await;
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

    // ---- S19: purpose/connector namespacing ----

    fn memory_connector() -> CacheConnectorConfig {
        CacheConnectorConfig {
            backend: "memory".to_string(),
            url: None,
            allow_private_urls: false,
            operations: Default::default(),
        }
    }

    /// The attack S19 closes: a workflow `cache_write` of a crafted
    /// `dedup:{channel}:{key}` key must not register as a seen idempotency
    /// key in the dedup store.
    #[tokio::test]
    async fn workflow_writes_cannot_poison_the_dedup_store() {
        let pool = CachePool::new(4, 60, 1000);
        let workflow = pool
            .get_backend(CachePurpose::Workflow, "wf-cache", &memory_connector())
            .await
            .expect("test");
        workflow
            .set("dedup:orders:token-1", "\"1\"")
            .await
            .expect("test");

        let dedup = pool.default_memory(CachePurpose::Dedup);
        assert_eq!(
            dedup
                .claim_dedup_key("dedup:orders:token-1", "owner", 300)
                .await
                .expect("test"),
            None,
            "a workflow-written key must not read as a duplicate in the dedup store"
        );
    }

    /// …and the same for the response cache: a forged `cache:{channel}:{hash}`
    /// entry must not be served to a real request.
    #[tokio::test]
    async fn workflow_writes_cannot_forge_a_cached_response() {
        let pool = CachePool::new(4, 60, 1000);
        let workflow = pool
            .get_backend(CachePurpose::Workflow, "wf-cache", &memory_connector())
            .await
            .expect("test");
        workflow
            .set("cache:orders:00000000deadbeef", r#"{"forged":true}"#)
            .await
            .expect("test");

        let response_cache = pool.default_memory(CachePurpose::ResponseCache);
        assert!(
            response_cache
                .get("cache:orders:00000000deadbeef")
                .await
                .expect("test")
                .is_none(),
            "a workflow-written key must not surface as a cached response"
        );
    }

    #[tokio::test]
    async fn two_memory_connectors_have_distinct_keyspaces() {
        let pool = CachePool::new(4, 60, 1000);
        let a = pool
            .get_backend(CachePurpose::Workflow, "conn-a", &memory_connector())
            .await
            .expect("test");
        let b = pool
            .get_backend(CachePurpose::Workflow, "conn-b", &memory_connector())
            .await
            .expect("test");

        a.set("shared-key", "\"from-a\"").await.expect("test");
        assert!(
            b.get("shared-key").await.expect("test").is_none(),
            "connector 'conn-b' must not see keys written through 'conn-a'"
        );
        // Same connector, same purpose: the namespace is stable, so a second
        // resolution reattaches to the same entries.
        let a2 = pool
            .get_backend(CachePurpose::Workflow, "conn-a", &memory_connector())
            .await
            .expect("test");
        assert_eq!(
            a2.get("shared-key").await.expect("test"),
            Some("\"from-a\"".to_string())
        );
    }

    /// Distinct instances mean distinct LRU budgets: flooding one namespace
    /// must not evict another namespace's entries.
    #[tokio::test]
    async fn lru_budgets_are_per_namespace() {
        let pool = CachePool::new(4, 60, 10);
        let dedup = pool.default_memory(CachePurpose::Dedup);
        assert_eq!(
            dedup
                .claim_dedup_key("dedup:ch:tok", "owner", 300)
                .await
                .expect("test"),
            None
        );

        let workflow = pool
            .get_backend(CachePurpose::Workflow, "hot", &memory_connector())
            .await
            .expect("test");
        for i in 0..1_000 {
            workflow.set(&format!("k{i}"), "v").await.expect("test");
        }

        assert_eq!(
            dedup
                .claim_dedup_key("dedup:ch:tok", "owner", 300)
                .await
                .expect("test"),
            Some("owner".to_string()),
            "a hot workflow cache must not evict dedup entries"
        );
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
