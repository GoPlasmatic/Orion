//! Multi-instance (HA) coordination runtime.
//!
//! One [`ClusterRuntime`] lives on `AppState`. With `cluster.enabled = false`
//! (the default) there is no shared backend. Deployment reconciliation still
//! runs so a failed inline build can recover. When enabled it carries the
//! default shared cache backend (over the state database), the
//! [`ClusterRepository`] used for breaker-reset and lease coordination, and
//! the last breaker epoch this node has applied.

use std::sync::Arc;
use std::sync::atomic::AtomicI64;

use crate::config::ClusterConfig;
use crate::connector::cache_backend::CacheBackend;
use crate::errors::OrionError;
use crate::storage::DbPool;
use crate::storage::repositories::cluster::{ClusterRepository, SqlClusterRepository};
use crate::storage::repositories::shared_state::SqlSharedState;

pub mod job_lease;
pub mod reconcile;
pub mod shared_store;

pub use job_lease::JobLeaseGate;
pub use reconcile::{reconcile_v2_state_once, start_cluster_tasks, start_v2_reconciler};

pub struct ClusterRuntime {
    /// Mirrors `cluster.enabled`.
    pub enabled: bool,
    /// This node's identity: `cluster.instance_id` or a boot-time UUID.
    /// Ephemeral by design (D4) — nothing registers or depends on a stable
    /// node list; it names lease holders, DLQ claimants and log lines.
    pub instance_id: String,
    /// The shared state database (`cluster_kv`, `cluster_rate_windows`) the
    /// fleet's dedup claims, response cache and rate-limit windows live in.
    /// `None` when disabled: a single node keeps all three in process.
    pub shared: Option<SqlSharedState>,
    /// Default shared cache backend (dedup/response-cache) on that store.
    pub default_cache: Option<Arc<dyn CacheBackend>>,
    /// Breaker-reset/lease coordination repository (always present; harmless
    /// when disabled — the tables exist on every backend).
    pub repo: Arc<dyn ClusterRepository>,
    /// Highest breaker epoch this node has already applied.
    pub last_seen_breaker_epoch: AtomicI64,
}

impl From<&ClusterRuntime> for crate::channel::registry::ClusterBackends {
    fn from(runtime: &ClusterRuntime) -> Self {
        Self {
            default_cache: runtime.default_cache.clone(),
            shared: runtime.shared.clone(),
        }
    }
}

/// Build the cluster runtime. When enabled, the shared stores are built over
/// `pool`, the state database the node has already connected; disabled, it
/// has none. Either way it reads the current breaker epoch.
pub async fn init_cluster_runtime(
    config: &ClusterConfig,
    pool: &DbPool,
) -> Result<Arc<ClusterRuntime>, OrionError> {
    // main.rs pre-resolves the id into the config so tracing agrees with
    // the runtime; test harnesses may leave it empty (fresh UUID).
    let instance_id = config.effective_instance_id();

    // The same database every node already shares for epochs and leases, so
    // a cluster needs no coordination service of its own.
    let (shared, default_cache) = if config.enabled {
        let store = SqlSharedState::new(pool.clone());
        let cache: Arc<dyn CacheBackend> =
            Arc::new(shared_store::SharedCacheBackend::new(store.clone()));
        (Some(store), Some(cache))
    } else {
        (None, None)
    };

    let repo: Arc<dyn ClusterRepository> = Arc::new(SqlClusterRepository::new(pool.clone()));

    // Seed the last-seen breaker epoch with the current DB value, so a reset
    // requested before this node started is not replayed on it.
    let breaker_epoch = repo.get_epoch().await?.breaker_epoch;

    Ok(Arc::new(ClusterRuntime {
        enabled: config.enabled,
        instance_id,
        shared,
        default_cache,
        repo,
        last_seen_breaker_epoch: AtomicI64::new(breaker_epoch),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn sqlite_pool() -> DbPool {
        crate::storage::test_sqlite_pool().await
    }

    #[tokio::test]
    async fn test_disabled_runtime_is_inert() {
        let runtime = init_cluster_runtime(&ClusterConfig::default(), &sqlite_pool().await)
            .await
            .expect("disabled runtime never fails");
        assert!(!runtime.enabled);
        assert!(runtime.shared.is_none());
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
