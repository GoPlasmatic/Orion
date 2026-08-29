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
pub mod job_lease;

pub use epoch_watcher::start_cluster_tasks;
pub use job_lease::JobLeaseGate;

/// What a config-epoch bump changed, so a peer can size its resync.
///
/// The epoch used to be a bare counter. A node answering a bump had no idea
/// what had moved, so it ran the widest resync there is: reload every
/// connector and evict **every** cached SQL, MongoDB and cache pool. One
/// workflow activation was therefore a fleet-wide reconnect storm — every node
/// dropping every pooled connection for a change that touched no connector.
///
/// The scope rides in the same `UPDATE` as the counter, so a reader that sees
/// the new epoch always sees the scope that goes with it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum EpochScope {
    /// Reload the engine and the channel registry. No connector reload and no
    /// pool eviction: nothing about a workflow or a channel row changes which
    /// connectors exist or what they point at.
    Definitions,
    /// A connector was created, updated, deleted or reloaded. Reload the
    /// connector registry and evict the pools, because the endpoint or the
    /// credentials behind a cached connection may now be wrong.
    Connectors,
    /// Everything. The default, and what an unrecognised or absent scope
    /// means — an older node's bump, or a value a newer node writes that this
    /// one does not know. Reading an unknown scope as "resync everything"
    /// is what keeps a mixed-version fleet correct: the cost is the storm
    /// this type exists to avoid, never a missed change.
    #[default]
    All,
}

impl EpochScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Definitions => "definitions",
            Self::Connectors => "connectors",
            Self::All => "all",
        }
    }

    /// Read a scope off the epoch row. Anything unrecognised — including the
    /// empty string a pre-scope writer leaves — is [`Self::All`].
    pub fn parse(raw: &str) -> Self {
        match raw {
            "definitions" => Self::Definitions,
            "connectors" => Self::Connectors,
            _ => Self::All,
        }
    }

    /// Whether a peer answering this scope must reload the connector registry
    /// and drop its cached pools.
    pub fn touches_connectors(self) -> bool {
        matches!(self, Self::Connectors | Self::All)
    }
}

pub struct ClusterRuntime {
    /// Mirrors `cluster.enabled`.
    pub enabled: bool,
    /// This node's identity: `cluster.instance_id` or a boot-time UUID.
    /// Ephemeral by design (D4) — nothing registers or depends on a stable
    /// node list; it names lease holders, DLQ claimants, Kafka static
    /// membership, and log lines.
    pub instance_id: String,
    /// Shared Redis handle from `cluster.redis_url` (None when disabled).
    ///
    /// A `ConnectionManager`, not a `MultiplexedConnection`: this handle is
    /// cloned into the default cache backend and every channel's rate
    /// limiter, and a multiplexed connection does not re-establish itself, so
    /// one Redis restart would break shared dedup (failing open), the shared
    /// response cache, and cluster rate limiting on every node until the pods
    /// were restarted.
    pub redis: Option<redis::aio::ConnectionManager>,
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
    /// Set when a bump failed after its mutation was already committed and
    /// live on this node, cleared by the next successful bump. Reported as
    /// the `config_propagation` component of `/health`.
    ///
    /// The failure it names is real but not this node's: the change is
    /// serving here and the peers have not been told. Nothing a client can do
    /// with a 500 helps — the row is written, so a retry is a duplicate
    /// version or a 409 — and nothing else notices, because the watcher on a
    /// peer only ever sees an epoch that did not move. So it is a node-health
    /// signal instead of a per-request error.
    propagation_degraded: std::sync::atomic::AtomicBool,
}

impl ClusterRuntime {
    /// Advance the config epoch after a successfully applied local mutation
    /// (the send side of the epoch bus; the watcher is the receive side).
    /// Runs even with cluster disabled (keeps the counter monotonic so
    /// enabling cluster later starts sane) but only propagates failures when
    /// enabled — on a single node a failed bump changes nothing, while in a
    /// cluster it means the change did NOT propagate and the caller must
    /// surface the error.
    pub async fn bump_config_epoch(&self, scope: EpochScope) {
        use std::sync::atomic::Ordering;
        match self.repo.bump_epoch(scope.as_str()).await {
            Ok(epoch) => {
                // fetch_max, not store: the inline reload already applied this
                // node's own change, but a concurrently observed higher epoch
                // must never be masked.
                self.last_seen_epoch.fetch_max(epoch, Ordering::AcqRel);
                self.propagation_degraded.store(false, Ordering::Release);
            }
            Err(e) if self.enabled => {
                // Not returned to the caller. The mutation is committed and
                // serving on this node; a 500 would tell the client its change
                // failed when it did not, and its retry writes a second
                // version or collides with the first. The peers are the ones
                // in trouble, and they cannot see it — a watcher polling an
                // epoch that did not move looks exactly like a quiet fleet —
                // so it surfaces here instead.
                self.propagation_degraded.store(true, Ordering::Release);
                crate::metrics::record_error("config_epoch_bump");
                tracing::error!(
                    error = %e,
                    scope = scope.as_str(),
                    "Failed to advance the config epoch: this node's change is live \
                     but peers will not see it until a later bump succeeds"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to bump config epoch (cluster disabled — ignored)");
            }
        }
    }

    /// Whether a bump has failed since the last successful one — the
    /// `config_propagation` component of `/health`. Always false outside
    /// cluster mode, where there is nothing to propagate to.
    pub fn propagation_degraded(&self) -> bool {
        self.propagation_degraded
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

impl From<&ClusterRuntime> for crate::channel::registry::ClusterBackends {
    fn from(runtime: &ClusterRuntime) -> Self {
        Self {
            default_cache: runtime.default_cache.clone(),
            redis: runtime.redis.clone(),
        }
    }
}

/// Build the cluster runtime. When enabled, connects the shared Redis and
/// fails fast on any error (a cluster node without its coordination Redis
/// must not serve). When disabled, performs no I/O.
pub async fn init_cluster_runtime(
    config: &ClusterConfig,
    pool: &DbPool,
) -> Result<Arc<ClusterRuntime>, OrionError> {
    // main.rs pre-resolves the id into the config so tracing/Kafka agree
    // with the runtime; test harnesses may leave it empty (fresh UUID).
    let instance_id = config.effective_instance_id();

    let (redis, default_cache) = if config.enabled {
        let client =
            redis::Client::open(config.redis_url.as_str()).map_err(|e| OrionError::Config {
                message: format!("cluster.redis_url is invalid: {e}"),
            })?;
        // Eager connect: a cluster node whose coordination Redis is
        // unreachable at boot must fail fast rather than start degraded.
        let conn = client
            .get_connection_manager()
            .await
            .map_err(|e| OrionError::Internal {
                context: "Failed to connect to cluster Redis (cluster.redis_url)".to_string(),
                source: Some(Box::new(e)),
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
        propagation_degraded: std::sync::atomic::AtomicBool::new(false),
    }))
}

#[cfg(test)]
mod tests {
    use super::EpochScope;

    /// The round trip a bump and a watcher make through the `epoch_scope`
    /// column.
    #[test]
    fn a_scope_survives_the_column() {
        for scope in [
            EpochScope::Definitions,
            EpochScope::Connectors,
            EpochScope::All,
        ] {
            assert_eq!(EpochScope::parse(scope.as_str()), scope);
        }
    }

    /// The rolling-deploy rule. A node running the previous release bumps the
    /// epoch without writing a scope, and a future release may write one this
    /// node has never heard of. Both must read as "resync everything": the
    /// cost is the reconnect storm the scope exists to avoid, never a change
    /// this node fails to apply.
    #[test]
    fn an_absent_or_unknown_scope_resyncs_everything() {
        assert_eq!(EpochScope::parse(""), EpochScope::All);
        assert_eq!(EpochScope::parse("something-newer"), EpochScope::All);
        assert_eq!(EpochScope::default(), EpochScope::All);
        assert!(EpochScope::All.touches_connectors());
    }

    /// The whole point: a channel or workflow change must not drop a single
    /// pooled connection anywhere in the fleet.
    #[test]
    fn a_definitions_change_leaves_the_connector_pools_alone() {
        assert!(!EpochScope::Definitions.touches_connectors());
        assert!(EpochScope::Connectors.touches_connectors());
    }
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
