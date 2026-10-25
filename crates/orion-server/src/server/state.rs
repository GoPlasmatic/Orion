use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use dataflow_rs::expr::ExprEngine;
use metrics_exporter_prometheus::PrometheusHandle;

use crate::config::AppConfig;
use crate::connector::ConnectorRegistry;
use crate::connector::cache_backend::CachePool;
use crate::queue::TraceQueue;
use crate::server::rate_limit::RateLimitState;
use crate::storage::DbPool;
use crate::storage::repositories::Repositories;

/// Shared cache and external connection-pool caches, grouped (R26).
pub struct Caches {
    /// In-memory cache backends for workflow cache functions, dedup stores
    /// and response caches.
    pub cache_pool: Arc<CachePool>,
    /// External SQL connection pool cache — shared so admin routes can evict stale pools.
    pub sql_pool_cache: Arc<crate::connector::pool_cache::SqlPoolCache>,
    /// Extensible provider-owned connection-pool caches (`V2-11.1`, `V2-05.4`).
    pub provider_pools: crate::extensions::ResourcePoolRegistry,
}

/// Owned fields shared across all route handlers.
///
/// Wrapped in an `Arc` (via the [`AppState`] type alias) so the per-request
/// clone Axum performs on `State<AppState>` is a single atomic refcount bump
/// rather than one per `Arc` field (~20+).
///
/// R26: the coherent clusters are grouped — [`Repositories`] (`repos`),
/// [`Caches`] (`caches`); everything genuinely
/// runtime-singular stays a flat field.
pub struct AppStateInner {
    /// The live serving generation — engine and channel estate, published as
    /// one value. Every ingress loads it once per unit of work; the reload
    /// republishes it. Was two fields, `engine` and `channel_registry`, each
    /// swapped separately (see [`crate::runtime::generation`]).
    pub runtime: Arc<crate::runtime::RuntimeHandle>,
    /// Builds the channel half of a generation. A reload's tool, not a
    /// request's: a request reads the estate off `runtime`.
    pub channel_loader: Arc<crate::channel::ChannelLoader>,
    /// Serialises a deployment revision's build end to end.
    ///
    /// A build is a read-modify-write over the published generation: it reads
    /// the revision's members, builds the new engine, builds the channel
    /// estate beside it from the *current* one, and publishes both. Two
    /// callers can start one concurrently (a deployment on this node and the
    /// cluster reconciler), and without this they interleave: whichever
    /// publishes last wins. That is the *older* build often enough to matter,
    /// which leaves a just-deployed channel invisible until the next build.
    ///
    /// This is now the *only* lock in the sequence. `ChannelLoader` used to
    /// keep one of its own, which guarded its half alone and could not span
    /// the engine store — the asymmetry that let the two halves be published a
    /// moment apart.
    pub reload_lock: tokio::sync::Mutex<()>,
    /// Serialises `POST /admin/backups` end to end: choosing the destination
    /// filename, running `VACUUM INTO`, and pruning to the retention count.
    ///
    /// Two backups started in the same second used to pick the same
    /// second-precision filename, and `VACUUM INTO` refuses a destination that
    /// exists — so one of the two failed with a 500. The filename now carries
    /// milliseconds and a collision suffix, which fixes the name; the lock
    /// fixes the rest of it. Concurrent backups are two full copies of the
    /// database competing for the same disk the live database is on, and their
    /// retention prunes race each other over the same files.
    ///
    /// A process-local mutex is sufficient because backups are refused
    /// outright in cluster mode (they would land on one arbitrary node), so
    /// there is never a second process writing this directory.
    pub backup_lock: tokio::sync::Mutex<()>,
    /// `[secrets]`, resolved at startup. Held so the admin plane's per-request
    /// engines (the workflow test endpoint) carry the same store the serving
    /// engine does — otherwise "test this workflow" would refuse a definition
    /// that runs fine in production.
    pub secrets: Arc<crate::engine::ResolvedSecrets>,
    /// `[vars]` as one JSON object, or `None` when the instance declares none.
    ///
    /// Stamped into `metadata.vars` at every ingress, overwriting whatever the
    /// caller sent — envelope mode merges caller-supplied metadata wholesale,
    /// so without that a request could name its own topic prefix. `None`
    /// strips the key instead, which is what makes it unforgeable on an
    /// instance that declares nothing.
    pub vars: Option<Arc<serde_json::Value>>,
    /// The six storage repositories. `repos.trace_dlq` backs the
    /// `/admin/trace-dlq` operator routes (O4) — the same repository
    /// instance the worker pool and the retry loop write to.
    pub repos: Repositories,
    /// Bounded producer for admin audit rows (O7). Admin handlers submit
    /// here; one background writer persists in order and is drained at
    /// shutdown, so a mutation accepted moments before SIGTERM still lands.
    pub audit_queue: crate::queue::audit_queue::AuditQueue,
    pub connector_registry: Arc<ConnectorRegistry>,
    /// Cache backends plus the external SQL connection-pool cache.
    pub caches: Caches,
    pub trace_queue: TraceQueue,
    /// The startup pool. **Route handlers should not reach for this** — go
    /// through [`AppStateInner::pool_stats`] or
    /// [`AppStateInner::backup_sqlite_into`] (R26). It stays public because
    /// bootstrap assembles it and the integration harness seeds rows through
    /// it; the two route-layer call sites that used to unwrap a concrete
    /// `sqlx` pool here now go through those methods instead.
    #[doc(hidden)]
    pub db_pool: DbPool,
    pub config: Arc<AppConfig>,
    pub start_time: chrono::DateTime<chrono::Utc>,
    pub metrics_handle: PrometheusHandle,
    pub http_client: reqwest::Client,
    /// The process's one expression engine (I188): Orion's ten operators,
    /// `[secrets]` and `engine.ops_budget`, built at boot and kept across
    /// every reload. The channel guards, channel auth, cron and `/validate` compile and evaluate on it. Cheap to clone: a clone
    /// shares the engine.
    pub exprs: ExprEngine,
    /// The instance's JWKS cache, built on `http_client` — the client with
    /// `PinnedDnsResolver`. Both verify surfaces (the channel `jwt` auth mode
    /// and the `jwt_verify` task) share it, so a key set is fetched once per
    /// URL per instance and an issuer's rotation is seen by both at once.
    pub jwks: Arc<crate::jwt::jwks::JwksCache>,
    /// The instance's OIDC discovery cache (#355), on the same pinned client.
    /// Read by `oauth2_login` compile for a provider that names an `issuer`; a
    /// reload passes it back through `ReloadDeps`.
    pub oidc_discovery: Arc<crate::channel::oidc_discovery::DiscoveryCache>,
    pub rate_limit_state: Option<Arc<RateLimitState>>,
    /// Startup readiness flag — set to true after engine is fully loaded.
    pub ready: Arc<AtomicBool>,
    /// Whether the last deployment build failed — the `engine_reload`
    /// component of `/health`.
    ///
    /// Set and cleared by the build itself, so every caller is covered: an
    /// apply, a rollback, and the cluster reconciler. Deliberately **not** on
    /// `/readyz`: a node whose build failed is still serving the previous
    /// generation correctly, and taking it out of rotation would replace a
    /// stale-config problem with an availability one.
    pub reload_degraded: Arc<AtomicBool>,
    /// Background queue for trace-storage writes. A no-op handle in sync/off modes.
    pub trace_persistence_queue: crate::queue::TracePersistenceQueue,
    /// Multi-instance coordination runtime. Inert when `cluster.enabled = false`.
    pub cluster: Arc<crate::cluster::ClusterRuntime>,
    /// The node's long-lived background tasks. Held here so `/health` and
    /// `/readyz` can report their liveness — before this, a dead persistence
    /// worker or DLQ retry consumer was invisible to every probe while the
    /// data plane kept answering 200s.
    pub tasks: Arc<crate::runtime::TaskRegistry>,

    /// What the cron scheduler is actually achieving, as distinct from whether
    /// its two loops are alive — which the task registry above already reports.
    /// A reconciler erroring on every pass is alive and doing nothing.
    pub cron_status: Arc<crate::cron::CronStatus>,
    /// The plugin sandbox, when `plugins.enabled`. `None` makes every stored
    /// plugin a load issue on this node rather than a running function.
    pub plugins: Option<Arc<crate::plugin::WasmRuntime>>,
    /// Per-client failed-admin-auth backoff. Node-local and ephemeral by
    /// design: it exists to blunt online guessing, not to be a shared ledger.
    pub admin_auth_failures: Arc<crate::auth::FailedAuthTracker>,
    /// The same, for channel credentials — a separate budget on purpose.
    ///
    /// One shared tracker would let guessing at a public data-plane channel
    /// lock the same address out of the admin plane, and the reverse. They are
    /// different credentials answering to different operators, so they get
    /// different budgets; keys are `channel\u{1f}client`, which also keeps one
    /// misconfigured integration behind a NAT from locking that address out of
    /// every *other* channel.
    pub channel_auth_failures: Arc<crate::auth::FailedAuthTracker>,
    /// `rate_limit.trusted_proxies`, parsed once at startup.
    ///
    /// Held here rather than read off `rate_limit_state` because client
    /// identification is not only the rate limiter's concern: the audit trail
    /// (O7) and the failed-auth backoff (S12) resolve the caller's address
    /// with the same policy. Sourcing it from the limiter tied it to
    /// `rate_limit.enabled`, which is `false` by default — so on any
    /// deployment behind an ingress or load balancer, every audit row
    /// recorded the proxy's address and there was no way to change that short
    /// of turning on rate limiting.
    ///
    /// The per-channel `rate_limit` keys on it too: the ingress guards enforce
    /// that limit with `[rate_limit] enabled = false`, so hanging the trust
    /// list off the limiter meant that in exactly the configuration the
    /// per-channel limit exists for, every client behind a proxy collapsed
    /// into one bucket (S15).
    pub trusted_proxies: Arc<Vec<ipnet::IpNet>>,
    /// The node's composition root (`V2-11.1`): registered native Rust
    /// `ChannelDriver`s, `ProviderBackend`s (`rust`, `workflow`, `wasm`),
    /// `AdmissionPolicy`, and `ResourcePoolRegistry`.
    pub composition: crate::extensions::CompositionRoot,
}

impl AppStateInner {
    /// Reverse proxies whose `X-Forwarded-For` / `X-Real-IP` are honoured
    /// when identifying the client. Empty by default, in which case the
    /// direct peer is the client — see [`Self::trusted_proxies`].
    pub fn trusted_proxies(&self) -> &[ipnet::IpNet] {
        &self.trusted_proxies
    }

    /// `(size, idle)` connection counts for the `/health` gauges (R26).
    pub fn pool_stats(&self) -> (u32, usize) {
        (self.db_pool.size(), self.db_pool.num_idle())
    }

    /// Database connectivity check for the health probes (D22 — this was
    /// `WorkflowRepository::ping`, which only existed because the probes
    /// needed a pool).
    /// Ping the state database, bounded by `engine.health_check_timeout_secs`
    /// so a hung database fails a probe instead of hanging it. In cluster
    /// mode this also answers for the shared dedup store, response cache and
    /// rate limits, which live in the same database.
    pub async fn ping_db(&self) -> Result<(), sqlx::Error> {
        let timeout = std::time::Duration::from_secs(self.config.engine.health_check_timeout_secs);
        tokio::time::timeout(timeout, self.db_pool.ping())
            .await
            .unwrap_or(Err(sqlx::Error::PoolTimedOut))
    }

    /// Copy the database to `path` via SQLite's `VACUUM INTO` (R26).
    ///
    /// `Ok(false)` when the backend is not SQLite — the operation has no
    /// equivalent on PostgreSQL or MySQL, which rely on operator snapshot and
    /// PITR tooling. The backup route used to `match` on the pool variant
    /// itself, which is the only reason a concrete `sqlx` pool was reachable
    /// from a handler.
    pub async fn backup_sqlite_into(&self, path: &str) -> Result<bool, sqlx::Error> {
        let DbPool::Sqlite(pool) = &self.db_pool else {
            return Ok(false);
        };
        // `VACUUM INTO` takes a literal, not a bind parameter. The path is
        // operator-configured (`backup.directory`) plus a generated timestamp,
        // never caller-supplied; the escape is belt-and-braces.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "VACUUM INTO '{}'",
            path.replace('\'', "''")
        )))
        .execute(pool)
        .await?;
        Ok(true)
    }
}

/// Shared application state accessible from all route handlers.
///
/// Cloning is O(1) — one atomic refcount bump on the `Arc`. Field access goes
/// through `Arc<T>`'s built-in `Deref` so call sites (`state.runtime`,
/// `state.config`, …) work directly against the inner struct.
pub type AppState = Arc<AppStateInner>;
