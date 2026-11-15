//! Multi-instance (HA) coordination runtime.
//!
//! One [`ClusterRuntime`] lives on `AppState`. With `cluster.enabled = false`
//! (the default) there is no shared store: a single node keeps dedup claims,
//! the response cache and rate limits in process. Deployment reconciliation
//! still runs so a failed inline build can recover. When enabled it carries
//! the cluster's [`SharedStateStore`] (the store an edition composed), the
//! default shared cache backend over it, the [`ClusterRepository`] used for
//! breaker-reset and lease coordination, and the last breaker epoch this
//! node has applied. Breaker epochs, job leases, DLQ claims and the config
//! epoch always stay on the state database, whatever the shared store is.
//!
//! **Cluster mode is not Community's.** Community ships no cluster store, so
//! `cluster.enabled = true` boots only when an edition registered one
//! ([`CompositionBuilder::with_cluster_store`](crate::extensions::CompositionBuilder::with_cluster_store));
//! Orion Standard and Enterprise compose a Redis store. A Community node
//! serves alone.

use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::time::Duration;

use orion_extension_api::SharedStateStore;

use crate::config::AppConfig;
use crate::connector::cache_backend::CacheBackend;
use crate::errors::OrionError;
use crate::storage::DbPool;
use crate::storage::repositories::cluster::{ClusterRepository, SqlClusterRepository};

pub mod reconcile;
pub mod shared_store;

pub use crate::storage::repositories::job_lease::JobLeaseGate;
pub use reconcile::{reconcile_v2_state_once, start_cluster_tasks, start_v2_reconciler};

pub struct ClusterRuntime {
    /// Mirrors `cluster.enabled`.
    pub enabled: bool,
    /// This node's identity: `cluster.instance_id` or a boot-time UUID.
    /// Ephemeral by design (D4) — nothing registers or depends on a stable
    /// node list; it names lease holders, DLQ claimants and log lines.
    pub instance_id: String,
    /// The store the fleet's dedup claims, response cache and rate-limit
    /// windows live in: the edition's. `None` when disabled: a single node
    /// keeps all three in process.
    pub shared: Option<Arc<dyn SharedStateStore>>,
    /// Whether `shared` is set: the store is then a readiness component of
    /// its own (`/readyz`), beside the database.
    pub external_store: bool,
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

/// Build the cluster runtime.
///
/// When `cluster.enabled`, the shared store is `store` (an edition's); with
/// none the node does not boot ([`NO_CLUSTER_STORE`]). The store is pinged
/// first, bounded by
/// `engine.health_check_timeout_secs`: one that does not answer fails the
/// boot, naming it, rather than letting every dedup claim fail open.
/// Disabled, there is no shared store and `store` is unused. Either way it
/// reads the current breaker epoch.
pub async fn init_cluster_runtime(
    config: &AppConfig,
    pool: &DbPool,
    store: Option<Arc<dyn SharedStateStore>>,
) -> Result<Arc<ClusterRuntime>, OrionError> {
    // main.rs pre-resolves the id into the config so tracing agrees with
    // the runtime; test harnesses may leave it empty (fresh UUID).
    let instance_id = config.cluster.effective_instance_id();

    let (shared, external_store) = match (config.cluster.enabled, store) {
        (false, _) => (None, false),
        (true, Some(store)) => {
            ping_at_boot(
                store.as_ref(),
                Duration::from_secs(config.engine.health_check_timeout_secs),
            )
            .await?;
            (Some(store), true)
        }
        // Community ships no cluster store: fleet-wide dedup, rate limits
        // and the response cache need one an edition composes.
        (true, None) => return Err(OrionError::validation(NO_CLUSTER_STORE)),
    };
    let default_cache = shared.clone().map(|store| {
        Arc::new(shared_store::SharedCacheBackend::new(store)) as Arc<dyn CacheBackend>
    });

    let repo: Arc<dyn ClusterRepository> = Arc::new(SqlClusterRepository::new(pool.clone()));

    // Seed the last-seen breaker epoch with the current DB value, so a reset
    // requested before this node started is not replayed on it.
    let breaker_epoch = repo.get_epoch().await?.breaker_epoch;

    Ok(Arc::new(ClusterRuntime {
        enabled: config.cluster.enabled,
        instance_id,
        shared,
        external_store,
        default_cache,
        repo,
        last_seen_breaker_epoch: AtomicI64::new(breaker_epoch),
    }))
}

/// Why a node with `cluster.enabled = true` and no composed store does not
/// boot.
pub const NO_CLUSTER_STORE: &str = "cluster.enabled = true needs a cluster store, and Community ships none: \
     cluster mode is an Orion Standard and Enterprise feature (their Redis store, \
     [extensions.redis]). Set cluster.enabled = false to run this node alone";

/// An edition's store must answer before the node serves on it.
async fn ping_at_boot(store: &dyn SharedStateStore, timeout: Duration) -> Result<(), OrionError> {
    let name = store.name();
    match tokio::time::timeout(timeout, store.ping()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(OrionError::internal(format!(
            "cluster store '{name}' did not answer at boot: {}",
            e.message()
        ))),
        Err(_) => Err(OrionError::internal(format!(
            "cluster store '{name}' did not answer at boot within {}s \
             (engine.health_check_timeout_secs)",
            timeout.as_secs()
        ))),
    }
}

/// A store standing in for an edition's, for this crate's unit tests: it
/// answers its ping as told, and nothing else is called on it.
#[cfg(test)]
pub(crate) mod test_store {
    use std::sync::Arc;

    use orion_extension_api::{SharedStateStore, StoreError};

    pub(crate) struct Fake {
        pub(crate) name: &'static str,
        pub(crate) ping: Result<(), StoreError>,
        pub(crate) hangs: bool,
        /// What `needs_sweep` answers.
        pub(crate) sweeps: bool,
    }

    #[async_trait::async_trait]
    impl SharedStateStore for Fake {
        fn name(&self) -> &str {
            self.name
        }
        async fn get(&self, _: &str) -> Result<Option<String>, StoreError> {
            unreachable!("not called here")
        }
        async fn get_many(&self, _: &[String]) -> Result<Vec<Option<String>>, StoreError> {
            unreachable!("not called here")
        }
        async fn set(&self, _: &str, _: &str, _: Option<u64>) -> Result<(), StoreError> {
            unreachable!("not called here")
        }
        async fn remove_many(&self, _: &[String]) -> Result<u64, StoreError> {
            unreachable!("not called here")
        }
        async fn claim(&self, _: &str, _: &str, _: u64) -> Result<Option<String>, StoreError> {
            unreachable!("not called here")
        }
        async fn incr_by(&self, _: &str, _: i64, _: Option<u64>) -> Result<i64, StoreError> {
            unreachable!("not called here")
        }
        async fn hit_window(&self, _: &str) -> Result<i64, StoreError> {
            unreachable!("not called here")
        }
        async fn ping(&self) -> Result<(), StoreError> {
            if self.hangs {
                std::future::pending::<()>().await;
            }
            self.ping.clone()
        }
        fn needs_sweep(&self) -> bool {
            self.sweeps
        }
    }

    /// A store named `fake` that answers its ping with `ping`.
    pub(crate) fn fake(ping: Result<(), StoreError>) -> Arc<dyn SharedStateStore> {
        Arc::new(Fake {
            name: "fake",
            ping,
            hangs: false,
            sweeps: false,
        })
    }

    /// A healthy store named `name`.
    pub(crate) fn named(name: &'static str) -> Arc<dyn SharedStateStore> {
        Arc::new(Fake {
            name,
            ping: Ok(()),
            hangs: false,
            sweeps: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::test_store::{Fake, fake};
    use super::*;
    use orion_extension_api::StoreError;

    pub(super) async fn sqlite_pool() -> DbPool {
        crate::storage::test_sqlite_pool().await
    }

    fn enabled() -> AppConfig {
        let mut config = AppConfig::default();
        config.cluster.enabled = true;
        config
    }

    #[tokio::test]
    async fn test_disabled_runtime_is_inert() {
        let runtime = init_cluster_runtime(&AppConfig::default(), &sqlite_pool().await, None)
            .await
            .expect("disabled runtime never fails");
        assert!(!runtime.enabled);
        assert!(runtime.shared.is_none());
        assert!(!runtime.external_store);
        assert!(runtime.default_cache.is_none());
        assert_eq!(runtime.instance_id.len(), 36); // generated UUID
    }

    #[tokio::test]
    async fn test_configured_instance_id_wins() {
        let mut config = AppConfig::default();
        config.cluster.instance_id = "node-7".to_string();
        let runtime = init_cluster_runtime(&config, &sqlite_pool().await, None)
            .await
            .expect("runtime");
        assert_eq!(runtime.instance_id, "node-7");
    }

    #[tokio::test]
    async fn an_enabled_cluster_with_no_store_does_not_boot() {
        let Err(refused) = init_cluster_runtime(&enabled(), &sqlite_pool().await, None).await
        else {
            unreachable!("Community has no cluster store to fall back on");
        };
        assert!(refused.to_string().contains(NO_CLUSTER_STORE), "{refused}");
    }

    #[tokio::test]
    async fn an_injected_store_replaces_the_database_store() {
        let runtime = init_cluster_runtime(&enabled(), &sqlite_pool().await, Some(fake(Ok(()))))
            .await
            .expect("runtime");
        let shared = runtime.shared.as_ref().expect("a shared store");
        assert_eq!(shared.name(), "fake");
        assert!(runtime.external_store);
        assert!(runtime.default_cache.is_some());
    }

    #[tokio::test]
    async fn a_store_that_does_not_answer_fails_the_boot() {
        let Err(refused) = init_cluster_runtime(
            &enabled(),
            &sqlite_pool().await,
            Some(fake(Err(StoreError::unavailable("connection refused")))),
        )
        .await
        else {
            unreachable!("an unreachable store fails the boot");
        };
        let message = refused.to_string();
        assert!(
            message.contains("'fake'") && message.contains("connection refused"),
            "{message}"
        );

        let mut config = enabled();
        config.engine.health_check_timeout_secs = 1;
        let hung: Arc<dyn SharedStateStore> = Arc::new(Fake {
            name: "fake",
            ping: Ok(()),
            hangs: true,
            sweeps: false,
        });
        let Err(refused) = init_cluster_runtime(&config, &sqlite_pool().await, Some(hung)).await
        else {
            unreachable!("a store that never answers fails the boot");
        };
        assert!(refused.to_string().contains("within 1s"), "{refused}");
    }

    #[tokio::test]
    async fn a_store_on_a_disabled_cluster_is_unused() {
        // Not even pinged: this one would fail the boot if it were.
        let runtime = init_cluster_runtime(
            &AppConfig::default(),
            &sqlite_pool().await,
            Some(fake(Err(StoreError::unavailable("down")))),
        )
        .await
        .expect("a disabled cluster ignores the store");
        assert!(runtime.shared.is_none());
        assert!(!runtime.external_store);
        assert!(runtime.default_cache.is_none());
    }
}
