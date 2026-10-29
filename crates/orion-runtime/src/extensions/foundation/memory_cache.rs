use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{
    AttemptContext, ConnectionPoolCache, ConnectorError, ConnectorOutput, DRIVER_MEMORY_CACHE,
    EffectiveConnectionIdentity, ImplementationKind, ProviderBackend, codes,
};

#[derive(Debug, Clone)]
struct CacheEntry {
    value: Value,
    expires_at: Option<Instant>,
    inserted_seq: u64,
}

/// Bounded, connection-partitioned in-memory cache store.
///
/// Keyed by [`EffectiveConnectionIdentity`] so:
/// - Two different connections never share keys.
/// - Old and new revisions of a connection have isolated keyspaces while
///   participating in [`ConnectionPoolCache`] eviction.
/// - Total partitions, entries per partition, and per-value byte sizes are bounded.
/// - Capacity eviction is FIFO (oldest inserted entry or partition after
///   purging expired entries).
pub struct PartitionedMemoryCache {
    max_entries_per_partition: usize,
    max_partitions: usize,
    max_value_bytes: usize,
    next_seq: AtomicU64,
    partitions: Mutex<BTreeMap<EffectiveConnectionIdentity, BTreeMap<String, CacheEntry>>>,
}

impl PartitionedMemoryCache {
    pub fn new(max_entries_per_partition: usize) -> Self {
        Self::with_limits(max_entries_per_partition, 256, 1024 * 1024)
    }

    pub fn with_limits(
        max_entries_per_partition: usize,
        max_partitions: usize,
        max_value_bytes: usize,
    ) -> Self {
        Self {
            max_entries_per_partition: max_entries_per_partition.max(1),
            max_partitions: max_partitions.max(1),
            max_value_bytes: max_value_bytes.max(64),
            next_seq: AtomicU64::new(1),
            partitions: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn max_value_bytes(&self) -> usize {
        self.max_value_bytes
    }

    pub fn get(&self, identity: &EffectiveConnectionIdentity, key: &str) -> Option<Value> {
        let mut guard = self.partitions.lock().unwrap_or_else(|e| e.into_inner());
        let partition = guard.get_mut(identity)?;
        let Some(entry) = partition.get(key) else {
            if partition.is_empty() {
                guard.remove(identity);
                orion_common::metrics::set_memory_cache_partitions(guard.len());
            }
            return None;
        };
        if let Some(exp) = entry.expires_at
            && Instant::now() >= exp
        {
            partition.remove(key);
            if partition.is_empty() {
                guard.remove(identity);
                orion_common::metrics::set_memory_cache_partitions(guard.len());
            }
            return None;
        }
        Some(entry.value.clone())
    }

    pub fn set(
        &self,
        identity: &EffectiveConnectionIdentity,
        key: String,
        value: Value,
        ttl: Option<Duration>,
    ) {
        let mut guard = self.partitions.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();

        if !guard.contains_key(identity) && guard.len() >= self.max_partitions {
            // Sweep expired entries across all partitions and drop empty partitions first.
            for part in guard.values_mut() {
                part.retain(|_, v| v.expires_at.is_none_or(|exp| now < exp));
            }
            guard.retain(|_, part| !part.is_empty());

            // If still at capacity, evict the partition holding the oldest entry (FIFO).
            if guard.len() >= self.max_partitions
                && let Some(oldest_partition_key) = guard
                    .iter()
                    .min_by_key(|(_, part)| {
                        part.values().map(|e| e.inserted_seq).min().unwrap_or(0)
                    })
                    .map(|(k, _)| k.clone())
            {
                guard.remove(&oldest_partition_key);
            }
        }

        let partition = guard.entry(identity.clone()).or_default();

        // Purge expired entries first if we are at capacity.
        if partition.len() >= self.max_entries_per_partition && !partition.contains_key(&key) {
            partition.retain(|_, v| v.expires_at.is_none_or(|exp| now < exp));
        }
        // If still at capacity, evict the oldest entry (FIFO by insertion sequence).
        if partition.len() >= self.max_entries_per_partition
            && !partition.contains_key(&key)
            && let Some(oldest_key) = partition
                .iter()
                .min_by_key(|(_, v)| v.inserted_seq)
                .map(|(k, _)| k.clone())
        {
            partition.remove(&oldest_key);
        }

        let expires_at = ttl.and_then(|d| now.checked_add(d));
        let inserted_seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        partition.insert(
            key,
            CacheEntry {
                value,
                expires_at,
                inserted_seq,
            },
        );
        orion_common::metrics::set_memory_cache_partitions(guard.len());
    }

    pub fn delete(&self, identity: &EffectiveConnectionIdentity, key: &str) -> bool {
        let mut guard = self.partitions.lock().unwrap_or_else(|e| e.into_inner());
        let Some(partition) = guard.get_mut(identity) else {
            return false;
        };
        let removed = partition.remove(key).is_some();
        if partition.is_empty() {
            guard.remove(identity);
            orion_common::metrics::set_memory_cache_partitions(guard.len());
        }
        removed
    }

    pub fn remove_partition(&self, identity: &EffectiveConnectionIdentity) {
        let mut guard = self.partitions.lock().unwrap_or_else(|e| e.into_inner());
        guard.remove(identity);
        orion_common::metrics::set_memory_cache_partitions(guard.len());
    }

    #[cfg(test)]
    pub fn partition_count(&self) -> usize {
        self.partitions.lock().map(|g| g.len()).unwrap_or(0)
    }
}

#[async_trait::async_trait]
impl ConnectionPoolCache for PartitionedMemoryCache {
    fn cache_id(&self) -> &str {
        DRIVER_MEMORY_CACHE
    }

    async fn evict(&self, connector_name: &str) {
        let mut guard = self.partitions.lock().unwrap_or_else(|e| e.into_inner());
        guard.retain(|k, _| k.connection_id != connector_name);
        orion_common::metrics::set_memory_cache_partitions(guard.len());
    }

    async fn evict_all(&self) {
        let mut guard = self.partitions.lock().unwrap_or_else(|e| e.into_inner());
        guard.clear();
        orion_common::metrics::set_memory_cache_partitions(0);
    }
}

/// Community foundation in-memory cache connector provider (`org.orion.memory-cache`).
pub struct MemoryCacheProviderBackend {
    store: Arc<PartitionedMemoryCache>,
}

impl MemoryCacheProviderBackend {
    pub fn new(max_entries_per_partition: usize) -> Self {
        Self {
            store: Arc::new(PartitionedMemoryCache::new(max_entries_per_partition)),
        }
    }

    pub fn with_limits(
        max_entries_per_partition: usize,
        max_partitions: usize,
        max_value_bytes: usize,
    ) -> Self {
        Self {
            store: Arc::new(PartitionedMemoryCache::with_limits(
                max_entries_per_partition,
                max_partitions,
                max_value_bytes,
            )),
        }
    }

    pub fn store(&self) -> Arc<PartitionedMemoryCache> {
        self.store.clone()
    }
}

impl Default for MemoryCacheProviderBackend {
    fn default() -> Self {
        Self::new(1024)
    }
}

#[async_trait::async_trait]
impl ProviderBackend for MemoryCacheProviderBackend {
    fn kind(&self) -> ImplementationKind {
        ImplementationKind::Rust
    }

    fn driver_id(&self) -> &str {
        DRIVER_MEMORY_CACHE
    }

    fn validate_config(&self, config: &Value) -> Result<(), ConnectorError> {
        if let Some(backend) = config.get("backend").and_then(Value::as_str)
            && !backend.eq_ignore_ascii_case("memory")
        {
            return Err(ConnectorError::invalid_input(
                format!(
                    "org.orion.memory-cache only supports backend 'memory' (got '{backend}'; \
                     use Enterprise provider 'com.orion.enterprise.redis' for Redis)"
                ),
                Some(json!({"pointer": "/backend"})),
            ));
        }
        Ok(())
    }

    async fn invoke(
        &self,
        ctx: &AttemptContext,
        operation: &str,
        input: Value,
    ) -> Result<ConnectorOutput, ConnectorError> {
        ctx.check_live()?;
        let identity = ctx.effective_identity();

        let Some(key) = input.get("key").and_then(Value::as_str) else {
            return Err(ConnectorError::invalid_input(
                "Memory cache operation requires a string 'key'",
                Some(json!({"pointer": "/key"})),
            ));
        };

        match operation {
            "get" | "cache_read" | "cache.get" => {
                let val = self.store.get(identity, key);
                Ok(ConnectorOutput::new(json!({
                    "hit": val.is_some(),
                    "value": val.unwrap_or(Value::Null),
                })))
            }
            "set" | "cache_write" | "cache.set" => {
                let value = input.get("value").cloned().unwrap_or(Value::Null);
                let approx_bytes = crate::connector::sql_decode::approx_json_value_bytes(&value);
                if approx_bytes > self.store.max_value_bytes() {
                    return Err(ConnectorError::invalid_input(
                        format!(
                            "Memory cache value size ({approx_bytes} bytes) exceeds maximum allowed ({} bytes)",
                            self.store.max_value_bytes()
                        ),
                        Some(json!({"pointer": "/value"})),
                    ));
                }
                let ttl = if let Some(raw_ms) = input.get("ttl_ms") {
                    if raw_ms.is_null() {
                        None
                    } else {
                        let ms = raw_ms.as_u64().filter(|&v| v > 0).ok_or_else(|| {
                            ConnectorError::invalid_input(
                                "Memory cache 'ttl_ms' must be a positive integer",
                                Some(json!({"pointer": "/ttl_ms"})),
                            )
                        })?;
                        Some(Duration::from_millis(ms))
                    }
                } else if let Some(raw_secs) = input.get("ttl_secs") {
                    if raw_secs.is_null() {
                        None
                    } else {
                        let secs = raw_secs.as_u64().filter(|&v| v > 0).ok_or_else(|| {
                            ConnectorError::invalid_input(
                                "Memory cache 'ttl_secs' must be a positive integer",
                                Some(json!({"pointer": "/ttl_secs"})),
                            )
                        })?;
                        Some(Duration::from_secs(secs))
                    }
                } else {
                    None
                };
                self.store.set(identity, key.to_string(), value, ttl);
                Ok(ConnectorOutput::new(json!({
                    "stored": true,
                    "key": key,
                })))
            }
            "delete" | "cache.delete" => {
                let deleted = self.store.delete(identity, key);
                Ok(ConnectorOutput::new(json!({
                    "deleted": deleted,
                    "key": key,
                })))
            }
            other => Err(ConnectorError::forbidden(
                codes::OPERATION_FORBIDDEN,
                "dispatch",
                format!("Unsupported memory-cache operation '{other}'"),
                ctx.effect_status(),
            )),
        }
    }

    async fn dispose(&self, identity: &EffectiveConnectionIdentity) {
        self.store.remove_partition(identity);
    }
}
