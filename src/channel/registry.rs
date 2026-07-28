use std::collections::HashMap;
use std::sync::Arc;

use datalogic_rs::{Engine as DatalogicEngine, Logic};
use tokio::sync::{RwLock, Semaphore};

use super::config::ChannelConfig;
use super::rate_limit_backend::{LocalRateLimitBackend, RateLimitBackend, RedisRateLimitBackend};
use super::routing::{RouteMatch, RouteTable};

use crate::config::{TraceStorageConfig, TraceStorageMode};
use crate::connector::ConnectorConfig;
use crate::connector::ConnectorRegistry;
use crate::connector::cache_backend::{CacheBackend, CachePool};
use crate::storage::models::Channel;

/// Trace-storage policy resolved for a single channel.
///
/// Produced by merging the global `[trace_storage]` config with the
/// channel-level `tracing` override (`ChannelTracingConfig`). Lives on
/// `ChannelRuntimeConfig` so the request hot path looks up exactly one
/// `Arc<ChannelRuntimeConfig>` and reads pre-resolved values.
#[derive(Debug, Clone, Copy)]
pub struct EffectiveTraceConfig {
    pub mode: TraceStorageMode,
    pub sample_rate: f64,
    pub errors_only: bool,
    /// When `true`, the engine should capture per-task execution traces
    /// and persist them. No global default — only enabled per-channel
    /// because the storage cost scales with task count and payload size.
    pub task_details: bool,
}

impl EffectiveTraceConfig {
    /// Returns `Some(reason)` if a trace with the given error state should be
    /// dropped per this config (off / errors_only / sampled out), or `None`
    /// to persist. Used by both the sync request path and the async-queue
    /// post-processing path so filter semantics stay consistent.
    pub fn should_drop(&self, has_errors: bool) -> Option<&'static str> {
        if matches!(self.mode, TraceStorageMode::Off) {
            return Some("off");
        }
        if self.errors_only && !has_errors {
            return Some("errors_only");
        }
        if self.sample_rate < 1.0 && rand::random::<f64>() >= self.sample_rate {
            return Some("sampled_out");
        }
        None
    }

    /// The same config as seen by an **async submission**, where trace
    /// persistence is not optional.
    ///
    /// R11: `mode = "off"` on the async path used to mint a throwaway UUID,
    /// answer 202 with `{"trace_id": null, "trace_token": null}` plus a
    /// `Warning: 299` header, and enqueue the work anyway — a 202 whose
    /// documented follow-up (`GET /admin/traces/{id}`) was structurally
    /// impossible, and a nullable `trace_id` baked into the schema forever.
    ///
    /// Appending `/async` *is* the request for a result to be fetched later,
    /// and the trace row is the mechanism that makes that possible, not an
    /// optional extra. So `Off` is upgraded to `Sync` here and nowhere else:
    /// the sync path, where the caller already has the answer in hand, still
    /// honours `off` exactly.
    ///
    /// `errors_only` and `sample_rate` are deliberately left alone — they drop
    /// the *result*, but `create_pending` still writes the row, so the id the
    /// caller was handed continues to resolve.
    pub fn for_async_submission(self) -> Self {
        Self {
            mode: match self.mode {
                TraceStorageMode::Off => TraceStorageMode::Sync,
                other => other,
            },
            ..self
        }
    }

    /// Compute the effective config by overlaying a channel-level
    /// override on top of the global storage config.
    pub fn resolve(
        global: &TraceStorageConfig,
        channel: Option<&super::config::ChannelTracingConfig>,
    ) -> Self {
        let (mode, sample_rate, errors_only, task_details) = match channel {
            Some(c) => (
                c.mode.unwrap_or(global.mode),
                c.sample_rate.unwrap_or(global.sample_rate),
                c.errors_only.unwrap_or(global.errors_only),
                c.task_details.unwrap_or(false),
            ),
            None => (global.mode, global.sample_rate, global.errors_only, false),
        };
        Self {
            mode,
            sample_rate,
            errors_only,
            task_details,
        }
    }
}

/// Runtime state for a single active channel.
pub struct ChannelRuntimeConfig {
    /// The channel DB model.
    pub channel: Channel,
    /// Parsed per-channel configuration.
    pub parsed_config: ChannelConfig,
    /// Per-channel rate limiter, built from `parsed_config.rate_limit` if
    /// configured. Governor-local on a single node; shared Redis fixed
    /// window in cluster mode.
    pub rate_limiter: Option<Arc<dyn RateLimitBackend>>,
    /// Pre-compiled JSONLogic expression for computing the rate limit key.
    pub rate_limit_key_logic: Option<Logic>,
    /// Pre-compiled JSONLogic expression for input validation.
    /// Evaluated against request data — truthy = pass, falsy = 400 reject.
    pub validation_logic: Option<Logic>,
    /// Per-channel concurrency limiter for backpressure.
    /// Limits max in-flight requests — returns 503 when exhausted.
    pub backpressure_semaphore: Option<Arc<Semaphore>>,
    /// Per-channel deduplication backend for idempotent request handling.
    /// Can be backed by in-memory DashMap or Redis, depending on channel config.
    pub dedup_store: Option<Arc<dyn CacheBackend>>,
    /// Per-channel response cache backend.
    /// When set, sync responses are cached with a configurable TTL.
    pub response_cache: Option<Arc<dyn CacheBackend>>,
    /// Trace storage policy after merging the global and per-channel config.
    pub trace_storage: EffectiveTraceConfig,
}

/// A channel the registry refused to load, and why.
///
/// Two classes produce one:
/// - **Config invariants (any mode).** A `config_json` that no longer parses
///   (N3) or a `validation_logic` that no longer compiles (N4) would load the
///   channel with its guards silently absent — serving unvalidated,
///   unthrottled traffic on the strength of a log line. Both are checked at
///   create/update time, so a failure here means the stored row is corrupt.
/// - **Backend degradation (cluster mode only).** A dedup/response-cache
///   backend that would fall back to per-node memory is a correctness loss in
///   a cluster but an acceptable degradation on one node.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChannelLoadIssue {
    pub channel: String,
    pub reason: String,
}

/// The shared backends cluster mode provides to channel loading. `Some` on
/// the registry means cluster mode: strict backend resolution (no silent
/// in-memory fallbacks) with these as the defaults. Deliberately narrow —
/// the registry needs these two handles, not the whole cluster runtime.
pub struct ClusterBackends {
    /// Default dedup/response-cache backend (the shared cluster Redis).
    pub default_cache: Option<Arc<dyn CacheBackend>>,
    /// Connection for shared rate-limit windows.
    pub redis: Option<redis::aio::ConnectionManager>,
}

/// In-memory registry of active channels, rebuilt on engine reload.
/// Mirrors the ConnectorRegistry pattern.
pub struct ChannelRegistry {
    by_name: RwLock<HashMap<String, Arc<ChannelRuntimeConfig>>>,
    route_table: RwLock<RouteTable>,
    /// Channels that failed to load, by name, with the reason (F35).
    ///
    /// A quarantined channel is refused at every ingress rather than served
    /// with none of its guards — that refusal is the whole point of N3/N4.
    /// Keeping them here instead of aborting the reload confines the blast
    /// radius to the broken channel: the other channels still load, and the
    /// admin mutations that trigger a reload still work.
    quarantined: RwLock<HashMap<String, String>>,
    /// `Some` = cluster mode (strict backend matrix in
    /// [`ChannelRegistry::reload`]: shared-Redis defaults and load errors
    /// instead of silent in-memory fallbacks).
    cluster: Option<ClusterBackends>,
}

impl Default for ChannelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self {
            by_name: RwLock::new(HashMap::new()),
            route_table: RwLock::new(RouteTable::default()),
            quarantined: RwLock::new(HashMap::new()),
            cluster: None,
        }
    }

    pub fn with_cluster(cluster: ClusterBackends) -> Self {
        Self {
            cluster: Some(cluster),
            ..Self::new()
        }
    }

    /// Why a channel is quarantined, or `None` when it is serviceable (F35).
    pub async fn quarantine_reason(&self, name: &str) -> Option<String> {
        self.quarantined.read().await.get(name).cloned()
    }

    /// Every quarantined channel, for `/health` and the admin surface.
    pub async fn quarantined(&self) -> Vec<ChannelLoadIssue> {
        self.quarantined
            .read()
            .await
            .iter()
            .map(|(channel, reason)| ChannelLoadIssue {
                channel: channel.clone(),
                reason: reason.clone(),
            })
            .collect()
    }

    /// Look up a channel's runtime config, refusing quarantined channels.
    ///
    /// Every ingress path goes through this rather than [`Self::get_by_name`]:
    /// a quarantined channel returns `Ok(None)` from a plain lookup, which is
    /// indistinguishable from "no config" and would serve it with none of its
    /// guards — the exact failure N3/N4 exist to prevent.
    pub async fn require_serviceable(
        &self,
        name: &str,
    ) -> Result<Option<Arc<ChannelRuntimeConfig>>, crate::errors::OrionError> {
        if let Some(reason) = self.quarantine_reason(name).await {
            return Err(crate::errors::OrionError::ServiceUnavailable(format!(
                "Channel '{name}' failed to load and is not being served: {reason}"
            )));
        }
        Ok(self.get_by_name(name).await)
    }

    /// Resolve a channel's dedup or response-cache backend — the full
    /// fallback matrix, shared by both stores:
    ///
    /// |                | connector named               | no connector           |
    /// |----------------|-------------------------------|------------------------|
    /// | single node    | resolve, warn + memory on any failure | process memory |
    /// | cluster mode   | resolve; failure/memory = load error  | shared Redis (else memory) |
    async fn resolve_backend(
        &self,
        connector_registry: &ConnectorRegistry,
        cache_pool: &CachePool,
        connector: Option<&str>,
        purpose: &str,
        channel_name: &str,
    ) -> Result<Arc<dyn CacheBackend>, ChannelLoadIssue> {
        let Some(connector_name) = connector else {
            return Ok(self
                .cluster
                .as_ref()
                .and_then(|c| c.default_cache.clone())
                .unwrap_or_else(|| cache_pool.memory()));
        };

        let strict = self.cluster.is_some();
        let resolved = match connector_registry.get(connector_name).await {
            Some(cfg) => match cfg.as_ref() {
                ConnectorConfig::Cache(cache_cfg) => {
                    if strict && cache_cfg.backend == "memory" {
                        Err(format!(
                            "connector '{connector_name}' uses the in-memory backend — \
                             per-node state in cluster mode is a silent correctness loss"
                        ))
                    } else {
                        cache_pool
                            .get_backend(connector_name, cache_cfg)
                            .await
                            .map_err(|e| {
                                format!(
                                    "failed to create backend from connector '{connector_name}': {e}"
                                )
                            })
                    }
                }
                _ => Err(format!(
                    "connector '{connector_name}' is not a cache connector"
                )),
            },
            None => Err(format!("connector '{connector_name}' not found")),
        };

        match resolved {
            Ok(backend) => Ok(backend),
            Err(reason) if strict => Err(ChannelLoadIssue {
                channel: channel_name.to_string(),
                reason: format!("{purpose}: {reason}"),
            }),
            Err(reason) => {
                tracing::warn!(
                    channel = %channel_name,
                    connector = %connector_name,
                    reason = %reason,
                    "{purpose} connector unavailable, falling back to in-memory"
                );
                Ok(cache_pool.memory())
            }
        }
    }

    /// Look up an active channel by name.
    pub async fn get_by_name(&self, name: &str) -> Option<Arc<ChannelRuntimeConfig>> {
        self.by_name.read().await.get(name).cloned()
    }

    /// Match a request (method, path) against REST channel route patterns.
    /// Path should NOT include the `/api/v1/data/` prefix.
    pub async fn match_route(&self, method: &str, path: &str) -> Option<RouteMatch> {
        self.route_table.read().await.match_route(method, path)
    }

    /// Rebuild the registry from a list of active channels.
    /// Builds per-channel rate limiters from `config_json.rate_limit` if configured.
    ///
    /// Returns the channels that were **not** loaded (see [`ChannelLoadIssue`]
    /// for the two classes). Those channels are **quarantined**: absent from
    /// the registry and from the route table, and refused at every ingress by
    /// [`Self::require_serviceable`]. The rest of the reload succeeds.
    ///
    /// This used to be all-or-nothing — a non-empty result left the registry
    /// untouched and callers hard-failed. That kept a broken channel from
    /// being served unguarded, which is right, but it also meant one channel
    /// with an unparseable `config_json` failed *every* operation that
    /// triggers a reload (activate, archive, delete, rollout) with a 500, and
    /// stopped the cluster epoch watcher resyncing every node (F35). Refusing
    /// the broken channel individually keeps the guarantee and drops the
    /// blast radius to the one row that is actually broken.
    pub async fn reload(
        &self,
        channels: &[Channel],
        connector_registry: &ConnectorRegistry,
        cache_pool: &CachePool,
        datalogic: &DatalogicEngine,
        global_trace_storage: &TraceStorageConfig,
        engine_issues: Vec<ChannelLoadIssue>,
    ) -> Vec<ChannelLoadIssue> {
        let cluster_redis = self.cluster.as_ref().and_then(|c| c.redis.clone());
        // F33: seed with the engine-build failures (workflow missing or
        // unconvertible). Those channels are quarantined exactly like ones
        // whose own config fails to load — previously they stayed in the
        // route table with no workflow behind them and served opaque engine
        // errors. The registry still runs its own checks on them below, so a
        // channel broken in both stages reports its more specific
        // config-load reason (the quarantine map is last-write-wins), and
        // engine-quarantined channels are removed from the serving map after
        // the loop.
        let mut issues: Vec<ChannelLoadIssue> = engine_issues;
        let engine_quarantined: std::collections::HashSet<String> =
            issues.iter().map(|i| i.channel.clone()).collect();

        let mut new_map = HashMap::new();
        for channel in channels {
            // N3: `unwrap_or_default()` here used to turn one typo in the
            // stored config into a channel with no rate limit, validation,
            // dedup, backpressure, timeout, or cache — and no log line.
            let parsed_config: ChannelConfig = match serde_json::from_str(&channel.config_json) {
                Ok(cfg) => cfg,
                Err(e) => {
                    tracing::error!(
                        channel = %channel.name,
                        error = %e,
                        "Refusing to load channel: config_json does not parse"
                    );
                    issues.push(ChannelLoadIssue {
                        channel: channel.name.clone(),
                        reason: format!("config_json does not parse: {e}"),
                    });
                    continue;
                }
            };

            let rate_limiter: Option<Arc<dyn RateLimitBackend>> =
                parsed_config.rate_limit.as_ref().map(|rl| {
                    let burst = rl.burst.unwrap_or(rl.requests_per_second / 2 + 1);
                    match cluster_redis.clone() {
                        // Cluster: shared fixed window — the configured limit
                        // holds across all replicas combined, and survives
                        // engine reloads (state lives in Redis, not here).
                        Some(conn) => Arc::new(RedisRateLimitBackend::new(
                            conn,
                            channel.name.clone(),
                            rl.requests_per_second,
                            burst,
                        )) as Arc<dyn RateLimitBackend>,
                        None => Arc::new(LocalRateLimitBackend::new(rl.requests_per_second, burst)),
                    }
                });

            // N5: an uncompilable `key_logic` used to fall back to
            // `client_ip`, silently re-dimensioning the limit — a per-API-key
            // or per-tenant limit became per-IP, so every tenant behind one
            // NAT shared a bucket and one tenant got N× its quota by rotating
            // IPs. Quarantine the channel instead, like N3/N4 do for the
            // other guard-bearing config.
            let key_logic_source = parsed_config
                .rate_limit
                .as_ref()
                .and_then(|rl| rl.key_logic.as_ref());
            let rate_limit_key_logic = match key_logic_source {
                Some(logic) => match datalogic.compile(logic) {
                    Ok(compiled) => Some(compiled),
                    Err(e) => {
                        tracing::error!(
                            channel = %channel.name,
                            error = %e,
                            "Refusing to load channel: rate_limit.key_logic does not compile"
                        );
                        issues.push(ChannelLoadIssue {
                            channel: channel.name.clone(),
                            reason: format!("rate_limit.key_logic does not compile: {e}"),
                        });
                        continue;
                    }
                },
                None => None,
            };

            // N4: dropping an uncompilable expression to `None` made
            // `validate_input` a no-op, so the channel's declared input
            // contract disappeared at reload time. Refuse instead — the
            // expression compiled at create/update time, so this is a
            // corrupt row or a datalogic upgrade that changed semantics.
            let validation_logic = match parsed_config.validation_logic.as_ref() {
                Some(logic) => match datalogic.compile(logic) {
                    Ok(compiled) => Some(compiled),
                    Err(e) => {
                        tracing::error!(
                            channel = %channel.name,
                            error = %e,
                            "Refusing to load channel: validation_logic does not compile"
                        );
                        issues.push(ChannelLoadIssue {
                            channel: channel.name.clone(),
                            reason: format!("validation_logic does not compile: {e}"),
                        });
                        continue;
                    }
                },
                None => None,
            };

            let backpressure_semaphore = parsed_config
                .backpressure
                .as_ref()
                .map(|bp| Arc::new(Semaphore::new(bp.max_concurrent)));

            // Dedup / response-cache backends via the shared fallback matrix
            // (see resolve_backend). A strict-mode refusal skips the channel.
            let dedup_store: Option<Arc<dyn CacheBackend>> =
                if let Some(ref dedup) = parsed_config.deduplication {
                    match self
                        .resolve_backend(
                            connector_registry,
                            cache_pool,
                            dedup.connector.as_deref(),
                            "deduplication",
                            &channel.name,
                        )
                        .await
                    {
                        Ok(backend) => Some(backend),
                        Err(issue) => {
                            issues.push(issue);
                            continue;
                        }
                    }
                } else {
                    None
                };

            let response_cache: Option<Arc<dyn CacheBackend>> = if let Some(ref cache_cfg) =
                parsed_config.cache
                && cache_cfg.enabled
            {
                match self
                    .resolve_backend(
                        connector_registry,
                        cache_pool,
                        cache_cfg.connector.as_deref(),
                        "cache",
                        &channel.name,
                    )
                    .await
                {
                    Ok(backend) => Some(backend),
                    Err(issue) => {
                        issues.push(issue);
                        continue;
                    }
                }
            } else {
                None
            };

            let trace_storage =
                EffectiveTraceConfig::resolve(global_trace_storage, parsed_config.tracing.as_ref());
            let runtime = Arc::new(ChannelRuntimeConfig {
                channel: channel.clone(),
                parsed_config,
                rate_limiter,
                rate_limit_key_logic,
                validation_logic,
                backpressure_semaphore,
                dedup_store,
                response_cache,
                trace_storage,
            });
            new_map.insert(channel.name.clone(), runtime);
        }

        // F33: a channel whose workflows failed to build must not serve even
        // when its own config loaded fine.
        for name in &engine_quarantined {
            new_map.remove(name);
        }

        *self.by_name.write().await = new_map;

        // Quarantined channels are excluded from the route table too, so
        // their REST routes 404 rather than resolving to a channel that will
        // then be refused — and so a broken channel cannot shadow the route
        // of a working one.
        let quarantined: HashMap<String, String> = issues
            .iter()
            .map(|i| (i.channel.clone(), i.reason.clone()))
            .collect();
        let serviceable: Vec<Channel> = channels
            .iter()
            .filter(|c| !quarantined.contains_key(&c.name))
            .cloned()
            .collect();
        *self.route_table.write().await = RouteTable::build(&serviceable);
        *self.quarantined.write().await = quarantined;

        if !issues.is_empty() {
            tracing::error!(
                quarantined = issues.len(),
                loaded = channels.len() - issues.len(),
                "Some channels failed to load and are being refused at every \
                 ingress. See /health for the list; the rest of the instance \
                 is unaffected."
            );
        }

        issues
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_channel_registry_empty() {
        let registry = ChannelRegistry::new();
        assert!(registry.get_by_name("nonexistent").await.is_none());
        assert!(registry.by_name.read().await.is_empty());
    }

    fn test_channel(name: &str, config_json: &str) -> Channel {
        let now = chrono::Utc::now().naive_utc();
        Channel {
            channel_id: format!("ch_{name}"),
            version: 1,
            name: name.to_string(),
            description: None,
            channel_type: "sync".to_string(),
            protocol: "http".to_string(),
            methods: None,
            route_pattern: None,
            topic: None,
            consumer_group: None,
            transport_config_json: "{}".to_string(),
            workflow_id: None,
            config_json: config_json.to_string(),
            status: "active".to_string(),
            priority: 0,
            created_at: now,
            updated_at: now,
        }
    }

    fn cluster_backends() -> ClusterBackends {
        ClusterBackends {
            default_cache: Some(CachePool::new(4, 60, 1000).memory()),
            redis: None,
        }
    }

    #[tokio::test]
    async fn test_cluster_strict_mode_refuses_broken_dedup_connector() {
        let registry = ChannelRegistry::with_cluster(cluster_backends());
        let channel = test_channel(
            "strict-ch",
            r#"{"deduplication": {"header": "idem", "connector": "missing-connector"}}"#,
        );
        let issues = registry
            .reload(
                &[channel],
                &ConnectorRegistry::new(crate::config::EngineConfig::default().circuit_breaker),
                &CachePool::new(4, 60, 1000),
                &DatalogicEngine::new(),
                &TraceStorageConfig::default(),
                Vec::new(),
            )
            .await;
        assert_eq!(issues.len(), 1);
        assert!(issues[0].reason.contains("missing-connector"));
        // The channel must NOT be served with a silent per-node fallback.
        assert!(registry.get_by_name("strict-ch").await.is_none());
    }

    #[tokio::test]
    async fn test_cluster_mode_defaults_dedup_to_shared_cache() {
        let registry = ChannelRegistry::with_cluster(cluster_backends());
        let channel = test_channel("shared-ch", r#"{"deduplication": {"header": "idem"}}"#);
        let issues = registry
            .reload(
                &[channel],
                &ConnectorRegistry::new(crate::config::EngineConfig::default().circuit_breaker),
                &CachePool::new(4, 60, 1000),
                &DatalogicEngine::new(),
                &TraceStorageConfig::default(),
                Vec::new(),
            )
            .await;
        assert!(issues.is_empty());
        let runtime = registry.get_by_name("shared-ch").await.expect("loaded");
        assert!(runtime.dedup_store.is_some());
    }

    #[tokio::test]
    async fn test_single_node_broken_connector_still_falls_back() {
        let registry = ChannelRegistry::new();
        let channel = test_channel(
            "fallback-ch",
            r#"{"deduplication": {"header": "idem", "connector": "missing-connector"}}"#,
        );
        let issues = registry
            .reload(
                &[channel],
                &ConnectorRegistry::new(crate::config::EngineConfig::default().circuit_breaker),
                &CachePool::new(4, 60, 1000),
                &DatalogicEngine::new(),
                &TraceStorageConfig::default(),
                Vec::new(),
            )
            .await;
        // Backend *degradation* stays cluster-only: on one node an in-memory
        // dedup store is the documented fallback, not a correctness loss.
        assert!(issues.is_empty());
        assert!(registry.get_by_name("fallback-ch").await.is_some());
    }

    async fn reload_single_node(channel: Channel) -> (ChannelRegistry, Vec<ChannelLoadIssue>) {
        let registry = ChannelRegistry::new();
        let issues = registry
            .reload(
                &[channel],
                &ConnectorRegistry::new(crate::config::EngineConfig::default().circuit_breaker),
                &CachePool::new(4, 60, 1000),
                &DatalogicEngine::new(),
                &TraceStorageConfig::default(),
                Vec::new(),
            )
            .await;
        (registry, issues)
    }

    /// N3: a stored config that does not parse must not load as "no guards".
    #[tokio::test]
    async fn test_malformed_config_json_refuses_to_load_single_node() {
        // `requests_per_second` typed as a string — passes create-time
        // validation nowhere, but this is what a hand-edited row looks like.
        let channel = test_channel(
            "broken-cfg-ch",
            r#"{"rate_limit": {"requests_per_second": "100"}}"#,
        );
        let (registry, issues) = reload_single_node(channel).await;
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].channel, "broken-cfg-ch");
        assert!(
            issues[0].reason.contains("config_json does not parse"),
            "unexpected reason: {}",
            issues[0].reason
        );
        assert!(registry.get_by_name("broken-cfg-ch").await.is_none());
    }

    #[tokio::test]
    async fn test_malformed_config_json_is_not_valid_json_refuses() {
        let channel = test_channel("not-json-ch", "{ this is not json ");
        let (registry, issues) = reload_single_node(channel).await;
        assert_eq!(issues.len(), 1);
        assert!(registry.get_by_name("not-json-ch").await.is_none());
    }

    /// N4: an uncompilable `validation_logic` must not silently disable
    /// input validation.
    #[tokio::test]
    async fn test_uncompilable_validation_logic_refuses_to_load_single_node() {
        // Multi-key object: valid JSON, rejected by the datalogic compiler
        // outside templating mode.
        let channel = test_channel(
            "bad-logic-ch",
            r#"{"validation_logic": {"==": [1, 1], "!=": [1, 2]}}"#,
        );
        let (registry, issues) = reload_single_node(channel).await;
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].channel, "bad-logic-ch");
        assert!(
            issues[0]
                .reason
                .contains("validation_logic does not compile"),
            "unexpected reason: {}",
            issues[0].reason
        );
        assert!(registry.get_by_name("bad-logic-ch").await.is_none());
    }

    #[tokio::test]
    async fn test_valid_validation_logic_still_loads() {
        let channel = test_channel(
            "good-logic-ch",
            r#"{"validation_logic": {"!!": [{"var": "data.id"}]}}"#,
        );
        let (registry, issues) = reload_single_node(channel).await;
        assert!(issues.is_empty(), "unexpected issues: {issues:?}");
        let runtime = registry.get_by_name("good-logic-ch").await.expect("loaded");
        assert!(runtime.validation_logic.is_some());
    }

    /// A refusal must not partially apply: the previously loaded channels
    /// keep serving with their guards rather than being dropped.
    #[tokio::test]
    async fn test_refusal_leaves_previous_registry_intact() {
        let registry = ChannelRegistry::new();
        let connectors =
            ConnectorRegistry::new(crate::config::EngineConfig::default().circuit_breaker);
        let cache_pool = CachePool::new(4, 60, 1000);
        let datalogic = DatalogicEngine::new();
        let tracing_cfg = TraceStorageConfig::default();

        let issues = registry
            .reload(
                &[test_channel("keep-ch", "{}")],
                &connectors,
                &cache_pool,
                &datalogic,
                &tracing_cfg,
                Vec::new(),
            )
            .await;
        assert!(issues.is_empty());

        let issues = registry
            .reload(
                &[
                    test_channel("keep-ch", "{}"),
                    test_channel("broken-ch", "{ nope"),
                ],
                &connectors,
                &cache_pool,
                &datalogic,
                &tracing_cfg,
                Vec::new(),
            )
            .await;
        assert_eq!(issues.len(), 1);
        assert!(registry.get_by_name("keep-ch").await.is_some());
        assert!(registry.get_by_name("broken-ch").await.is_none());
    }

    // ---- Trace policy: EffectiveTraceConfig::resolve + should_drop --------

    fn global_tracing(
        mode: TraceStorageMode,
        sample_rate: f64,
        errors_only: bool,
    ) -> TraceStorageConfig {
        TraceStorageConfig {
            mode,
            sample_rate,
            errors_only,
            ..Default::default()
        }
    }

    /// Channel-over-global precedence, field by field: every Some on the
    /// channel override wins; every None falls through to the global value;
    /// task_details has no global and defaults to false.
    #[test]
    fn effective_trace_config_overlays_channel_fields_over_global() {
        let global = global_tracing(TraceStorageMode::Sync, 0.5, false);

        // No channel override at all → globals verbatim, task_details off.
        let eff = EffectiveTraceConfig::resolve(&global, None);
        assert!(matches!(eff.mode, TraceStorageMode::Sync));
        assert_eq!(eff.sample_rate, 0.5);
        assert!(!eff.errors_only);
        assert!(!eff.task_details);

        // Channel override with every field set → all channel values.
        let channel = super::super::config::ChannelTracingConfig {
            mode: Some(TraceStorageMode::Off),
            sample_rate: Some(0.1),
            errors_only: Some(true),
            task_details: Some(true),
        };
        let eff = EffectiveTraceConfig::resolve(&global, Some(&channel));
        assert!(matches!(eff.mode, TraceStorageMode::Off));
        assert_eq!(eff.sample_rate, 0.1);
        assert!(eff.errors_only);
        assert!(eff.task_details);

        // Channel override with every field None → globals fall through.
        let channel = super::super::config::ChannelTracingConfig {
            mode: None,
            sample_rate: None,
            errors_only: None,
            task_details: None,
        };
        let eff = EffectiveTraceConfig::resolve(&global, Some(&channel));
        assert!(matches!(eff.mode, TraceStorageMode::Sync));
        assert_eq!(eff.sample_rate, 0.5);
        assert!(!eff.errors_only);
        assert!(!eff.task_details, "task_details has no global to inherit");
    }

    /// Drop-filter precedence: Off beats everything; errors_only spares
    /// error traces; the sampling coin at the deterministic extremes
    /// (0.0 always drops, 1.0 never enters the roll).
    #[test]
    fn should_drop_filters_in_precedence_order() {
        let off =
            EffectiveTraceConfig::resolve(&global_tracing(TraceStorageMode::Off, 1.0, false), None);
        assert_eq!(
            off.should_drop(true),
            Some("off"),
            "Off wins even for errors"
        );

        let errors_only =
            EffectiveTraceConfig::resolve(&global_tracing(TraceStorageMode::Sync, 1.0, true), None);
        assert_eq!(errors_only.should_drop(false), Some("errors_only"));
        assert_eq!(
            errors_only.should_drop(true),
            None,
            "error traces are spared"
        );

        let sampled_out = EffectiveTraceConfig::resolve(
            &global_tracing(TraceStorageMode::Sync, 0.0, false),
            None,
        );
        assert_eq!(
            sampled_out.should_drop(false),
            Some("sampled_out"),
            "rate 0.0 must drop every trace"
        );

        let keep_all = EffectiveTraceConfig::resolve(
            &global_tracing(TraceStorageMode::Sync, 1.0, false),
            None,
        );
        assert_eq!(
            keep_all.should_drop(false),
            None,
            "rate 1.0 never samples out"
        );
        assert_eq!(keep_all.should_drop(true), None);
    }
}
