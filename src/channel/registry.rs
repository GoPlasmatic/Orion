use std::collections::HashMap;
use std::sync::Arc;

use datalogic_rs::CompiledLogic;
use tokio::sync::{RwLock, Semaphore};

use super::KeyedLimiter;
use super::build_keyed_limiter;
use super::config::ChannelConfig;
use super::routing::{RouteMatch, RouteTable};

use crate::connector::ConnectorConfig;
use crate::connector::ConnectorRegistry;
use crate::connector::cache_backend::{CacheBackend, CachePool};
use crate::storage::models::Channel;

/// Runtime state for a single active channel.
pub struct ChannelRuntimeConfig {
    /// The channel DB model.
    pub channel: Channel,
    /// Parsed per-channel configuration.
    pub parsed_config: ChannelConfig,
    /// Per-channel rate limiter, built from `parsed_config.rate_limit` if configured.
    pub rate_limiter: Option<Arc<KeyedLimiter>>,
    /// Pre-compiled JSONLogic expression for computing the rate limit key.
    pub rate_limit_key_logic: Option<CompiledLogic>,
    /// Pre-compiled JSONLogic expression for input validation.
    /// Evaluated against request data — truthy = pass, falsy = 400 reject.
    pub validation_logic: Option<CompiledLogic>,
    /// Per-channel concurrency limiter for backpressure.
    /// Limits max in-flight requests — returns 503 when exhausted.
    pub backpressure_semaphore: Option<Arc<Semaphore>>,
    /// Per-channel deduplication backend for idempotent request handling.
    /// Can be backed by in-memory DashMap or Redis, depending on channel config.
    pub dedup_store: Option<Arc<dyn CacheBackend>>,
    /// Per-channel response cache backend.
    /// When set, sync responses are cached with a configurable TTL.
    pub response_cache: Option<Arc<dyn CacheBackend>>,
}

/// In-memory registry of active channels, rebuilt on engine reload.
/// Mirrors the ConnectorRegistry pattern.
pub struct ChannelRegistry {
    by_name: RwLock<HashMap<String, Arc<ChannelRuntimeConfig>>>,
    route_table: RwLock<RouteTable>,
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
    pub async fn reload(
        &self,
        channels: &[Channel],
        connector_registry: &ConnectorRegistry,
        cache_pool: &CachePool,
    ) {
        let mut new_map = HashMap::new();
        for channel in channels {
            let parsed_config: ChannelConfig =
                serde_json::from_str(&channel.config_json).unwrap_or_default();

            let rate_limiter = parsed_config.rate_limit.as_ref().map(|rl| {
                let burst = rl.burst.unwrap_or(rl.requests_per_second / 2 + 1);
                Arc::new(build_keyed_limiter(rl.requests_per_second, burst))
            });

            let rate_limit_key_logic = parsed_config
                .rate_limit
                .as_ref()
                .and_then(|rl| rl.key_logic.as_ref())
                .and_then(|logic| {
                    CompiledLogic::compile(logic)
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
                CompiledLogic::compile(logic)
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

            let dedup_store: Option<Arc<dyn CacheBackend>> =
                if let Some(ref dedup) = parsed_config.deduplication {
                    if let Some(ref connector_name) = dedup.connector {
                        // Use the configured cache connector for dedup
                        match connector_registry.get(connector_name).await {
                            Some(cfg) => match cfg.as_ref() {
                                ConnectorConfig::Cache(cache_cfg) => {
                                    match cache_pool.get_backend(connector_name, cache_cfg).await {
                                        Ok(backend) => Some(backend),
                                        Err(e) => {
                                            tracing::warn!(
                                                channel = %channel.name,
                                                connector = %connector_name,
                                                error = %e,
                                                "Failed to create dedup backend from connector, \
                                                 falling back to in-memory"
                                            );
                                            Some(cache_pool.memory())
                                        }
                                    }
                                }
                                _ => {
                                    tracing::warn!(
                                        channel = %channel.name,
                                        connector = %connector_name,
                                        "Dedup connector is not a cache connector, \
                                         falling back to in-memory"
                                    );
                                    Some(cache_pool.memory())
                                }
                            },
                            None => {
                                tracing::warn!(
                                    channel = %channel.name,
                                    connector = %connector_name,
                                    "Dedup connector not found, falling back to in-memory"
                                );
                                Some(cache_pool.memory())
                            }
                        }
                    } else {
                        // No connector specified — use built-in in-memory
                        Some(cache_pool.memory())
                    }
                } else {
                    None
                };

            // Resolve response cache backend (same pattern as dedup)
            let response_cache: Option<Arc<dyn CacheBackend>> = if let Some(ref cache_cfg) =
                parsed_config.cache
                && cache_cfg.enabled
            {
                if let Some(ref connector_name) = cache_cfg.connector {
                    match connector_registry.get(connector_name).await {
                        Some(cfg) => match cfg.as_ref() {
                            ConnectorConfig::Cache(cc) => {
                                match cache_pool.get_backend(connector_name, cc).await {
                                    Ok(backend) => Some(backend),
                                    Err(e) => {
                                        tracing::warn!(
                                            channel = %channel.name,
                                            connector = %connector_name,
                                            error = %e,
                                            "Failed to create cache backend from connector, \
                                             falling back to in-memory"
                                        );
                                        Some(cache_pool.memory())
                                    }
                                }
                            }
                            _ => {
                                tracing::warn!(
                                    channel = %channel.name,
                                    connector = %connector_name,
                                    "Cache connector is not a cache connector, \
                                     falling back to in-memory"
                                );
                                Some(cache_pool.memory())
                            }
                        },
                        None => {
                            tracing::warn!(
                                channel = %channel.name,
                                connector = %connector_name,
                                "Cache connector not found, falling back to in-memory"
                            );
                            Some(cache_pool.memory())
                        }
                    }
                } else {
                    Some(cache_pool.memory())
                }
            } else {
                None
            };

            let runtime = Arc::new(ChannelRuntimeConfig {
                channel: channel.clone(),
                parsed_config,
                rate_limiter,
                rate_limit_key_logic,
                validation_logic,
                backpressure_semaphore,
                dedup_store,
                response_cache,
            });
            new_map.insert(channel.name.clone(), runtime);
        }
        *self.by_name.write().await = new_map;

        // Rebuild the REST route table from active channels
        *self.route_table.write().await = RouteTable::build(channels);
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
}
