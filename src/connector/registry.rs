use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::RwLock;

use crate::errors::OrionError;
use crate::storage::repositories::connectors::ConnectorRepository;

use super::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use super::config::ConnectorConfig;

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
    load_issues: RwLock<Vec<ConnectorLoadIssue>>,
}

/// An enabled connector that could not be loaded into the registry (F16).
///
/// A connector that fails to load does not fail anything at load time — it is
/// simply absent, so every workflow using it returns a 500 at request time,
/// possibly hours later. Recording the failures makes the degraded set
/// visible on `/health` and on `GET /api/v1/admin/connectors` instead of
/// leaving a log line as the only evidence.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConnectorLoadIssue {
    pub connector: String,
    pub connector_id: String,
    /// Which step failed: `env_substitution`, `json_parse`,
    /// `secret_resolution` or `deserialize`. A bounded set, so it is safe as
    /// a metric or log label.
    pub stage: &'static str,
    pub reason: String,
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
            load_issues: RwLock::new(Vec::new()),
        }
    }

    /// The connectors that failed to load on the most recent load (F16).
    /// Empty when every enabled connector loaded.
    pub async fn load_issues(&self) -> Vec<ConnectorLoadIssue> {
        self.load_issues.read().await.clone()
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

        let max = self.cb_config.max_breakers;

        // N18: this warning used to be nested inside `len() >= max`, which
        // implies it, so it fired on every eviction and never in advance —
        // noise at exactly the moment signal was wanted.
        if breakers.len() >= max.saturating_mul(9) / 10 && breakers.len() < max {
            tracing::warn!(
                count = breakers.len(),
                max = max,
                "Circuit breaker map approaching capacity limit"
            );
        }

        if breakers.len() >= max {
            // N19: prefer a CLOSED victim. An OPEN breaker is by definition one
            // that is *not* receiving traffic — `check()` rejects its requests —
            // which makes it the natural LRU victim, and evicting it recreates a
            // CLOSED breaker that re-admits the full load to a dependency still
            // known to be broken. Fall back to plain LRU only when every entry
            // is tripped, which is itself worth a log line.
            let victim = breakers
                .iter()
                .filter(|(_, e)| e.breaker.state_name() == "closed")
                .min_by_key(|(_, e)| e.last_access.load(Ordering::Relaxed))
                .map(|(k, _)| k.clone());

            let victim = match victim {
                Some(k) => Some(k),
                None => {
                    tracing::warn!(
                        max = max,
                        "Circuit breaker map is full and every breaker is tripped; \
                         evicting an open one, which will re-admit load to a broken dependency"
                    );
                    breakers
                        .iter()
                        .min_by_key(|(_, e)| e.last_access.load(Ordering::Relaxed))
                        .map(|(k, _)| k.clone())
                }
            };

            if let Some(key) = victim {
                breakers.remove(&key);
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
    ///
    /// Connectors that fail to load are skipped, as before, but are now also
    /// recorded as [`ConnectorLoadIssue`]s so the degraded set is reportable
    /// (F16) rather than existing only as a log line.
    pub async fn load_from_repo(
        &self,
        repo: &dyn ConnectorRepository,
    ) -> Result<usize, OrionError> {
        let connectors = repo.list_enabled().await?;

        // Build new map outside the lock to avoid holding it during deserialization
        let mut new_configs = HashMap::new();
        let mut issues: Vec<ConnectorLoadIssue> = Vec::new();
        for connector in &connectors {
            // Resolve ${VAR} / ${VAR:-default} placeholders against the process
            // environment so connector configs can reference secrets without
            // storing them in the database. Substitution failures (missing
            // required var, malformed syntax) skip the connector and log —
            // matching how an unparseable config_json is handled below.
            // `storage` was accepted, validated, persisted and listed for the
            // whole 0.x line with no handler behind it (proposal F15), so any
            // workflow referencing one failed at request time. It is removed in
            // 1.0. Stored rows would otherwise surface as a bare serde "unknown
            // variant `storage`", which says nothing about what to do; name the
            // removal and the remedy instead.
            if connector.connector_type == "storage" {
                tracing::error!(
                    connector_id = %connector.id,
                    connector_name = %connector.name,
                    "Connector type 'storage' was removed in 1.0; it never had a handler. \
                     Delete this connector, or disable it, to clear this issue."
                );
                issues.push(ConnectorLoadIssue {
                    connector: connector.name.clone(),
                    connector_id: connector.id.clone(),
                    stage: "removed_type",
                    reason: "connector type 'storage' was removed in 1.0 (it never had a \
                             handler); delete or disable this connector"
                        .to_string(),
                });
                continue;
            }
            let source_label = format!("connector '{}' config_json", connector.name);
            let resolved = match crate::config::env_substitute::substitute(
                &connector.config_json,
                &source_label,
            ) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        connector_id = %connector.id,
                        connector_name = %connector.name,
                        error = %e,
                        "Failed to resolve env vars in connector config, skipping"
                    );
                    issues.push(ConnectorLoadIssue {
                        connector: connector.name.clone(),
                        connector_id: connector.id.clone(),
                        stage: "env_substitution",
                        reason: e.to_string(),
                    });
                    continue;
                }
            };
            // Parse to Value, walk and resolve any `scheme://reference`
            // secret references (B5), then deserialize into the typed
            // `ConnectorConfig`. Errors at this stage skip the connector
            // and warn — matching how unparseable config_json is handled.
            let mut value: serde_json::Value = match serde_json::from_str(&resolved) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        connector_id = %connector.id,
                        connector_name = %connector.name,
                        error = %e,
                        "Failed to parse connector config JSON, skipping"
                    );
                    issues.push(ConnectorLoadIssue {
                        connector: connector.name.clone(),
                        connector_id: connector.id.clone(),
                        stage: "json_parse",
                        reason: e.to_string(),
                    });
                    continue;
                }
            };
            let resolvers = super::secrets::default_resolvers();
            if let Err(e) = super::secrets::resolve_in_place(&mut value, &resolvers, &source_label)
            {
                // Logged at ERROR, not WARN: an unresolved secret means the
                // connector is absent at request time with no other signal
                // (making the degraded set visible on /health is F16).
                tracing::error!(
                    connector_id = %connector.id,
                    connector_name = %connector.name,
                    error = %e,
                    "Failed to resolve secret reference in connector config, skipping"
                );
                issues.push(ConnectorLoadIssue {
                    connector: connector.name.clone(),
                    connector_id: connector.id.clone(),
                    stage: "secret_resolution",
                    reason: e.to_string(),
                });
                continue;
            }
            // `ConnectorConfig` is internally tagged on `type`, but the type
            // lives in its own column and the create/update API takes it as
            // `connector_type` alongside the config. Inject it, exactly as
            // `validate_connector_config` does, so the column is the single
            // source of truth.
            //
            // Without this, a connector authored the documented way — with no
            // redundant `"type"` inside `config` — failed to deserialize with
            // "missing field `type`" and silently never loaded, which is the
            // shape every example and every admin UI produces.
            if let Some(obj) = value.as_object_mut() {
                obj.insert(
                    "type".to_string(),
                    serde_json::Value::String(connector.connector_type.clone()),
                );
            }
            match serde_json::from_value::<ConnectorConfig>(value) {
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
                    issues.push(ConnectorLoadIssue {
                        connector: connector.name.clone(),
                        connector_id: connector.id.clone(),
                        stage: "deserialize",
                        reason: e.to_string(),
                    });
                }
            }
        }

        // Minimal write lock — just swap
        let count = new_configs.len();
        *self.configs.write().await = new_configs;
        if !issues.is_empty() {
            tracing::error!(
                degraded = issues.len(),
                loaded = count,
                "Some enabled connectors failed to load; every workflow using \
                 one will fail at request time. See /health and \
                 GET /api/v1/admin/connectors for the list."
            );
        }
        *self.load_issues.write().await = issues;
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
        assert_eq!(states.get("key1").expect("test"), "closed");
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
