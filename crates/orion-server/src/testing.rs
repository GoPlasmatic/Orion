//! A real node for an edition's integration tests.
//!
//! [`TestNode::start`] builds a node over an edition's own
//! [`CompositionBuilder`] through [`crate::bootstrap::assemble`], the same
//! ordered boot sequence [`crate::run`] uses, so a channel driver or provider
//! backend under test is composed, frozen and served exactly as it would be in
//! production. Drive it through [`TestNode::router`] (an in-process
//! `axum::Router`; `tower::ServiceExt::oneshot` sends one request) and deploy
//! to it through the v2 admin API, as an operator would.
//!
//! Where it differs from a production boot, deliberately:
//!
//! 1. **Storage.** An unset or default `storage.url` becomes in-memory SQLite,
//!    with a pool of at least five connections, migrated on open. The
//!    override is not written back: [`TestNode::state`] keeps the config the
//!    test passed.
//! 2. **Metrics.** The Prometheus recorder is process-global, so the first
//!    node in a test process installs it and later ones get a standalone
//!    handle. No tracing subscriber is installed; a test that wants logs
//!    installs its own.
//! 3. **No DLQ retry loop.** A node-owned poller would claim DLQ rows out from
//!    under a test that drives retries itself.
//! 4. **No serving tasks.** What [`crate::run`] spawns after `assemble`
//!    (cluster tasks, the limiter prune, the HTTP listener) is the test's to
//!    start when it wants them.
//!
//! [`test_config`] is the default config with trace persistence pinned to
//! `sync`, so a trace is committed before its response returns.

use std::sync::Arc;

use crate::bootstrap::{AssembleParams, BootstrapError, TaskHandles};
use crate::config::{AppConfig, StorageConfig, TraceStorageMode};
use crate::extensions::CompositionBuilder;
use crate::runtime::state::AppState;
use crate::storage::DbPool;

/// The default config, with trace persistence pinned to `sync`: a test that
/// submits a request and then reads its trace needs the row written before
/// the response returns, and under `batch` that read would race the flush.
pub fn test_config() -> AppConfig {
    let mut config = AppConfig::default();
    config.trace_storage.mode = TraceStorageMode::Sync;
    config
}

/// A node assembled for a test: its state, the background tasks it started
/// and the state database it runs on. See the [module](self) documentation.
pub struct TestNode {
    state: AppState,
    handles: TaskHandles,
    pool: DbPool,
}

impl TestNode {
    /// Assemble a node serving `composition` under `config`.
    pub async fn start(
        mut config: AppConfig,
        composition: CompositionBuilder,
    ) -> Result<Self, BootstrapError> {
        let storage = if config.storage.url.is_empty()
            || config.storage.url == StorageConfig::default().url
        {
            StorageConfig {
                url: "sqlite::memory:".to_string(),
                max_connections: 5,
                ..config.storage.clone()
            }
        } else {
            StorageConfig {
                max_connections: config.storage.max_connections.max(5),
                ..config.storage.clone()
            }
        };
        let pool = crate::storage::init_pool(&storage).await?;

        config.trace_queue.dlq_retry_enabled = false;

        // Installed before `assemble`, as production installs it, so the
        // metrics the build sequence emits land on a live recorder.
        let metrics_handle = crate::metrics::init_metrics_with_instance(None);

        let assembled = crate::bootstrap::assemble(
            AssembleParams {
                config: Arc::new(config),
                pool: pool.clone(),
                metrics_handle,
            },
            composition,
        )
        .await?;
        Ok(Self {
            state: assembled.state,
            handles: assembled.task_handles,
            pool,
        })
    }

    /// The node's full HTTP surface (admin, data plane, operational routes),
    /// as the server serves it.
    pub fn router(&self) -> crate::server::Router {
        crate::server::build_router(self.state.clone())
    }

    /// The node's shared state.
    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// The state database the node runs on.
    pub fn pool(&self) -> &DbPool {
        &self.pool
    }

    /// Stop the node's background work in production order: the trace
    /// workers, the persistence queue, then the audit writer.
    pub async fn shutdown(self) {
        let Self { state, handles, .. } = self;
        drop(state);
        handles.shutdown().await;
    }

    /// The parts, for a test that owns the node's lifecycle itself.
    pub fn into_parts(self) -> (AppState, TaskHandles, DbPool) {
        (self.state, self.handles, self.pool)
    }
}
