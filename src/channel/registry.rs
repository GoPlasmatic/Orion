use std::collections::HashMap;
use std::sync::Arc;

use datalogic_rs::{Engine as DatalogicEngine, Logic};
use tokio::sync::{RwLock, Semaphore};

use super::config::ChannelConfig;
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
    pub rate_limiter: Option<Arc<dyn super::rate_limit_backend::RateLimitBackend>>,
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

/// A channel the registry refused to load, and why. Only produced in
/// cluster mode, where a silent per-node fallback (in-memory dedup/cache)
/// is a correctness loss rather than a degradation.
#[derive(Debug, Clone)]
pub struct ChannelLoadIssue {
    pub channel: String,
    pub reason: String,
}

/// In-memory registry of active channels, rebuilt on engine reload.
/// Mirrors the ConnectorRegistry pattern.
pub struct ChannelRegistry {
    by_name: RwLock<HashMap<String, Arc<ChannelRuntimeConfig>>>,
    route_table: RwLock<RouteTable>,
    /// Cluster runtime, when running multi-instance. Drives the strict
    /// backend matrix in [`ChannelRegistry::reload`]: shared-Redis defaults
    /// and load errors instead of silent in-memory fallbacks.
    cluster: Option<Arc<crate::cluster::ClusterRuntime>>,
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

    pub fn with_cluster(cluster: Arc<crate::cluster::ClusterRuntime>) -> Self {
        Self {
            by_name: RwLock::new(HashMap::new()),
            route_table: RwLock::new(RouteTable::new()),
            cluster: Some(cluster),
        }
    }

    /// Resolve a named cache connector to a backend, or explain why not.
    async fn resolve_cache_connector(
        connector_registry: &ConnectorRegistry,
        cache_pool: &CachePool,
        connector_name: &str,
        cluster_strict: bool,
    ) -> Result<Arc<dyn CacheBackend>, String> {
        match connector_registry.get(connector_name).await {
            Some(cfg) => {
                match cfg.as_ref() {
                    ConnectorConfig::Cache(cache_cfg) => {
                        if cluster_strict && cache_cfg.backend == "memory" {
                            return Err(format!(
                                "connector '{connector_name}' uses the in-memory backend — \
                             per-node state in cluster mode is a silent correctness loss"
                            ));
                        }
                        cache_pool
                        .get_backend(connector_name, cache_cfg)
                        .await
                        .map_err(|e| {
                            format!("failed to create backend from connector '{connector_name}': {e}")
                        })
                    }
                    _ => Err(format!(
                        "connector '{connector_name}' is not a cache connector"
                    )),
                }
            }
            None => Err(format!("connector '{connector_name}' not found")),
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
    /// Returns the channels that were **not** loaded. Always empty outside
    /// cluster mode (single-node keeps the historical warn-and-fall-back
    /// behavior); in cluster mode a dedup/cache backend that would silently
    /// degrade to per-node memory refuses to load instead — callers turn a
    /// non-empty result into a hard error.
    pub async fn reload(
        &self,
        channels: &[Channel],
        connector_registry: &ConnectorRegistry,
        cache_pool: &CachePool,
        datalogic: &DatalogicEngine,
        global_trace_storage: &TracingStorageConfig,
    ) -> Vec<ChannelLoadIssue> {
        let cluster_strict = self.cluster.as_ref().is_some_and(|c| c.enabled);
        let cluster_default_cache = self
            .cluster
            .as_ref()
            .filter(|c| c.enabled)
            .and_then(|c| c.default_cache.clone());
        let cluster_redis = self
            .cluster
            .as_ref()
            .filter(|c| c.enabled)
            .and_then(|c| c.redis.clone());
        let mut issues: Vec<ChannelLoadIssue> = Vec::new();

        let mut new_map = HashMap::new();
        for channel in channels {
            let parsed_config: ChannelConfig =
                serde_json::from_str(&channel.config_json).unwrap_or_default();

            let rate_limiter: Option<Arc<dyn super::rate_limit_backend::RateLimitBackend>> =
                parsed_config.rate_limit.as_ref().map(|rl| {
                    let burst = rl.burst.unwrap_or(rl.requests_per_second / 2 + 1);
                    match cluster_redis.clone() {
                        // Cluster: shared fixed window — the configured limit
                        // holds across all replicas combined, and survives
                        // engine reloads (state lives in Redis, not here).
                        Some(conn) => {
                            Arc::new(super::rate_limit_backend::RedisRateLimitBackend::new(
                                conn,
                                channel.name.clone(),
                                rl.requests_per_second,
                                burst,
                            ))
                                as Arc<dyn super::rate_limit_backend::RateLimitBackend>
                        }
                        None => Arc::new(super::rate_limit_backend::LocalRateLimitBackend::new(
                            rl.requests_per_second,
                            burst,
                        )),
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

            let validation_logic = parsed_config.validation_logic.as_ref().and_then(|logic| {
                datalogic
                    .compile(logic)
                    .map_err(|e| {
                        tracing::warn!(
                            channel = %channel.name,
                            error = %e,
                            "Failed to compile validation_logic, skipping input validation"
                        );
                    })
                    .ok()
            });

            let backpressure_semaphore = parsed_config
                .backpressure
                .as_ref()
                .map(|bp| Arc::new(Semaphore::new(bp.max_concurrent)));

            // Dedup backend. Fallback matrix:
            //   cluster off: named connector, warn + memory on any failure
            //                (historical behavior); no connector → memory.
            //   cluster on:  failure/memory-connector → channel-load error;
            //                no connector → the shared cluster Redis.
            let dedup_store: Option<Arc<dyn CacheBackend>> =
                if let Some(ref dedup) = parsed_config.deduplication {
                    if let Some(ref connector_name) = dedup.connector {
                        match Self::resolve_cache_connector(
                            connector_registry,
                            cache_pool,
                            connector_name,
                            cluster_strict,
                        )
                        .await
                        {
                            Ok(backend) => Some(backend),
                            Err(reason) => {
                                if cluster_strict {
                                    issues.push(ChannelLoadIssue {
                                        channel: channel.name.clone(),
                                        reason: format!("deduplication: {reason}"),
                                    });
                                    continue;
                                }
                                tracing::warn!(
                                    channel = %channel.name,
                                    connector = %connector_name,
                                    reason = %reason,
                                    "Dedup connector unavailable, falling back to in-memory"
                                );
                                Some(cache_pool.memory())
                            }
                        }
                    } else if let Some(ref default_cache) = cluster_default_cache {
                        Some(default_cache.clone())
                    } else {
                        // No connector specified — use built-in in-memory
                        Some(cache_pool.memory())
                    }
                } else {
                    None
                };

            // Resolve response cache backend (same matrix as dedup)
            let response_cache: Option<Arc<dyn CacheBackend>> = if let Some(ref cache_cfg) =
                parsed_config.cache
                && cache_cfg.enabled
            {
                if let Some(ref connector_name) = cache_cfg.connector {
                    match Self::resolve_cache_connector(
                        connector_registry,
                        cache_pool,
                        connector_name,
                        cluster_strict,
                    )
                    .await
                    {
                        Ok(backend) => Some(backend),
                        Err(reason) => {
                            if cluster_strict {
                                issues.push(ChannelLoadIssue {
                                    channel: channel.name.clone(),
                                    reason: format!("cache: {reason}"),
                                });
                                continue;
                            }
                            tracing::warn!(
                                channel = %channel.name,
                                connector = %connector_name,
                                reason = %reason,
                                "Cache connector unavailable, falling back to in-memory"
                            );
                            Some(cache_pool.memory())
                        }
                    }
                } else if let Some(ref default_cache) = cluster_default_cache {
                    Some(default_cache.clone())
                } else {
                    Some(cache_pool.memory())
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

    async fn cluster_runtime(pool: crate::storage::DbPool) -> Arc<crate::cluster::ClusterRuntime> {
        let cache_pool = CachePool::new(4, 60);
        Arc::new(crate::cluster::ClusterRuntime {
            enabled: true,
            instance_id: "test-node".to_string(),
            redis: None,
            default_cache: Some(cache_pool.memory()),
            repo: Arc::new(crate::storage::repositories::cluster::SqlClusterRepository::new(pool)),
            last_seen_epoch: std::sync::atomic::AtomicI64::new(0),
            last_seen_breaker_epoch: std::sync::atomic::AtomicI64::new(0),
        })
    }

    async fn sqlite_pool() -> crate::storage::DbPool {
        crate::storage::init_pool(&crate::config::StorageConfig {
            url: "sqlite::memory:".to_string(),
            max_connections: 1,
            ..Default::default()
        })
        .await
        .expect("test pool")
    }

    #[tokio::test]
    async fn test_cluster_strict_mode_refuses_broken_dedup_connector() {
        let registry = ChannelRegistry::with_cluster(cluster_runtime(sqlite_pool().await).await);
        let channel = test_channel(
            "strict-ch",
            r#"{"deduplication": {"header": "idem", "connector": "missing-connector"}}"#,
        );
        let issues = registry
            .reload(
                &[channel],
                &ConnectorRegistry::new(crate::config::EngineConfig::default().circuit_breaker),
                &CachePool::new(4, 60),
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
        let registry = ChannelRegistry::with_cluster(cluster_runtime(sqlite_pool().await).await);
        let channel = test_channel("shared-ch", r#"{"deduplication": {"header": "idem"}}"#);
        let issues = registry
            .reload(
                &[channel],
                &ConnectorRegistry::new(crate::config::EngineConfig::default().circuit_breaker),
                &CachePool::new(4, 60),
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
                &CachePool::new(4, 60),
                &DatalogicEngine::new(),
                &TracingStorageConfig::default(),
            )
            .await;
        // Historical behavior preserved: warn + in-memory fallback, loaded.
        assert!(issues.is_empty());
        assert!(registry.get_by_name("fallback-ch").await.is_some());
    }
}
