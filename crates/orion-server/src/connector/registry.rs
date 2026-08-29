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

/// Process-wide source of [`ConnectorRegistry::config_generation`] tokens.
/// Drawn on construction and on every load that *changed* the connector set,
/// so a token is unique across registry *instances* as well as across the
/// changes of one instance.
static CONNECTOR_GENERATION: AtomicU64 = AtomicU64::new(0);

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
    /// See [`ConnectorRegistry::config_generation`].
    generation: AtomicU64,
    /// Managed OAuth2 token lifecycle (#268) — per-connector runtime state
    /// living beside the circuit breakers, keyed off the same identity.
    oauth: super::oauth::OAuthTokenManager,
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
            generation: AtomicU64::new(CONNECTOR_GENERATION.fetch_add(1, Ordering::Relaxed)),
            oauth: super::oauth::OAuthTokenManager::new(),
        }
    }

    /// The managed-OAuth2 token manager (#268). Inert until
    /// [`super::oauth::OAuthTokenManager::init`] runs in bootstrap; a bare
    /// registry (unit tests) refuses oauth2 with a clear error rather than
    /// panicking.
    pub fn oauth(&self) -> &super::oauth::OAuthTokenManager {
        &self.oauth
    }

    /// A token identifying *this registry instance and the connector set it
    /// currently holds*. It is drawn on construction, and again on any load
    /// that changed the set; two equal tokens mean the same instance holding
    /// configs that compare equal, and no two instances ever share one.
    ///
    /// N17: [`crate::channel::ChannelRegistry`] caches a whole
    /// `ChannelRuntimeConfig` across reloads, and that struct embeds dedup and
    /// response-cache backends resolved *through this registry*. The cache is
    /// only sound while the connector set behind those backends is unchanged,
    /// so this token is part of its key. Every production path that changes a
    /// connector — create, update, delete, import, and the epoch-driven
    /// resync — goes through [`Self::reload`], which is what keeps the token
    /// honest; a future path that mutates `configs` without loading would have
    /// to advance it too.
    ///
    /// "On change" rather than "on load" still matters on a remote node.
    /// `resync_from_db` no longer reloads connectors on every epoch tick — the
    /// bump carries an [`crate::cluster::EpochScope`] and a definitions-only
    /// change skips the connector half entirely — but a connector-scoped bump,
    /// an unknown scope, and every bump from a pre-scope node all still reload
    /// here. A token that moved per load would make each of those rebuild
    /// every channel's runtime on every node, which is exactly the cost N17
    /// exists to remove.
    pub fn config_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
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
                .filter(|(_, e)| e.breaker.is_closed())
                .min_by_key(|(_, e)| e.last_access.load(Ordering::Relaxed))
                .map(|(k, _)| k.clone())
                .or_else(|| {
                    tracing::warn!(
                        max = max,
                        "Circuit breaker map is full and every breaker is tripped; \
                         evicting an open one, which will re-admit load to a broken dependency"
                    );
                    breakers
                        .iter()
                        .min_by_key(|(_, e)| e.last_access.load(Ordering::Relaxed))
                        .map(|(k, _)| k.clone())
                });

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
    /// recorded as `ConnectorLoadIssue`s so the degraded set is reportable
    /// (F16) rather than existing only as a log line.
    ///
    /// A load that produces the same connector set leaves both the stored map
    /// and [`Self::config_generation`] untouched — see that method for why the
    /// distinction between "loaded" and "changed" matters.
    pub async fn load_from_repo(
        &self,
        repo: &dyn ConnectorRepository,
    ) -> Result<usize, OrionError> {
        let connectors = repo.list_enabled().await?;

        // Build new map outside the lock to avoid holding it during deserialization
        let mut new_configs = HashMap::new();
        let mut issues: Vec<ConnectorLoadIssue> = Vec::new();
        // N23: the resolver set is connector-independent, and now a process
        // singleton — borrowed here so the loop below is not re-deciding it
        // per connector.
        let resolvers = super::secrets::default_resolvers();
        for connector in &connectors {
            // Every issue below carries this connector's identity; only the
            // stage and the reason differ.
            let issue = |stage: &'static str, reason: String| ConnectorLoadIssue {
                connector: connector.name.clone(),
                connector_id: connector.id.clone(),
                stage,
                reason,
            };
            // Resolve ${VAR} / ${VAR:-default} placeholders against the process
            // environment so connector configs can reference secrets without
            // storing them in the database. Substitution failures (missing
            // required var, malformed syntax) skip the connector and log —
            // matching how an unparseable config_json is handled below.
            // `storage` spent the 0.x line as an accepted type with no handler
            // (F15) and was removed in 1.0; #265 reinstates the name *with* a
            // handler and a real config shape. A stored 0.x row now parses
            // against that shape like any other connector — and reports a
            // config-parse load issue if it doesn't fit, which names exactly
            // what to fix.
            let source_label = format!("connector '{}' config_json", connector.name);
            // §3.7: `${VAR}` in a *stored* connector config is deprecated. It
            // predates `env://` and overlaps it, but resolves at a different
            // layer — textually, before the JSON is parsed — so it can inject
            // structure rather than a value, it is invisible to the masking
            // policy (`is_resolvable_reference` sees `${VAR}` as an ordinary
            // string, so an export carries the placeholder while a real
            // credential would be masked), and it is unreachable from the
            // offline surfaces, which have no process environment to read.
            // `env://` and `vault://` have none of those properties. Warned
            // here rather than refused: a stored connector cannot be edited by
            // an upgrade, and expand/contract gives the operator a release to
            // move before the placeholder stops resolving.
            let placeholders =
                crate::config::env_substitute::referenced_vars(&connector.config_json);
            if !placeholders.is_empty() {
                tracing::warn!(
                    connector_id = %connector.id,
                    connector_name = %connector.name,
                    variables = %placeholders.iter().cloned().collect::<Vec<_>>().join(", "),
                    "DEPRECATED: this connector's config uses ${{VAR}} placeholders. \
                     Replace each with an env:// reference (\"env://VAR\") — ${{VAR}} in \
                     stored connector configs will stop resolving in a future release"
                );
            }
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
                    issues.push(issue("env_substitution", e.to_string()));
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
                    issues.push(issue("json_parse", e.to_string()));
                    continue;
                }
            };
            if let Err(e) =
                super::secrets::resolve_in_place(&mut value, resolvers, &source_label).await
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
                issues.push(issue("secret_resolution", e.to_string()));
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
                    issues.push(issue("deserialize", e.to_string()));
                }
            }
        }

        // Minimal write lock — compare, then swap only if this load actually
        // changed something.
        //
        // The comparison is inside the lock, not before it: the token is a
        // read-modify-write of the config map, and two concurrent loads that
        // both compared against the same outgoing map could each conclude
        // "unchanged" while the map they wrote differed from it.
        let count = new_configs.len();
        let changed = {
            let mut configs = self.configs.write().await;
            let changed = *configs != new_configs;
            if changed {
                *configs = new_configs;
            }
            changed
        };
        // N17: the token is [`crate::channel::ChannelRegistry`]'s cache key,
        // so it must move when the effective connector set moves and stay put
        // when it does not. Advancing it on every *load* rather than every
        // *change* would be sound but useless: a connector-scoped resync (and
        // every bump from a node too old to write a scope) reloads connectors
        // on every remote node, so the token would invalidate every channel's
        // cached runtime on all of them — exactly the cost N17 exists to
        // remove.
        //
        // Advanced *after* the swap, so anything that reads the token and
        // then the configs cannot pair a new token with old configs.
        if changed {
            self.generation.store(
                CONNECTOR_GENERATION.fetch_add(1, Ordering::Relaxed),
                Ordering::Release,
            );
        }
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

    /// Insert an already-parsed connector under `name`.
    ///
    /// Test-only: production loads go through [`Self::load_from_repo`], which
    /// does `${VAR}` substitution and `env://` secret resolution first.
    #[cfg(test)]
    pub(crate) async fn insert_for_test(&self, name: &str, config: ConnectorConfig) {
        self.configs
            .write()
            .await
            .insert(name.to_string(), Arc::new(config));
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

/// A repository that returns whatever connector rows a test hands it.
///
/// Lives outside `mod tests` because the channel registry's tests need it too:
/// its per-channel cache is keyed on
/// [`ConnectorRegistry::config_generation`], and the thing worth pinning is
/// what happens to that key when a *load* of an unchanged set runs — which
/// takes a repository.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::storage::models::Connector;
    use crate::storage::repositories::connectors::{
        ConnectorFilter, CreateConnectorRequest, UpdateConnectorRequest,
    };
    use crate::storage::repositories::helpers::PaginatedResult;

    pub(crate) struct StubConnectorRepo {
        rows: std::sync::Mutex<Vec<Connector>>,
        /// In-memory `connector_oauth_state` (#268): name → (fingerprint,
        /// state_json). Functional rather than `unreachable!` because the
        /// token-manager tests exercise rotation persistence through it.
        oauth_state: std::sync::Mutex<std::collections::HashMap<String, (String, String)>>,
    }

    impl StubConnectorRepo {
        /// Rows as `(name, connector_type, config_json)`.
        pub(crate) fn with(rows: Vec<(&str, &str, &str)>) -> Self {
            Self {
                rows: std::sync::Mutex::new(rows.into_iter().map(row).collect()),
                oauth_state: std::sync::Mutex::new(std::collections::HashMap::new()),
            }
        }

        pub(crate) fn set(&self, rows: Vec<(&str, &str, &str)>) {
            *self.rows.lock().expect("test") = rows.into_iter().map(row).collect();
        }
    }

    fn row((name, connector_type, config_json): (&str, &str, &str)) -> Connector {
        let now = chrono::Utc::now().naive_utc();
        Connector {
            tags_json: "[]".to_string(),
            id: format!("con_{name}"),
            name: name.to_string(),
            connector_type: connector_type.to_string(),
            config_json: config_json.to_string(),
            enabled: true,
            created_at: now,
            updated_at: now,
        }
    }

    #[async_trait::async_trait]
    impl ConnectorRepository for StubConnectorRepo {
        async fn list_enabled(&self) -> Result<Vec<Connector>, OrionError> {
            Ok(self.rows.lock().expect("test").clone())
        }
        async fn create(&self, _req: &CreateConnectorRequest) -> Result<Connector, OrionError> {
            unreachable!("not used by load_from_repo")
        }
        async fn get_by_id(&self, _id: &str) -> Result<Connector, OrionError> {
            unreachable!("not used by load_from_repo")
        }
        async fn list_paginated(
            &self,
            _filter: &ConnectorFilter,
        ) -> Result<PaginatedResult<Connector>, OrionError> {
            unreachable!("not used by load_from_repo")
        }
        async fn update(
            &self,
            _id: &str,
            _req: &UpdateConnectorRequest,
        ) -> Result<Connector, OrionError> {
            unreachable!("not used by load_from_repo")
        }
        async fn delete(&self, _id: &str) -> Result<(), OrionError> {
            unreachable!("not used by load_from_repo")
        }
        async fn exists_by_name(&self, _name: &str) -> Result<bool, OrionError> {
            unreachable!("not used by load_from_repo")
        }
        async fn get_by_name(&self, _name: &str) -> Result<Connector, OrionError> {
            unreachable!("not used by load_from_repo")
        }
        async fn snapshot(&self, _filter: &ConnectorFilter) -> Result<Vec<Connector>, OrionError> {
            unreachable!("not used by load_from_repo")
        }
        async fn get_oauth_state(
            &self,
            connector_name: &str,
        ) -> Result<Option<crate::storage::models::ConnectorOauthStateRow>, OrionError> {
            Ok(self
                .oauth_state
                .lock()
                .expect("test")
                .get(connector_name)
                .map(
                    |(fingerprint, state_json)| crate::storage::models::ConnectorOauthStateRow {
                        fingerprint: fingerprint.clone(),
                        state_json: state_json.clone(),
                    },
                ))
        }
        async fn put_oauth_state(
            &self,
            connector_name: &str,
            fingerprint: &str,
            state_json: &str,
        ) -> Result<(), OrionError> {
            self.oauth_state.lock().expect("test").insert(
                connector_name.to_string(),
                (fingerprint.to_string(), state_json.to_string()),
            );
            Ok(())
        }
        async fn delete_oauth_state(&self, connector_name: &str) -> Result<(), OrionError> {
            self.oauth_state
                .lock()
                .expect("test")
                .remove(connector_name);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::StubConnectorRepo;
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

    // ---- N17: the generation token tracks changes, not loads --------------

    /// The one that matters in a cluster. A connector-scoped resync reloads
    /// the connector registry on every remote node, and so does every bump
    /// from a node too old to write a scope — so if the token moved per load,
    /// each of those would rebuild every channel's runtime config on every
    /// node, and N17's whole saving would be confined to the node that made
    /// the change.
    #[tokio::test]
    async fn reloading_an_unchanged_connector_set_does_not_move_the_token() {
        let repo = StubConnectorRepo::with(vec![(
            "cache-1",
            "cache",
            r#"{"backend":"redis","url":"redis://localhost:6379"}"#,
        )]);
        let registry = ConnectorRegistry::default();
        assert_eq!(registry.reload(&repo).await.expect("loads"), 1);
        let after_first = registry.config_generation();

        for _ in 0..5 {
            assert_eq!(registry.reload(&repo).await.expect("loads"), 1);
            assert_eq!(
                registry.config_generation(),
                after_first,
                "an epoch resync that changed no connector must leave the \
                 channel registry's cache key alone"
            );
        }
    }

    /// The counterpart: a real edit has to move it, or a channel could stay
    /// pinned to a dedup/response-cache backend the operator has replaced.
    #[tokio::test]
    async fn changing_a_connector_moves_the_token() {
        let repo = StubConnectorRepo::with(vec![(
            "cache-1",
            "cache",
            r#"{"backend":"redis","url":"redis://old:6379"}"#,
        )]);
        let registry = ConnectorRegistry::default();
        registry.reload(&repo).await.expect("loads");
        let before = registry.config_generation();

        // Same connector, new URL — the case the token exists for.
        repo.set(vec![(
            "cache-1",
            "cache",
            r#"{"backend":"redis","url":"redis://new:6379"}"#,
        )]);
        registry.reload(&repo).await.expect("loads");
        let after_edit = registry.config_generation();
        assert_ne!(
            before, after_edit,
            "an edited connector must move the token"
        );

        // Adding one moves it too.
        repo.set(vec![
            (
                "cache-1",
                "cache",
                r#"{"backend":"redis","url":"redis://new:6379"}"#,
            ),
            ("cache-2", "cache", r#"{"backend":"memory"}"#),
        ]);
        registry.reload(&repo).await.expect("loads");
        let after_add = registry.config_generation();
        assert_ne!(after_edit, after_add, "a new connector must move the token");

        // And so does removing one.
        repo.set(vec![(
            "cache-1",
            "cache",
            r#"{"backend":"redis","url":"redis://new:6379"}"#,
        )]);
        registry.reload(&repo).await.expect("loads");
        assert_ne!(
            after_add,
            registry.config_generation(),
            "a deleted connector must move the token"
        );
    }

    /// A load that changes nothing must not disturb the stored `Arc`s either:
    /// holders keep the exact config they had, which is what makes "the token
    /// did not move" mean "nothing behind it moved".
    #[tokio::test]
    async fn an_unchanged_reload_keeps_the_stored_config_arcs() {
        let repo =
            StubConnectorRepo::with(vec![("http-1", "http", r#"{"url":"https://example.com"}"#)]);
        let registry = ConnectorRegistry::default();
        registry.reload(&repo).await.expect("loads");
        let first = registry.get("http-1").await.expect("loaded");

        registry.reload(&repo).await.expect("loads");
        let second = registry.get("http-1").await.expect("loaded");
        assert!(Arc::ptr_eq(&first, &second));
    }

    /// Two registry instances holding identical configs must still be
    /// distinguishable: the token names the instance as well as its contents,
    /// because a `ChannelRuntimeConfig` cached against one registry says
    /// nothing about backends resolved through another.
    #[tokio::test]
    async fn identical_configs_in_two_registries_do_not_share_a_token() {
        let repo =
            StubConnectorRepo::with(vec![("http-1", "http", r#"{"url":"https://example.com"}"#)]);
        let a = ConnectorRegistry::default();
        let b = ConnectorRegistry::default();
        a.reload(&repo).await.expect("loads");
        b.reload(&repo).await.expect("loads");
        assert_ne!(a.config_generation(), b.config_generation());
    }
}
