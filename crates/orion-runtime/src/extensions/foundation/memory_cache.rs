use orion_common::sync::LockExt;
use std::collections::{BTreeMap, VecDeque};
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
    partitions: Mutex<BTreeMap<EffectiveConnectionIdentity, Partition>>,
}

/// One identity's entries, with a FIFO index so eviction is O(1) rather than
/// a scan of every entry (P-26). The index may hold keys already removed;
/// eviction skips them lazily.
#[derive(Default)]
struct Partition {
    entries: BTreeMap<String, CacheEntry>,
    order: VecDeque<String>,
}

impl Partition {
    fn len(&self) -> usize {
        self.entries.len()
    }
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    fn contains_key(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }
    fn get(&self, key: &str) -> Option<&CacheEntry> {
        self.entries.get(key)
    }
    fn remove(&mut self, key: &str) -> Option<CacheEntry> {
        self.entries.remove(key)
    }
    fn insert(&mut self, key: String, entry: CacheEntry) {
        if self.entries.insert(key.clone(), entry).is_none() {
            self.order.push_back(key);
        }
    }
    fn retain(&mut self, mut f: impl FnMut(&String, &CacheEntry) -> bool) {
        self.entries.retain(|k, v| f(k, v));
        let entries = &self.entries;
        self.order.retain(|k| entries.contains_key(k));
    }
    /// The oldest live key, dropping stale index entries on the way.
    fn pop_oldest(&mut self) -> Option<String> {
        while let Some(front) = self.order.front() {
            if self.entries.contains_key(front) {
                let front = front.clone();
                self.order.pop_front();
                return Some(front);
            }
            self.order.pop_front();
        }
        None
    }
    /// The oldest insertion sequence still stored, for partition eviction.
    fn oldest_seq(&self) -> Option<u64> {
        self.entries.values().map(|e| e.inserted_seq).min()
    }
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
        let mut guard = self.partitions.lock_unpoisoned();
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
        let mut guard = self.partitions.lock_unpoisoned();
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
                    .min_by_key(|(_, part)| part.oldest_seq().unwrap_or(0))
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
            && let Some(oldest_key) = partition.pop_oldest()
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
        let mut guard = self.partitions.lock_unpoisoned();
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
        let mut guard = self.partitions.lock_unpoisoned();
        guard.remove(identity);
        orion_common::metrics::set_memory_cache_partitions(guard.len());
    }

    #[cfg(test)]
    pub fn partition_count(&self) -> usize {
        self.partitions.lock().map(|g| g.len()).unwrap_or(0)
    }

    /// When `key` expires: `None` when it is not stored, `Some(None)` when it
    /// never expires.
    #[cfg(test)]
    pub fn expires_at(
        &self,
        identity: &EffectiveConnectionIdentity,
        key: &str,
    ) -> Option<Option<Instant>> {
        let guard = self.partitions.lock_unpoisoned();
        guard.get(identity)?.get(key).map(|e| e.expires_at)
    }
}

#[async_trait::async_trait]
impl ConnectionPoolCache for PartitionedMemoryCache {
    fn cache_id(&self) -> &str {
        DRIVER_MEMORY_CACHE
    }

    async fn evict(&self, connector_name: &str) {
        let mut guard = self.partitions.lock_unpoisoned();
        guard.retain(|k, _| k.connection_id != connector_name);
        orion_common::metrics::set_memory_cache_partitions(guard.len());
    }
}

/// A `set` TTL field: absent or `null` is no TTL from it; anything else
/// must be a positive integer.
fn positive(input: &Value, field: &str) -> Result<Option<u64>, ConnectorError> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(raw) => raw.as_u64().filter(|&v| v > 0).map(Some).ok_or_else(|| {
            ConnectorError::invalid_input(
                format!("Memory cache '{field}' must be a positive integer"),
                Some(json!({"pointer": format!("/{field}")})),
            )
        }),
    }
}

/// Community foundation in-memory cache connector provider (`org.orion.memory-cache`).
///
/// `set` takes an optional TTL as `ttl_ms` or `ttl_secs`. A field that is
/// absent or `null` sets nothing, so `{"ttl_ms": null, "ttl_secs": 30}`
/// expires in 30 seconds; both non-null is `invalid_input`, and neither is
/// an entry that does not expire.
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

    fn describe(&self) -> Option<orion_extension_api::ProviderDescription> {
        Some(super::memory_cache_description())
    }

    fn validate_config(&self, config: &Value) -> Result<(), ConnectorError> {
        if let Some(backend) = config.get("backend").and_then(Value::as_str)
            && !backend.eq_ignore_ascii_case("memory")
        {
            return Err(ConnectorError::invalid_input(
                format!("org.orion.memory-cache only supports backend 'memory' (got '{backend}')"),
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
                let ttl = match (positive(&input, "ttl_ms")?, positive(&input, "ttl_secs")?) {
                    (Some(_), Some(_)) => {
                        return Err(ConnectorError::invalid_input(
                            "Memory cache 'set' takes 'ttl_ms' or 'ttl_secs', not both",
                            Some(json!({"pointer": "/ttl_ms", "conflicts_with": "/ttl_secs"})),
                        ));
                    }
                    (Some(ms), None) => Some(Duration::from_millis(ms)),
                    (None, Some(secs)) => Some(Duration::from_secs(secs)),
                    (None, None) => None,
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
