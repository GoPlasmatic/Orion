use std::collections::HashMap;
use std::sync::Arc;

use datalogic_rs::{Engine as DatalogicEngine, Logic};
use tokio::sync::{RwLock, Semaphore};

use super::config::ChannelConfig;
use super::rate_limit_backend::{LocalRateLimitBackend, RateLimitBackend, RedisRateLimitBackend};
use super::routing::{RouteMatch, RouteTable};

use crate::config::{TraceStorageMode, TracingStorageConfig};
use crate::connector::ConnectorConfig;
use crate::connector::ConnectorRegistry;
use crate::connector::cache_backend::{CacheBackend, CachePool};
use crate::storage::models::Channel;

/// Trace-storage policy resolved for a single channel.
///
/// Produced by merging the global `[tracing.storage]` config with the
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

    /// Compute the effective config by overlaying a channel-level
    /// override on top of the global storage config.
    pub fn resolve(
        global: &TracingStorageConfig,
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
#[derive(Debug, Clone)]
pub struct ChannelLoadIssue {
    pub channel: String,
    pub reason: String,
}

impl ChannelLoadIssue {
    /// Format a non-empty issue list as the hard error both boot and reload
    /// surface (one wording for both).
    pub fn refusal_error(issues: &[ChannelLoadIssue]) -> crate::errors::OrionError {
        let detail = issues
            .iter()
            .map(|i| format!("{}: {}", i.channel, i.reason))
            .collect::<Vec<_>>()
            .join("; ");
        crate::errors::OrionError::Config {
            message: format!("refused to load {} channel(s): {detail}", issues.len()),
        }
    }
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
            route_table: RwLock::new(RouteTable::new()),
            cluster: None,
        }
    }

    pub fn with_cluster(cluster: ClusterBackends) -> Self {
        Self {
            by_name: RwLock::new(HashMap::new()),
            route_table: RwLock::new(RouteTable::new()),
            cluster: Some(cluster),
        }
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
    /// for the two classes). A non-empty result leaves the registry
    /// **untouched**: callers turn it into a hard error, and a partial swap
    /// would drop the refused channel's runtime config while the engine still
    /// routes to it — i.e. serve it with none of its guards, the exact failure
    /// the refusal exists to prevent.
    pub async fn reload(
        &self,
        channels: &[Channel],
        connector_registry: &ConnectorRegistry,
        cache_pool: &CachePool,
        datalogic: &DatalogicEngine,
        global_trace_storage: &TracingStorageConfig,
    ) -> Vec<ChannelLoadIssue> {
        let cluster_redis = self.cluster.as_ref().and_then(|c| c.redis.clone());
        let mut issues: Vec<ChannelLoadIssue> = Vec::new();

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

            let rate_limit_key_logic = parsed_config
                .rate_limit
                .as_ref()
                .and_then(|rl| rl.key_logic.as_ref())
                .and_then(|logic| {
                    datalogic
                        .compile(logic)
                        .map_err(|e| {
                            tracing::warn!(
                                channel = %channel.name,
                                error = %e,
                                "Failed to compile rate limit key_logic, falling back to client_ip"
                            );
                        })
                        .ok()
                });

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

        // All-or-nothing: see the doc comment. Callers hard-fail, and the
        // previous (consistent) registry state is what keeps serving until
        // the operator fixes the offending row.
        if !issues.is_empty() {
            return issues;
        }

        *self.by_name.write().await = new_map;

        // Rebuild the REST route table from active channels
        *self.route_table.write().await = RouteTable::build(channels);

        issues
    }

    /// Get all active channel names.
    pub async fn channel_names(&self) -> Vec<String> {
        self.by_name.read().await.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_channel_registry_empty() {
        let registry = ChannelRegistry::new();
        assert!(registry.get_by_name("nonexistent").await.is_none());
        assert!(registry.channel_names().await.is_empty());
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
                &TracingStorageConfig::default(),
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
                &TracingStorageConfig::default(),
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
                &TracingStorageConfig::default(),
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
                &TracingStorageConfig::default(),
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
        let tracing_cfg = TracingStorageConfig::default();

        let issues = registry
            .reload(
                &[test_channel("keep-ch", "{}")],
                &connectors,
                &cache_pool,
                &datalogic,
                &tracing_cfg,
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
            )
            .await;
        assert_eq!(issues.len(), 1);
        assert!(registry.get_by_name("keep-ch").await.is_some());
        assert!(registry.get_by_name("broken-ch").await.is_none());
    }
}
