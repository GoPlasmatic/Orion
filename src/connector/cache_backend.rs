//! Unified cache backend abstraction.
//!
//! Provides a [`CacheBackend`] trait with two implementations:
//! - [`MemoryCacheBackend`] — in-process DashMap, always available
//! - [`RedisCacheBackend`] — Redis via multiplexed connection (feature-gated)
//!
//! [`CachePool`] dispatches to the correct backend based on connector config.

use std::sync::Arc;
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
}

/// In-process cache backed by [`DashMap`]. Always available (no feature flag).
pub struct MemoryCacheBackend {
    entries: DashMap<String, MemoryEntry>,
}

impl MemoryCacheBackend {
    /// Create a new in-memory cache with a background cleanup task.
    pub fn new(cleanup_interval_secs: u64) -> Arc<Self> {
        let store = Arc::new(Self {
            entries: DashMap::new(),
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
        Ok(Some(entry.value.clone()))
    }

    async fn set(&self, key: &str, value: &str) -> Result<(), OrionError> {
        self.entries.insert(
            key.to_string(),
            MemoryEntry {
                value: value.to_string(),
                expires_at: None,
            },
        );
        Ok(())
    }

    async fn set_ex(&self, key: &str, value: &str, ttl_secs: u64) -> Result<(), OrionError> {
        self.entries.insert(
            key.to_string(),
            MemoryEntry {
                value: value.to_string(),
                expires_at: Some(Instant::now() + Duration::from_secs(ttl_secs)),
            },
        );
        Ok(())
    }

    async fn check_and_insert(&self, key: &str, window_secs: u64) -> Result<bool, OrionError> {
        use dashmap::mapref::entry::Entry;

        let now = Instant::now();
        let expires_at = now + Duration::from_secs(window_secs);

        match self.entries.entry(key.to_string()) {
            Entry::Vacant(vacant) => {
                vacant.insert(MemoryEntry {
                    value: "1".to_string(),
                    expires_at: Some(expires_at),
                });
                Ok(true) // new key
            }
            Entry::Occupied(mut occupied) => {
                // Check if existing entry has expired
                if let Some(exp) = occupied.get().expires_at
                    && now >= exp
                {
                    // Expired — treat as new
                    occupied.insert(MemoryEntry {
                        value: "1".to_string(),
                        expires_at: Some(expires_at),
                    });
                    return Ok(true);
                }
                Ok(false) // duplicate
            }
        }
    }
}

// ============================================================
// Redis backend (feature-gated)
// ============================================================

pub struct RedisCacheBackend {
    conn: redis::aio::MultiplexedConnection,
}

impl RedisCacheBackend {
    pub fn new(conn: redis::aio::MultiplexedConnection) -> Self {
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
    pub fn new(max_redis_pool_entries: usize, cleanup_interval_secs: u64) -> Self {
        Self {
            memory: MemoryCacheBackend::new(cleanup_interval_secs),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_memory_get_set() {
        let backend = MemoryCacheBackend::new(60);
        assert!(backend.get("k1").await.unwrap().is_none());
        backend.set("k1", "v1").await.unwrap();
        assert_eq!(backend.get("k1").await.unwrap(), Some("v1".to_string()));
    }

    #[tokio::test]
    async fn test_memory_set_ex_expires() {
        let backend = MemoryCacheBackend::new(60);
        backend.set_ex("k1", "v1", 1).await.unwrap();
        assert_eq!(backend.get("k1").await.unwrap(), Some("v1".to_string()));
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(backend.get("k1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_memory_check_and_insert_new() {
        let backend = MemoryCacheBackend::new(60);
        assert!(backend.check_and_insert("dedup-1", 300).await.unwrap());
    }

    #[tokio::test]
    async fn test_memory_check_and_insert_duplicate() {
        let backend = MemoryCacheBackend::new(60);
        assert!(backend.check_and_insert("dedup-1", 300).await.unwrap());
        assert!(!backend.check_and_insert("dedup-1", 300).await.unwrap());
    }

    #[tokio::test]
    async fn test_memory_check_and_insert_expired() {
        let backend = MemoryCacheBackend::new(60);
        assert!(backend.check_and_insert("k", 1).await.unwrap());
        tokio::time::sleep(Duration::from_secs(2)).await;
        // After expiry, key is treated as new
        assert!(backend.check_and_insert("k", 1).await.unwrap());
    }

    #[tokio::test]
    async fn test_memory_purge_expired() {
        let backend = MemoryCacheBackend::new(60);
        backend.set_ex("keep", "val", 3600).await.unwrap();
        backend.set_ex("expire", "val", 1).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        backend.purge_expired();
        assert!(backend.get("keep").await.unwrap().is_some());
        assert!(backend.get("expire").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_memory_set_overwrites() {
        let backend = MemoryCacheBackend::new(60);
        backend.set("k", "v1").await.unwrap();
        backend.set("k", "v2").await.unwrap();
        assert_eq!(backend.get("k").await.unwrap(), Some("v2".to_string()));
    }
}
