use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::RwLock;

use crate::errors::OrionError;
use crate::storage::repositories::connectors::ConnectorRepository;

use super::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use super::types::ConnectorConfig;

/// Monotonic counter for LRU tracking of circuit breaker access.
static BREAKER_ACCESS_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A circuit breaker entry with LRU tracking.
struct BreakerEntry {
    breaker: Arc<CircuitBreaker>,
    last_access: AtomicU64,
}

impl BreakerEntry {
    fn new(breaker: Arc<CircuitBreaker>) -> Self {
        Self {
            breaker,
            last_access: AtomicU64::new(BREAKER_ACCESS_COUNTER.fetch_add(1, Ordering::Relaxed)),
        }
    }

    fn touch(&self) {
        self.last_access.store(
            BREAKER_ACCESS_COUNTER.fetch_add(1, Ordering::Relaxed),
            Ordering::Relaxed,
        );
    }
}

/// In-memory registry for active connector configurations.
pub struct ConnectorRegistry {
    configs: RwLock<HashMap<String, Arc<ConnectorConfig>>>,
    circuit_breakers: RwLock<HashMap<String, BreakerEntry>>,
    cb_config: CircuitBreakerConfig,
}

impl Default for ConnectorRegistry {
    fn default() -> Self {
        Self::new(CircuitBreakerConfig::default())
    }
}

impl ConnectorRegistry {
    pub fn new(cb_config: CircuitBreakerConfig) -> Self {
        Self {
            configs: RwLock::new(HashMap::new()),
            circuit_breakers: RwLock::new(HashMap::new()),
            cb_config,
        }
    }

    /// Get or create a circuit breaker for the given key (e.g. "channel:connector").
    pub async fn get_or_create_breaker(&self, key: &str) -> Arc<CircuitBreaker> {
        // Fast path: read lock
        {
            let breakers = self.circuit_breakers.read().await;
            if let Some(entry) = breakers.get(key) {
                entry.touch();
                return entry.breaker.clone();
            }
        }
        // Slow path: write lock on miss
        let mut breakers = self.circuit_breakers.write().await;
        // Double-check after acquiring write lock
        if let Some(entry) = breakers.get(key) {
            entry.touch();
            return entry.breaker.clone();
        }

        // Evict LRU entry if at capacity
        let max = self.cb_config.max_breakers;
        if breakers.len() >= max {
            if breakers.len() >= max * 9 / 10 {
                tracing::warn!(
                    count = breakers.len(),
                    max = max,
                    "Circuit breaker map approaching capacity limit"
                );
            }
            // Find the least-recently-used entry
            if let Some(lru_key) = breakers
                .iter()
                .min_by_key(|(_, e)| e.last_access.load(Ordering::Relaxed))
                .map(|(k, _)| k.clone())
            {
                breakers.remove(&lru_key);
            }
        }

        let breaker = Arc::new(CircuitBreaker::new(self.cb_config.clone()));
        let entry = BreakerEntry::new(breaker.clone());
        breakers.insert(key.to_string(), entry);
        breaker
    }

    /// Return all circuit breaker states for admin/health introspection.
    pub async fn circuit_breaker_states(&self) -> HashMap<String, String> {
        let breakers = self.circuit_breakers.read().await;
        breakers
            .iter()
            .map(|(k, v)| (k.clone(), v.breaker.state_name().to_string()))
            .collect()
    }

    /// Force-reset a circuit breaker by key. Returns `true` if the key existed.
    pub async fn reset_circuit_breaker(&self, key: &str) -> bool {
        let breakers = self.circuit_breakers.read().await;
        if let Some(entry) = breakers.get(key) {
            entry.breaker.reset();
            true
        } else {
            false
        }
    }

    /// Whether circuit breakers are enabled.
    pub fn circuit_breaker_enabled(&self) -> bool {
        self.cb_config.enabled
    }

    /// Load all enabled connectors from the repository into the registry.
    pub async fn load_from_repo(
        &self,
        repo: &dyn ConnectorRepository,
    ) -> Result<usize, OrionError> {
        let connectors = repo.list_enabled().await?;

        // Build new map outside the lock to avoid holding it during deserialization
        let mut new_configs = HashMap::new();
        for connector in &connectors {
            match serde_json::from_str::<ConnectorConfig>(&connector.config_json) {
                Ok(config) => {
                    new_configs.insert(connector.name.clone(), Arc::new(config));
                }
                Err(e) => {
                    tracing::warn!(
                        connector_id = %connector.id,
                        connector_name = %connector.name,
                        error = %e,
                        "Failed to parse connector config, skipping"
                    );
                }
            }
        }

        // Minimal write lock — just swap
        let count = new_configs.len();
        *self.configs.write().await = new_configs;
        Ok(count)
    }

    /// Get a connector config by name.
    pub async fn get(&self, name: &str) -> Option<Arc<ConnectorConfig>> {
        self.configs.read().await.get(name).cloned()
    }

    /// Reload all connectors from the repository.
    pub async fn reload(&self, repo: &dyn ConnectorRepository) -> Result<usize, OrionError> {
        self.load_from_repo(repo).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_connector_registry_get_and_set() {
        let registry = ConnectorRegistry::default();
        assert!(registry.get("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn test_connector_registry_circuit_breaker_disabled_by_default() {
        let registry = ConnectorRegistry::default();
        assert!(!registry.circuit_breaker_enabled());
    }

    #[tokio::test]
    async fn test_connector_registry_circuit_breaker_enabled() {
        let config = CircuitBreakerConfig {
            enabled: true,
            failure_threshold: 5,
            recovery_timeout_secs: 30,
            ..Default::default()
        };
        let registry = ConnectorRegistry::new(config);
        assert!(registry.circuit_breaker_enabled());
    }

    #[tokio::test]
    async fn test_connector_registry_get_or_create_breaker() {
        let config = CircuitBreakerConfig {
            enabled: true,
            failure_threshold: 5,
            recovery_timeout_secs: 30,
            ..Default::default()
        };
        let registry = ConnectorRegistry::new(config);
        let b1 = registry.get_or_create_breaker("key1").await;
        let b2 = registry.get_or_create_breaker("key1").await;
        // Should return the same breaker
        assert!(Arc::ptr_eq(&b1, &b2));
    }

    #[tokio::test]
    async fn test_connector_registry_circuit_breaker_states() {
        let config = CircuitBreakerConfig {
            enabled: true,
            failure_threshold: 5,
            recovery_timeout_secs: 30,
            ..Default::default()
        };
        let registry = ConnectorRegistry::new(config);
        let _ = registry.get_or_create_breaker("key1").await;
        let states = registry.circuit_breaker_states().await;
        assert_eq!(states.len(), 1);
        assert_eq!(states.get("key1").unwrap(), "closed");
    }

    #[tokio::test]
    async fn test_connector_registry_reset_circuit_breaker() {
        let config = CircuitBreakerConfig {
            enabled: true,
            failure_threshold: 1,
            recovery_timeout_secs: 300,
            ..Default::default()
        };
        let registry = ConnectorRegistry::new(config);
        let breaker = registry.get_or_create_breaker("key1").await;
        breaker.record_failure(); // trips it
        assert!(!breaker.check()); // open

        let found = registry.reset_circuit_breaker("key1").await;
        assert!(found);
        assert!(breaker.check()); // closed again
    }

    #[tokio::test]
    async fn test_connector_registry_reset_nonexistent_breaker() {
        let registry = ConnectorRegistry::default();
        assert!(!registry.reset_circuit_breaker("nope").await);
    }

    #[tokio::test]
    async fn test_circuit_breaker_bounded_capacity() {
        let config = CircuitBreakerConfig {
            enabled: true,
            failure_threshold: 5,
            recovery_timeout_secs: 30,
            max_breakers: 3,
        };
        let registry = ConnectorRegistry::new(config);

        // Fill to capacity
        let _b1 = registry.get_or_create_breaker("key1").await;
        let _b2 = registry.get_or_create_breaker("key2").await;
        let _b3 = registry.get_or_create_breaker("key3").await;

        // Access key2 and key3 to make key1 the LRU
        let _b2_again = registry.get_or_create_breaker("key2").await;
        let _b3_again = registry.get_or_create_breaker("key3").await;

        // Adding a 4th should evict key1 (LRU)
        let _b4 = registry.get_or_create_breaker("key4").await;

        let states = registry.circuit_breaker_states().await;
        assert_eq!(states.len(), 3);
        assert!(
            !states.contains_key("key1"),
            "key1 should have been evicted as LRU"
        );
        assert!(states.contains_key("key2"));
        assert!(states.contains_key("key3"));
        assert!(states.contains_key("key4"));
    }
}
