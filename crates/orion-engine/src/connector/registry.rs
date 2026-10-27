use arc_swap::ArcSwap;
use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{Mutex, RwLock};

use super::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use super::config::ConnectorConfig;

/// An immutable connector estate: every config by name.
pub type ConnectorSnapshot = Arc<HashMap<String, Arc<ConnectorConfig>>>;

tokio::task_local! {
    /// The connector estate of the generation a run executes on (`TD-07`).
    /// The runtime scopes it around an admitted run whose generation carries
    /// a snapshot; [`ConnectorRegistry::get`] reads it before the live map.
    pub static PINNED_CONNECTORS: ConnectorSnapshot;
}

/// Monotonic counter for LRU tracking of circuit breaker access.
static BREAKER_ACCESS_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Process-wide source of [`ConnectorRegistry::config_generation`] tokens.
/// Drawn on construction and on every load that *changed* the connector set,
/// so a token is unique across registry *instances* as well as across the
/// changes of one instance.
static CONNECTOR_GENERATION: AtomicU64 = AtomicU64::new(0);

trait BreakerKeyLookup {
    fn channel(&self) -> &str;
    fn connector(&self) -> &str;
}

#[derive(Clone, Eq)]
struct BreakerKey {
    channel: String,
    connector: String,
}

#[derive(Clone, Copy)]
struct BreakerKeyRef<'a> {
    channel: &'a str,
    connector: &'a str,
}

impl BreakerKeyRef<'_> {
    fn into_owned(self) -> BreakerKey {
        BreakerKey {
            channel: self.channel.to_string(),
            connector: self.connector.to_string(),
        }
    }
}

impl BreakerKeyLookup for BreakerKey {
    fn channel(&self) -> &str {
        &self.channel
    }

    fn connector(&self) -> &str {
        &self.connector
    }
}

impl BreakerKeyLookup for BreakerKeyRef<'_> {
    fn channel(&self) -> &str {
        self.channel
    }

    fn connector(&self) -> &str {
        self.connector
    }
}

impl PartialEq for BreakerKey {
    fn eq(&self, other: &Self) -> bool {
        self.channel == other.channel && self.connector == other.connector
    }
}

impl Hash for BreakerKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.channel.as_str().hash(state);
        self.connector.as_str().hash(state);
    }
}

impl PartialEq for dyn BreakerKeyLookup + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.channel() == other.channel() && self.connector() == other.connector()
    }
}

impl Eq for dyn BreakerKeyLookup + '_ {}

impl Hash for dyn BreakerKeyLookup + '_ {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.channel().hash(state);
        self.connector().hash(state);
    }
}

impl<'a> Borrow<dyn BreakerKeyLookup + 'a> for BreakerKey {
    fn borrow(&self) -> &(dyn BreakerKeyLookup + 'a) {
        self
    }
}

/// A circuit breaker entry with LRU tracking.
struct BreakerEntry {
    breaker: Arc<CircuitBreaker>,
    last_access: AtomicU64,
    /// The composite `"{channel}:{connector}"` string — the shape the admin API
    /// and the cluster reset broadcast (`job_leases.breaker_key`) both speak.
    /// Keying the map by [`BreakerKey`] avoids allocating this formatted string
    /// on every hot-path lookup while keeping [`ConnectorRegistry::prune_breakers`]
    /// exact even when names contain `:`.
    composite_key: String,
}

impl BreakerEntry {
    fn new(breaker: Arc<CircuitBreaker>, channel: &str, connector: &str) -> Self {
        Self {
            breaker,
            last_access: AtomicU64::new(BREAKER_ACCESS_COUNTER.fetch_add(1, Ordering::Relaxed)),
            composite_key: format!("{channel}:{connector}"),
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
    configs: ArcSwap<HashMap<String, Arc<ConnectorConfig>>>,
    reload_lock: Mutex<()>,
    /// A synchronous lock, not an async one: no critical section awaits, and
    /// the read side is taken on every guarded connector call. An async
    /// `RwLock` made each of those a future and a semaphore acquisition for a
    /// hash lookup that never yields.
    circuit_breakers: std::sync::RwLock<HashMap<BreakerKey, BreakerEntry>>,
    cb_config: CircuitBreakerConfig,
    load_issues: RwLock<Vec<ConnectorLoadIssue>>,
    /// See [`ConnectorRegistry::config_generation`].
    generation: AtomicU64,
    /// The instance's `[vars]`, for `var://` references in a stored config.
    /// `None` on a registry built without them — every reference then fails to
    /// resolve and names the connector, which is the right answer for a
    /// deployment value nobody declared.
    vars: Option<Arc<serde_json::Value>>,
    /// Managed OAuth2 token lifecycle (#268) — per-connector runtime state
    /// living beside the circuit breakers, keyed off the same identity.
    oauth: super::oauth::OAuthTokenManager,
}

/// An enabled connector that could not be loaded into the registry (F16).
///
/// A connector that fails to load does not fail anything at load time — it is
/// simply absent, so every workflow using it returns a 500 at request time,
/// possibly hours later. Recording the failures makes the degraded set
/// visible on `/health` and on `GET /api/v2/admin/connectors` instead of
/// leaving a log line as the only evidence.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConnectorLoadIssue {
    pub connector: String,
    pub connector_id: String,
    /// Which step failed: `env_substitution`, `json_parse`, `var_reference`,
    /// `secret_resolution`, `deserialize` or `endpoint` (a resolved endpoint
    /// whose scheme its backend cannot serve). A bounded set, so it is safe
    /// as a metric or log label.
    pub stage: &'static str,
    pub reason: String,
}

impl Default for ConnectorRegistry {
    fn default() -> Self {
        Self::new(CircuitBreakerConfig::default())
    }
}

impl ConnectorRegistry {
    /// The instance's `[vars]`, so a stored config can reference one with
    /// `var://name`. Builder-style because every test constructs a registry and
    /// almost none of them declares a var.
    pub fn with_vars(mut self, vars: Option<Arc<serde_json::Value>>) -> Self {
        self.vars = vars;
        self
    }

    pub fn new(cb_config: CircuitBreakerConfig) -> Self {
        Self {
            configs: ArcSwap::from_pointee(HashMap::new()),
            reload_lock: Mutex::new(()),
            circuit_breakers: std::sync::RwLock::new(HashMap::new()),
            cb_config,
            load_issues: RwLock::new(Vec::new()),
            generation: AtomicU64::new(CONNECTOR_GENERATION.fetch_add(1, Ordering::Relaxed)),
            oauth: super::oauth::OAuthTokenManager::new(),
            vars: None,
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
    /// N17: `orion_server::channel::ChannelLoader` caches a whole
    /// `ChannelRuntimeConfig` across reloads, and that struct embeds dedup and
    /// response-cache backends resolved *through this registry*. The cache is
    /// only sound while the connector set behind those backends is unchanged,
    /// so this token is part of its key. Every production path that changes
    /// the connector set — a deployment build — goes through [`Self::load`],
    /// which is what keeps the token honest; a future path that mutates
    /// `configs` without loading would have to advance it too.
    ///
    /// "On change" rather than "on load" matters: every deployment build
    /// loads the manifest's Connections, including the many that change no
    /// connection. A token that moved per load would make each of those
    /// rebuild every channel's runtime, which is exactly the cost N17 exists
    /// to remove.
    pub fn config_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// The connectors that failed to load on the most recent load (F16).
    /// Empty when every enabled connector loaded.
    pub async fn load_issues(&self) -> Vec<ConnectorLoadIssue> {
        self.load_issues.read().await.clone()
    }

    /// Get or create the circuit breaker for a `(channel, connector)` pairing.
    ///
    /// The map is keyed by `BreakerKey` so hot-path lookups borrow `(channel, connector)`
    /// without allocating a formatted `String`, while the entry caches the
    /// composite `"{channel}:{connector}"` string for admin API / cluster-reset
    /// compatibility.
    pub async fn get_or_create_breaker(
        &self,
        channel: &str,
        connector: &str,
    ) -> Arc<CircuitBreaker> {
        let lookup = BreakerKeyRef { channel, connector };
        // Fast path: read lock with zero-allocation borrowed lookup
        {
            let breakers = self
                .circuit_breakers
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(entry) = breakers.get(&lookup as &dyn BreakerKeyLookup) {
                entry.touch();
                return entry.breaker.clone();
            }
        }
        // Slow path: write lock on miss
        let mut breakers = self
            .circuit_breakers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Double-check after acquiring write lock
        if let Some(entry) = breakers.get(&lookup as &dyn BreakerKeyLookup) {
            entry.touch();
            return entry.breaker.clone();
        }

        let max = self.cb_config.max_breakers;

        // N18: the warning sits outside the `len() >= max` branch, which
        // implies it: nested inside, it would fire on every eviction and never
        // in advance — noise at exactly the moment signal is wanted.
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
        let entry = BreakerEntry::new(breaker.clone(), channel, connector);
        breakers.insert(lookup.into_owned(), entry);
        breaker
    }

    /// Drop breakers whose channel or connector no longer exists.
    ///
    /// Called once per reload with the live channel set; the live connector
    /// set is this registry's own loaded configs (an enabled, resolvable
    /// connector). Each breaker is keyed by `(channel, connector)` so this
    /// filters directly on the two components without parsing a composite key.
    /// Without it, a deleted connector's or an archived
    /// channel's breaker lingered until LRU pressure at `max_breakers` reclaimed
    /// it, holding a slot a live pairing could use.
    ///
    /// `live_channels` is the set of channel *names* — the routing identity
    /// stamped into `metadata.channel` and composed into the breaker key — not
    /// the `channel_id` storage PK; the entry's `channel` is that same name, so
    /// the caller must pass names or every breaker is dropped.
    ///
    /// A no-op when breakers are disabled (the map is then always empty).
    pub async fn prune_breakers(&self, live_channels: &std::collections::HashSet<&str>) {
        if !self.cb_config.enabled {
            return;
        }
        let live_configs = self.configs.load();
        let mut breakers = self
            .circuit_breakers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        breakers.retain(|key, _| {
            live_configs.contains_key(&key.connector)
                && live_channels.contains(key.channel.as_str())
        });
    }

    /// Return all circuit breaker states for admin/health introspection.
    pub async fn circuit_breaker_states(&self) -> HashMap<String, String> {
        let breakers = self
            .circuit_breakers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        breakers
            .values()
            .map(|v| (v.composite_key.clone(), v.breaker.state_name().to_string()))
            .collect()
    }

    /// Force-reset a circuit breaker by key. Returns `true` if the key existed.
    pub async fn reset_circuit_breaker(&self, key: &str) -> bool {
        let breakers = self
            .circuit_breakers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = breakers.values().find(|e| e.composite_key == key) {
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

    /// Replace the loaded connectors with `connectors` — a deployment's
    /// Connections over built-in drivers — each resolved by
    /// `resolve_connector_config`: references resolved, the typed parse by
    /// type, the endpoint checked. One that does not resolve is a load issue
    /// (F16), never an abort. Answers how many loaded.
    ///
    /// A load that produces the same connector set leaves both the stored map
    /// and [`Self::config_generation`] untouched — see that method for why the
    /// distinction between "loaded" and "changed" matters.
    pub async fn load(&self, connectors: Vec<StoredConnector>) -> usize {
        // Build new map outside the lock to avoid holding it during deserialization
        let mut new_configs = HashMap::new();
        let mut issues: Vec<ConnectorLoadIssue> = Vec::new();
        // N23: the resolver set is connector-independent, and now a process
        // singleton — borrowed here so the loop below is not re-deciding it
        // per connector.
        let resolvers = super::secrets::default_resolvers();
        for connector in &connectors {
            match resolve_connector_config(
                &connector.name,
                &connector.id,
                &connector.connector_type,
                &connector.config_json,
                self.vars.as_deref(),
                resolvers,
            )
            .await
            {
                Ok(config) => {
                    new_configs.insert(connector.name.clone(), Arc::new(config));
                }
                Err(e) => issues.push(ConnectorLoadIssue {
                    connector: connector.name.clone(),
                    connector_id: connector.id.clone(),
                    stage: e.stage,
                    reason: e.reason,
                }),
            }
        }

        // Minimal reload lock — compare, then swap the ArcSwap snapshot only if
        // this load actually changed something.
        //
        // The comparison is inside the lock, not before it: the token is a
        // read-modify-write of the config map, and two concurrent loads that
        // both compared against the same outgoing map could each conclude
        // "unchanged" while the map they wrote differed from it.
        let count = new_configs.len();
        let changed = {
            let _reload_guard = self.reload_lock.lock().await;
            let current = self.configs.load();
            let changed = **current != new_configs;
            if changed {
                self.configs.store(Arc::new(new_configs));
            }
            changed
        };
        // N17: the token is the channel loader's reuse-cache key,
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
                 GET /api/v2/admin/connectors for the list."
            );
        }
        *self.load_issues.write().await = issues;
        count
    }

    /// Insert an already-parsed connector under `name`.
    ///
    /// Test-only: production loads go through [`Self::load`], which does
    /// `${VAR}` substitution and `env://` secret resolution first.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn insert_for_test(&self, name: &str, config: ConnectorConfig) {
        let _reload_guard = self.reload_lock.lock().await;
        let mut next = (**self.configs.load()).clone();
        next.insert(name.to_string(), Arc::new(config));
        self.configs.store(Arc::new(next));
    }

    /// Get a connector config by name.
    ///
    /// When executing inside a run pinned to a connector snapshot
    /// ([`PINNED_CONNECTORS`], `TD-07`), resolves from that snapshot so
    /// connector tasks within one run observe a consistent connector estate
    /// across concurrent reloads.
    pub async fn get(&self, name: &str) -> Option<Arc<ConnectorConfig>> {
        if let Ok(pinned) = PINNED_CONNECTORS.try_with(|snapshot| snapshot.get(name).cloned()) {
            return pinned;
        }
        self.configs.load().get(name).cloned()
    }

    /// Snapshot the current immutable connector map for attachment to a
    /// `orion_server::runtime::RuntimeGeneration` (`TD-07`).
    pub fn snapshot_configs(&self) -> Arc<HashMap<String, Arc<ConnectorConfig>>> {
        self.configs.load_full()
    }

    /// Restore a previously captured connector map, generation token, and load
    /// issues after an aborted generation reload (`TD-07`).
    pub async fn restore_snapshot(
        &self,
        configs: Arc<HashMap<String, Arc<ConnectorConfig>>>,
        generation: u64,
        issues: Vec<ConnectorLoadIssue>,
    ) {
        let _reload_guard = self.reload_lock.lock().await;
        self.configs.store(configs);
        self.generation.store(generation, Ordering::Release);
        *self.load_issues.write().await = issues;
    }

    /// Every loaded connector, by name. A snapshot: no lock is held.
    pub async fn list_all(&self) -> Vec<(String, Arc<ConnectorConfig>)> {
        self.configs
            .load()
            .iter()
            .map(|(name, cfg)| (name.clone(), cfg.clone()))
            .collect()
    }
}

/// One connector to load: a deployed connection over a built-in driver.
#[derive(Debug, Clone)]
pub struct StoredConnector {
    /// Identifies it in a load issue.
    pub id: String,
    /// The name tasks reference it by.
    pub name: String,
    pub connector_type: String,
    pub config_json: String,
}

/// Why a stored connector did not resolve: the `ConnectorLoadIssue` fields
/// without the identity.
#[derive(Debug, Clone)]
pub struct ResolveIssue {
    /// `env_substitution`, `json_parse`, `var_reference`,
    /// `secret_resolution`, `deserialize` or `endpoint`.
    pub stage: &'static str,
    pub reason: String,
}

impl ResolveIssue {
    fn new(stage: &'static str, reason: impl Into<String>) -> Self {
        Self {
            stage,
            reason: reason.into(),
        }
    }
}

/// A stored connector's config, resolved exactly as the registry loads it:
/// `${VAR}` placeholders (deprecated), the JSON parse, `var://` against
/// `[vars]`, secret references, the typed parse by `connector_type`, then
/// the endpoint's scheme. One sequence for the server and for the offline
/// tools that dial a connector (`sql check`), so "resolved the way the
/// server resolves it" holds by construction.
pub async fn resolve_connector_config(
    name: &str,
    id: &str,
    connector_type: &str,
    config_json: &str,
    vars: Option<&serde_json::Value>,
    resolvers: &[Box<dyn super::secrets::SecretResolver>],
) -> Result<ConnectorConfig, ResolveIssue> {
    // Resolve ${VAR} / ${VAR:-default} placeholders against the process
    // environment so connector configs can reference secrets without
    // storing them in the database. Substitution failures (missing
    // required var, malformed syntax) skip the connector and log —
    // matching how an unparseable config_json is handled below.
    let source_label = format!("connector '{}' config_json", name);
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
    let placeholders = crate::config::env_substitute::referenced_vars(config_json);
    if !placeholders.is_empty() {
        tracing::warn!(
            connector_id = %id,
            connector_name = %name,
            variables = %placeholders.iter().cloned().collect::<Vec<_>>().join(", "),
            "DEPRECATED: this connector's config uses ${{VAR}} placeholders. \
             Replace each with an env:// reference (\"env://VAR\") — ${{VAR}} in \
             stored connector configs will stop resolving in a future release"
        );
    }
    let resolved = match crate::config::env_substitute::substitute(config_json, &source_label) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                connector_id = %id,
                connector_name = %name,
                error = %e,
                "Failed to resolve env vars in connector config, skipping"
            );
            return Err(ResolveIssue::new("env_substitution", e.to_string()));
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
                connector_id = %id,
                connector_name = %name,
                error = %e,
                "Failed to parse connector config JSON, skipping"
            );
            return Err(ResolveIssue::new("json_parse", e.to_string()));
        }
    };
    // `var://` first, and typed: a var keeps the type it was declared
    // with, so a numeric knob stays a number. Nothing walks into an
    // expression here — a connector config has none.
    if let Err(e) = crate::config::vars::resolve_var_references(&mut value, vars, &|_| false) {
        tracing::error!(
            connector_id = %id,
            connector_name = %name,
            error = %e,
            "Refusing to load connector: unresolved var reference"
        );
        return Err(ResolveIssue::new("var_reference", e));
    }
    // Where the references are, before they are replaced: the typed
    // parse below coerces a resolved `true`/`false` to a boolean at
    // these paths, and nowhere else.
    let sites = super::secrets::reference_sites(&value, resolvers);
    if let Err(e) = super::secrets::resolve_in_place(&mut value, resolvers, &source_label).await {
        // Logged at ERROR, not WARN: an unresolved secret means the
        // connector is absent at request time with no other signal
        // (making the degraded set visible on /health is F16).
        tracing::error!(
            connector_id = %id,
            connector_name = %name,
            error = %e,
            "Failed to resolve secret reference in connector config, skipping"
        );
        return Err(ResolveIssue::new("secret_resolution", e.to_string()));
    }
    // The type lives in its own column, and the create/update API
    // takes it as `connector_type` alongside the config, so the column
    // is the single source of truth: the config is typed as the
    // variant it names, whatever `"type"` the stored document carries
    // or lacks. (Deserializing the tagged enum needed a `type`
    // injected first — a connector authored the documented way, with
    // none inside `config`, once failed with "missing field `type`"
    // and silently never loaded.)
    //
    // Typed per variant, through the resolved-reference parse, so a
    // boolean field whose reference resolved to `true` or `false`
    // takes the boolean — and anything else there is refused naming
    // the field, never the value.
    let Some(connector_type) = super::ConnectorType::from_stored(connector_type) else {
        return Err(ResolveIssue::new(
            "deserialize",
            format!(
                "unknown connector type '{}', expected one of {}",
                connector_type,
                super::VALID_CONNECTOR_TYPES.join(", ")
            ),
        ));
    };
    let config = match ConnectorConfig::parse_variant(
        connector_type,
        &value,
        &super::secrets::ResolvedReferences { sites: &sites },
    ) {
        Ok(config) => config,
        Err(e) => {
            tracing::warn!(
                connector_id = %id,
                connector_name = %name,
                error = %e,
                "Failed to parse connector config, skipping"
            );
            return Err(ResolveIssue::new("deserialize", e.to_string()));
        }
    };
    // Nothing judged the endpoint after its references resolved: a
    // stored `env://` URL that resolves to `ftp://…` reached the
    // client unjudged and failed late and obscurely. The scheme is
    // checked here, on the final value; the private-address check
    // stays on the request and pool-open paths, which resolve DNS
    // and see every redirect.
    if let Err(e) = crate::connector::endpoints::validate_endpoint_schemes(
        &config,
        crate::connector::endpoints::EndpointPhase::Load,
    ) {
        tracing::warn!(
            connector_id = %id,
            connector_name = %name,
            error = %e.client_message(),
            "Connector endpoint refused at load, skipping"
        );
        return Err(ResolveIssue::new("endpoint", e.client_message()));
    }
    // A row stored while the cache connector had another backend (Redis,
    // before it left Community) parses, but this build cannot serve it.
    // Refused here, so it is a load issue an operator sees on `/health` and
    // the connector list, not a silent per-node store or a failure on
    // first use.
    if let ConnectorConfig::Cache(cache) = &config
        && let Some(reason) = cache.unsupported_backend()
    {
        tracing::warn!(
            connector_id = %id,
            connector_name = %name,
            error = %reason,
            "Cache connector backend refused at load, skipping"
        );
        return Err(ResolveIssue::new("deserialize", reason));
    }
    Ok(config)
}

/// Connector rows a test hands the registry, and an in-memory OAuth2 state
/// store.
///
/// Lives outside `mod tests` because the channel loader's tests need it too:
/// its per-channel cache is keyed on
/// [`ConnectorRegistry::config_generation`], and the thing worth pinning is
/// what happens to that key when a *load* of an unchanged set runs.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::*;
    use crate::errors::OrionError;

    pub struct StubConnectorRepo {
        rows: std::sync::Mutex<Vec<StoredConnector>>,
        /// In-memory `connector_oauth_state` (#268): name → (fingerprint,
        /// state_json). The token-manager tests exercise rotation
        /// persistence through it.
        oauth_state: std::sync::Mutex<std::collections::HashMap<String, (String, String)>>,
    }

    impl StubConnectorRepo {
        /// Rows as `(name, connector_type, config_json)`.
        pub fn with(rows: Vec<(&str, &str, &str)>) -> Self {
            Self {
                rows: std::sync::Mutex::new(rows.into_iter().map(row).collect()),
                oauth_state: std::sync::Mutex::new(std::collections::HashMap::new()),
            }
        }

        pub fn set(&self, rows: Vec<(&str, &str, &str)>) {
            *self.rows.lock().expect("test") = rows.into_iter().map(row).collect();
        }

        /// The rows, as [`ConnectorRegistry::load`] takes them.
        pub fn stored(&self) -> Vec<StoredConnector> {
            self.rows.lock().expect("test").clone()
        }
    }

    fn row((name, connector_type, config_json): (&str, &str, &str)) -> StoredConnector {
        StoredConnector {
            id: format!("con_{name}"),
            name: name.to_string(),
            connector_type: connector_type.to_string(),
            config_json: config_json.to_string(),
        }
    }

    #[async_trait::async_trait]
    impl crate::storage::repositories::oauth_state::OAuthStateRepository for StubConnectorRepo {
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
        let b1 = registry.get_or_create_breaker("ch", "conn").await;
        let b2 = registry.get_or_create_breaker("ch", "conn").await;
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
        let _ = registry.get_or_create_breaker("ch", "conn").await;
        let states = registry.circuit_breaker_states().await;
        assert_eq!(states.len(), 1);
        assert_eq!(states.get("ch:conn").expect("test"), "closed");
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
        let breaker = registry.get_or_create_breaker("ch", "conn").await;
        breaker.record_failure(); // trips it
        assert!(!breaker.check()); // open

        let found = registry.reset_circuit_breaker("ch:conn").await;
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
        let _b1 = registry.get_or_create_breaker("ch", "key1").await;
        let _b2 = registry.get_or_create_breaker("ch", "key2").await;
        let _b3 = registry.get_or_create_breaker("ch", "key3").await;

        // Access key2 and key3 to make key1 the LRU
        let _b2_again = registry.get_or_create_breaker("ch", "key2").await;
        let _b3_again = registry.get_or_create_breaker("ch", "key3").await;

        // Adding a 4th should evict key1 (LRU)
        let _b4 = registry.get_or_create_breaker("ch", "key4").await;

        let states = registry.circuit_breaker_states().await;
        assert_eq!(states.len(), 3);
        assert!(
            !states.contains_key("ch:key1"),
            "key1 should have been evicted as LRU"
        );
        assert!(states.contains_key("ch:key2"));
        assert!(states.contains_key("ch:key3"));
        assert!(states.contains_key("ch:key4"));
    }

    /// A reload prunes breakers whose channel was archived or whose connector
    /// was deleted, keying off the components the entry stores rather than
    /// parsing the composite key.
    #[tokio::test]
    async fn prune_breakers_drops_dead_channels_and_connectors() {
        let repo = StubConnectorRepo::with(vec![("conn-live", "cache", r#"{"backend":"memory"}"#)]);
        let config = CircuitBreakerConfig {
            enabled: true,
            failure_threshold: 5,
            recovery_timeout_secs: 30,
            ..Default::default()
        };
        let registry = ConnectorRegistry::new(config);
        registry.load(repo.stored()).await;

        registry.get_or_create_breaker("ch-live", "conn-live").await;
        registry.get_or_create_breaker("ch-gone", "conn-live").await;
        registry.get_or_create_breaker("ch-live", "conn-gone").await;

        let live: std::collections::HashSet<&str> = ["ch-live"].into_iter().collect();
        registry.prune_breakers(&live).await;

        let states = registry.circuit_breaker_states().await;
        assert!(
            states.contains_key("ch-live:conn-live"),
            "a live channel + live connector is kept"
        );
        assert!(
            !states.contains_key("ch-gone:conn-live"),
            "a breaker for an archived channel is pruned"
        );
        assert!(
            !states.contains_key("ch-live:conn-gone"),
            "a breaker for a deleted connector is pruned"
        );
    }

    /// A cache row stored while Redis was a Community backend parses, but is
    /// a load issue naming the backend there is, not a connector that fails
    /// on first use or quietly becomes per-node memory.
    #[tokio::test]
    async fn a_stored_redis_cache_row_is_a_load_issue() {
        let repo = StubConnectorRepo::with(vec![
            (
                "old-redis",
                "cache",
                r#"{"backend":"redis","url":"redis://cache.internal:6379"}"#,
            ),
            ("mem", "cache", r#"{"backend":"memory"}"#),
        ]);
        let registry = ConnectorRegistry::default();
        assert_eq!(registry.load(repo.stored()).await, 1);
        assert!(registry.get("old-redis").await.is_none());
        assert!(registry.get("mem").await.is_some());
        let issues = registry.load_issues().await;
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].stage, "deserialize");
        assert_eq!(
            issues[0].reason,
            "Invalid cache backend 'redis'. Must be one of: memory"
        );
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
        let repo = StubConnectorRepo::with(vec![("cache-1", "cache", r#"{"backend":"memory"}"#)]);
        let registry = ConnectorRegistry::default();
        assert_eq!(registry.load(repo.stored()).await, 1);
        let after_first = registry.config_generation();

        for _ in 0..5 {
            assert_eq!(registry.load(repo.stored()).await, 1);
            assert_eq!(
                registry.config_generation(),
                after_first,
                "an epoch resync that changed no connector must leave the \
                 channel loader's cache key alone"
            );
        }
    }

    /// The counterpart: a real edit has to move it, or a channel could stay
    /// pinned to a dedup/response-cache backend the operator has replaced.
    #[tokio::test]
    async fn changing_a_connector_moves_the_token() {
        let repo = StubConnectorRepo::with(vec![("cache-1", "cache", r#"{"backend":"memory"}"#)]);
        let registry = ConnectorRegistry::default();
        registry.load(repo.stored()).await;
        let before = registry.config_generation();

        // Same connector, new config — the case the token exists for.
        repo.set(vec![(
            "cache-1",
            "cache",
            r#"{"backend":"memory","operations":{"write":false}}"#,
        )]);
        registry.load(repo.stored()).await;
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
                r#"{"backend":"memory","operations":{"write":false}}"#,
            ),
            ("cache-2", "cache", r#"{"backend":"memory"}"#),
        ]);
        registry.load(repo.stored()).await;
        let after_add = registry.config_generation();
        assert_ne!(after_edit, after_add, "a new connector must move the token");

        // And so does removing one.
        repo.set(vec![(
            "cache-1",
            "cache",
            r#"{"backend":"memory","operations":{"write":false}}"#,
        )]);
        registry.load(repo.stored()).await;
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
        registry.load(repo.stored()).await;
        let first = registry.get("http-1").await.expect("loaded");

        registry.load(repo.stored()).await;
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
        a.load(repo.stored()).await;
        b.load(repo.stored()).await;
        assert_ne!(a.config_generation(), b.config_generation());
    }
}
