//! Multi-instance (HA) coordination runtime.
//!
//! One [`ClusterRuntime`] lives on `AppState`. With `cluster.enabled = false`
//! (the default) it is inert: no Redis connection, no shared backends — the
//! only observable artifact is the boot-time instance id. When enabled it
//! carries the shared Redis handle, the default shared cache backend, the
//! [`ClusterRepository`] used for epoch/lease coordination, and the last
//! epoch values this node has applied.

use std::sync::Arc;
use std::sync::atomic::AtomicI64;

use crate::config::ClusterConfig;
use crate::connector::cache_backend::CacheBackend;
use crate::errors::OrionError;
use crate::storage::DbPool;
use crate::storage::repositories::cluster::{ClusterRepository, SqlClusterRepository};

pub mod epoch_watcher;

pub use epoch_watcher::start_cluster_tasks;

pub struct ClusterRuntime {
    /// Mirrors `cluster.enabled`.
    pub enabled: bool,
    /// This node's identity: `cluster.instance_id` or a boot-time UUID.
    /// Ephemeral by design (D4) — nothing registers or depends on a stable
    /// node list; it names lease holders, DLQ claimants, Kafka static
    /// membership, and log lines.
    pub instance_id: String,
    /// Shared Redis connection from `cluster.redis_url` (None when disabled).
    pub redis: Option<redis::aio::MultiplexedConnection>,
    /// Default shared cache backend (dedup/response-cache) on that Redis.
    pub default_cache: Option<Arc<dyn CacheBackend>>,
    /// Epoch/lease coordination repository (always present; harmless when
    /// disabled — the epoch tables exist on every backend).
    pub repo: Arc<dyn ClusterRepository>,
    /// Highest config epoch this node has already applied (its own bumps
    /// count as applied — the inline reload happens before the bump).
    pub last_seen_epoch: AtomicI64,
    /// Highest breaker epoch this node has already applied.
    pub last_seen_breaker_epoch: AtomicI64,
}

/// Build the cluster runtime. When enabled, connects the shared Redis and
/// fails fast on any error (a cluster node without its coordination Redis
/// must not serve). When disabled, performs no I/O.
pub async fn init_cluster_runtime(
    config: &ClusterConfig,
    pool: &DbPool,
) -> Result<Arc<ClusterRuntime>, OrionError> {
    let instance_id = if config.instance_id.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        config.instance_id.clone()
    };

    let (redis, default_cache) = if config.enabled {
        let client =
            redis::Client::open(config.redis_url.as_str()).map_err(|e| OrionError::Config {
                message: format!("cluster.redis_url is invalid: {e}"),
            })?;
        let conn = client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| OrionError::InternalSource {
                context: "Failed to connect to cluster Redis (cluster.redis_url)".to_string(),
                source: Box::new(e),
            })?;
        let cache: Arc<dyn CacheBackend> = Arc::new(
            crate::connector::cache_backend::RedisCacheBackend::new(conn.clone()),
        );
        (Some(conn), Some(cache))
    } else {
        (None, None)
    };

    let repo: Arc<dyn ClusterRepository> = Arc::new(SqlClusterRepository::new(pool.clone()));

    // Seed last-seen epochs with the current DB values: this runs BEFORE the
    // initial channel/workflow load, so anything already counted is included
    // in that load, and any bump that lands after this read correctly
    // triggers a watcher resync.
    let (epoch, breaker_epoch) = if config.enabled {
        let row = repo.get_epoch().await?;
        (row.epoch, row.breaker_epoch)
    } else {
        (0, 0)
    };

    Ok(Arc::new(ClusterRuntime {
        enabled: config.enabled,
        instance_id,
        redis,
        default_cache,
        repo,
        last_seen_epoch: AtomicI64::new(epoch),
        last_seen_breaker_epoch: AtomicI64::new(breaker_epoch),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn sqlite_pool() -> DbPool {
        crate::storage::init_pool(&crate::config::StorageConfig {
            url: "sqlite::memory:".to_string(),
            max_connections: 1,
            ..Default::default()
        })
        .await
        .expect("test pool")
    }

    #[tokio::test]
    async fn test_disabled_runtime_is_inert() {
        let runtime = init_cluster_runtime(&ClusterConfig::default(), &sqlite_pool().await)
            .await
            .expect("disabled runtime never fails");
        assert!(!runtime.enabled);
        assert!(runtime.redis.is_none());
        assert!(runtime.default_cache.is_none());
        assert_eq!(runtime.instance_id.len(), 36); // generated UUID
    }

    #[tokio::test]
    async fn test_configured_instance_id_wins() {
        let config = ClusterConfig {
            instance_id: "node-7".to_string(),
            ..Default::default()
        };
        let runtime = init_cluster_runtime(&config, &sqlite_pool().await)
            .await
            .expect("runtime");
        assert_eq!(runtime.instance_id, "node-7");
    }
}
