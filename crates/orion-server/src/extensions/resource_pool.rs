//! Extensible connection-pool registry and version-fenced session cache (`V2-05.4`, `V2-11.1`).
//!
//! Replaces reliance on a closed [`crate::connector::PoolSlot`] enum for new
//! Direct Rust providers:
//! - [`ConnectionPoolCache`] is an open async eviction trait that any provider's
//!   pool manager can implement and register on [`ResourcePoolRegistry`].
//! - [`VersionedSessionPool`] partitions stateful sessions by
//!   [`EffectiveConnectionIdentity`] (`(provider_digest, connection_id, revision, secret_hash)`),
//!   fences invalidated creators so late completions from a superseded revision
//!   cannot populate the current cache, and shares a stable capacity ceiling
//!   keyed by `(provider_digest, connection_id)` across retained revisions (`CTX-08`).

use std::any::{Any, TypeId};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::contract::{ConnectorError, EffectStatus, EffectiveConnectionIdentity};

/// Boxed async future used by object-safe pool eviction hooks.
pub type BoxPoolFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Open trait for any connection-pool cache that participates in per-connector
/// and cluster-resync blanket eviction (`V2-11.1`).
pub trait ConnectionPoolCache: Send + Sync {
    /// Stable driver/cache identifier for diagnostics (e.g. `"orion.postgresql"`).
    fn cache_id(&self) -> &str;

    /// Evict all cached sessions/pools belonging to `connector_name`.
    fn evict<'a>(&'a self, connector_name: &'a str) -> BoxPoolFuture<'a, ()>;

    /// Evict every cached session/pool in this cache (used on cluster epoch resync).
    fn evict_all<'a>(&'a self) -> BoxPoolFuture<'a, ()>;
}

type TypedPoolMap = BTreeMap<TypeId, Arc<dyn Any + Send + Sync>>;
type TypedNamedPoolMap = BTreeMap<(TypeId, String), Arc<dyn Any + Send + Sync>>;

/// Registry of provider-owned connection-pool caches (`V2-11.1`).
///
/// Allows Community foundation providers and Enterprise providers to register
/// typed pool caches (`TypeId`) and receive per-connector and blanket eviction
/// notifications without adding variants to [`crate::connector::PoolSlot`].
#[derive(Default, Clone)]
pub struct ResourcePoolRegistry {
    caches: Arc<RwLock<Vec<Arc<dyn ConnectionPoolCache>>>>,
    typed: Arc<RwLock<TypedPoolMap>>,
    typed_by_id: Arc<RwLock<TypedNamedPoolMap>>,
}

impl std::fmt::Debug for ResourcePoolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.caches.read().map(|g| g.len()).unwrap_or(0);
        f.debug_struct("ResourcePoolRegistry")
            .field("registered_caches", &count)
            .finish()
    }
}

impl ResourcePoolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a typed pool cache that also participates in lifecycle eviction.
    pub fn register<C>(&self, cache: Arc<C>)
    where
        C: ConnectionPoolCache + 'static,
    {
        let cache_id = cache.cache_id().to_string();
        if let Ok(mut caches) = self.caches.write() {
            caches.push(cache.clone());
        }
        if let Ok(mut typed) = self.typed.write() {
            typed.insert(TypeId::of::<C>(), cache.clone());
        }
        if let Ok(mut typed_by_id) = self.typed_by_id.write() {
            typed_by_id.insert((TypeId::of::<C>(), cache_id), cache);
        }
    }

    /// Retrieve a previously registered typed pool cache.
    pub fn get<C: Send + Sync + 'static>(&self) -> Option<Arc<C>> {
        let guard = self.typed.read().ok()?;
        let any_arc = guard.get(&TypeId::of::<C>())?.clone();
        Arc::downcast::<C>(any_arc).ok()
    }

    /// Retrieve a previously registered typed pool cache by its `cache_id` (`TD-28`).
    pub fn get_by_id<C: Send + Sync + 'static>(&self, cache_id: &str) -> Option<Arc<C>> {
        let guard = self.typed_by_id.read().ok()?;
        let any_arc = guard
            .get(&(TypeId::of::<C>(), cache_id.to_string()))?
            .clone();
        Arc::downcast::<C>(any_arc).ok()
    }

    /// Number of registered pool caches.
    pub fn len(&self) -> usize {
        self.caches.read().map(|g| g.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Evict `connector_name` across all registered provider pool caches.
    pub async fn evict_connector(&self, connector_name: &str) {
        let snapshot: Vec<Arc<dyn ConnectionPoolCache>> = self
            .caches
            .read()
            .map(|g| g.iter().cloned().collect())
            .unwrap_or_default();
        for cache in snapshot {
            cache.evict(connector_name).await;
        }
    }

    /// Evict all entries across all registered provider pool caches.
    pub async fn evict_all(&self) {
        let snapshot: Vec<Arc<dyn ConnectionPoolCache>> = self
            .caches
            .read()
            .map(|g| g.iter().cloned().collect())
            .unwrap_or_default();
        for cache in snapshot {
            cache.evict_all().await;
        }
    }
}

/// A leased session holding a permit from the stable `(provider_digest, connection_id)`
/// capacity ceiling (`V2-05.4`, `CTX-08`).
#[derive(Debug)]
pub struct SessionHandle<S> {
    session: S,
    identity: EffectiveConnectionIdentity,
    _permit: OwnedSemaphorePermit,
}

impl<S> SessionHandle<S> {
    pub fn session(&self) -> &S {
        &self.session
    }

    pub fn identity(&self) -> &EffectiveConnectionIdentity {
        &self.identity
    }
}

struct SessionPoolState<S> {
    /// Cached sessions keyed by full effective identity (`(provider_digest, connection_id, revision, secret_hash)`).
    sessions: BTreeMap<EffectiveConnectionIdentity, S>,
    /// Explicitly invalidated identities or minimum valid revision per `(provider_digest, connection_id)`.
    /// Any creator completing for an identity in `invalidated` or below `min_revision` is fenced (`CTX-08`).
    invalidated: BTreeSet<EffectiveConnectionIdentity>,
    min_revision: BTreeMap<(String, String), u64>,
    /// Shared capacity semaphores keyed by stable `(provider_digest, connection_id)` so
    /// retained revisions and rotated secrets share one capacity ceiling (`CTX-08`).
    capacity: BTreeMap<(String, String), Arc<Semaphore>>,
    /// Monotonic global eviction epoch incremented on `evict_all` so in-flight creators
    /// started before blanket eviction cannot populate the cache (`TD-28`).
    global_eviction_epoch: u64,
    /// Per-connector eviction epochs incremented on `evict(connector_name)` (`TD-28`).
    connector_eviction_epochs: BTreeMap<String, u64>,
}

/// Version- and credential-fenced session pool (`V2-05.4`, `CTX-08`).
///
/// Guarantees:
/// 1. Distinct revisions or rotated `secret_hash` values never share a cached session.
/// 2. Invalidating an identity (or advancing a connection's minimum revision) fences
///    in-flight creators so a slow creator for an old credential/revision cannot
///    insert its stale session into the cache after completion.
/// 3. All active borrows across old and new revisions of the same `(provider_digest, connection_id)`
///    draw from one shared `Semaphore` of size `max_capacity_per_connection`.
pub struct VersionedSessionPool<S> {
    cache_id: String,
    max_capacity_per_connection: usize,
    state: Mutex<SessionPoolState<S>>,
}

impl<S: Clone + Send + Sync + 'static> VersionedSessionPool<S> {
    pub fn new(cache_id: impl Into<String>, max_capacity_per_connection: usize) -> Self {
        Self {
            cache_id: cache_id.into(),
            max_capacity_per_connection: max_capacity_per_connection.max(1),
            state: Mutex::new(SessionPoolState {
                sessions: BTreeMap::new(),
                invalidated: BTreeSet::new(),
                min_revision: BTreeMap::new(),
                capacity: BTreeMap::new(),
                global_eviction_epoch: 0,
                connector_eviction_epochs: BTreeMap::new(),
            }),
        }
    }

    /// Invalidate a specific effective identity and optionally advance the minimum
    /// acceptable revision for its connection (`CTX-08`).
    pub fn invalidate(
        &self,
        identity: &EffectiveConnectionIdentity,
        next_min_revision: Option<u64>,
    ) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.sessions.remove(identity);
        guard.invalidated.insert(identity.clone());
        if let Some(min_rev) = next_min_revision {
            let key = (
                identity.provider_digest.clone(),
                identity.connection_id.clone(),
            );
            let entry = guard.min_revision.entry(key).or_insert(min_rev);
            if min_rev > *entry {
                *entry = min_rev;
            }
            // Drop any cached session for this connection below min_rev.
            guard.sessions.retain(|k, _| {
                !(k.provider_digest == identity.provider_digest
                    && k.connection_id == identity.connection_id
                    && k.revision < min_rev)
            });
        }
    }

    fn is_fenced(state: &SessionPoolState<S>, identity: &EffectiveConnectionIdentity) -> bool {
        if state.invalidated.contains(identity) {
            return true;
        }
        let key = (
            identity.provider_digest.clone(),
            identity.connection_id.clone(),
        );
        state
            .min_revision
            .get(&key)
            .is_some_and(|&min_rev| identity.revision < min_rev)
    }

    /// Acquire a session handle for `identity`, creating the session via `creator`
    /// on cache miss while enforcing the shared per-connection capacity ceiling (`CTX-08`).
    pub async fn acquire<F, Fut>(
        &self,
        identity: &EffectiveConnectionIdentity,
        creator: F,
    ) -> Result<SessionHandle<S>, ConnectorError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<S, ConnectorError>>,
    {
        let (semaphore, cached, start_global_epoch, start_connector_epoch) = {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if Self::is_fenced(&guard, identity) {
                return Err(ConnectorError::capability_invalid(
                    "Connection identity has been invalidated or superseded",
                    EffectStatus::NotStarted,
                ));
            }
            let cap_key = (
                identity.provider_digest.clone(),
                identity.connection_id.clone(),
            );
            let sem = guard
                .capacity
                .entry(cap_key)
                .or_insert_with(|| Arc::new(Semaphore::new(self.max_capacity_per_connection)))
                .clone();
            let existing = guard.sessions.get(identity).cloned();
            let g_epoch = guard.global_eviction_epoch;
            let c_epoch = guard
                .connector_eviction_epochs
                .get(&identity.connection_id)
                .copied()
                .unwrap_or(0);
            (sem, existing, g_epoch, c_epoch)
        };

        // Non-blocking capacity check so a suspended parent holding the only permit
        // cannot deadlock a child (`CTX-07`) and retained revisions cannot exceed
        // the stable connection ceiling (`CTX-08`).
        let permit = semaphore.try_acquire_owned().map_err(|_| {
            ConnectorError::budget_exhausted(
                format!(
                    "Stable connection capacity ({}) exhausted for '{}'",
                    self.max_capacity_per_connection, identity.connection_id
                ),
                EffectStatus::NotStarted,
            )
        })?;

        if let Some(session) = cached {
            return Ok(SessionHandle {
                session,
                identity: identity.clone(),
                _permit: permit,
            });
        }

        // Create outside the lock.
        let created = creator().await?;

        // Re-check fence after async creation (`CTX-08`, `TD-28`): if `identity` was invalidated
        // or evicted while `creator` was running, refuse to insert into the cache.
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if Self::is_fenced(&guard, identity) {
            return Err(ConnectorError::capability_invalid(
                "Connection identity was invalidated during session creation",
                EffectStatus::NotStarted,
            ));
        }
        let current_c_epoch = guard
            .connector_eviction_epochs
            .get(&identity.connection_id)
            .copied()
            .unwrap_or(0);
        if guard.global_eviction_epoch != start_global_epoch
            || current_c_epoch != start_connector_epoch
        {
            return Err(ConnectorError::capability_invalid(
                "Connection session was evicted during creation",
                EffectStatus::NotStarted,
            ));
        }
        let session = guard
            .sessions
            .entry(identity.clone())
            .or_insert(created)
            .clone();

        Ok(SessionHandle {
            session,
            identity: identity.clone(),
            _permit: permit,
        })
    }

    pub fn cached_count(&self) -> usize {
        self.state.lock().map(|g| g.sessions.len()).unwrap_or(0)
    }
}

impl<S: Clone + Send + Sync + 'static> ConnectionPoolCache for VersionedSessionPool<S> {
    fn cache_id(&self) -> &str {
        &self.cache_id
    }

    fn evict<'a>(&'a self, connector_name: &'a str) -> BoxPoolFuture<'a, ()> {
        Box::pin(async move {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let epoch = guard
                .connector_eviction_epochs
                .entry(connector_name.to_string())
                .or_insert(0);
            *epoch = epoch.saturating_add(1);
            guard
                .sessions
                .retain(|k, _| k.connection_id != connector_name);
        })
    }

    fn evict_all<'a>(&'a self) -> BoxPoolFuture<'a, ()> {
        Box::pin(async move {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.global_eviction_epoch = guard.global_eviction_epoch.saturating_add(1);
            guard.sessions.clear();
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    fn sample_id(revision: u64, secret_hash: &str) -> EffectiveConnectionIdentity {
        EffectiveConnectionIdentity {
            provider_digest: "sha256:1111".to_string(),
            connection_id: "orders-db".to_string(),
            revision,
            secret_hash: secret_hash.to_string(),
        }
    }

    #[tokio::test]
    async fn ctx_08_session_isolation_late_creator_fencing_and_shared_capacity() {
        let pool = Arc::new(VersionedSessionPool::<String>::new("orion.postgresql", 2));

        let rev1_sec1 = sample_id(1, "sha256:sec-a");
        let rev1_sec2 = sample_id(1, "sha256:sec-b");
        let rev2_sec2 = sample_id(2, "sha256:sec-b");

        // 1. Distinct secret hashes produce distinct sessions.
        let h1 = pool
            .acquire(&rev1_sec1, || async { Ok("session-rev1-secA".to_string()) })
            .await
            .expect("rev1_sec1 acquired");
        assert_eq!(h1.session(), "session-rev1-secA");

        let h2 = pool
            .acquire(&rev1_sec2, || async { Ok("session-rev1-secB".to_string()) })
            .await
            .expect("rev1_sec2 acquired");
        assert_eq!(h2.session(), "session-rev1-secB");

        // 2. Stable capacity ceiling (2) is shared across both identities of "orders-db"!
        // While h1 and h2 are held, a 3rd acquire on rev2_sec2 must fail without multiplying capacity.
        let third_err = pool
            .acquire(&rev2_sec2, || async { Ok("session-rev2-secB".to_string()) })
            .await
            .expect_err("must enforce shared capacity ceiling of 2 across revisions");
        assert_eq!(
            third_err.code,
            crate::extensions::contract::codes::BUDGET_EXHAUSTED
        );

        drop(h1);
        drop(h2);

        // 3. Late creator from an invalidated identity cannot refill the pool.
        let slow_id = sample_id(1, "sha256:rotating");
        let (start_tx, start_rx) = oneshot::channel::<()>();
        let (finish_tx, finish_rx) = oneshot::channel::<()>();
        let pool_clone = pool.clone();
        let slow_id_clone = slow_id.clone();

        let slow_task = tokio::spawn(async move {
            pool_clone
                .acquire(&slow_id_clone, || async move {
                    let _ = start_tx.send(());
                    let _ = finish_rx.await;
                    Ok("stale-session".to_string())
                })
                .await
        });

        start_rx.await.expect("creator started");
        // Invalidate slow_id and advance min_revision to 2 while creator is in flight.
        pool.invalidate(&slow_id, Some(2));
        let _ = finish_tx.send(());

        let slow_res = slow_task.await.expect("joined");
        assert!(
            slow_res.is_err(),
            "late creator from invalidated identity must be rejected"
        );

        // Fresh rev 2 succeeds and is the only cached entry.
        let h_new = pool
            .acquire(&rev2_sec2, || async { Ok("session-rev2-secB".to_string()) })
            .await
            .expect("rev2 acquires");
        assert_eq!(h_new.session(), "session-rev2-secB");
        assert_eq!(pool.cached_count(), 1);
    }

    #[tokio::test]
    async fn evict_fences_in_flight_creator_and_typed_registry_preserves_distinct_cache_ids() {
        let registry = ResourcePoolRegistry::new();
        let pg_pool = Arc::new(VersionedSessionPool::<String>::new(
            "org.orion.postgresql",
            2,
        ));
        let sqlite_pool = Arc::new(VersionedSessionPool::<String>::new("org.orion.sqlite", 2));
        registry.register(pg_pool.clone());
        registry.register(sqlite_pool.clone());

        // Both pools share TypeId::of::<VersionedSessionPool<String>>() and are retrievable by cache_id!
        assert_eq!(
            registry
                .get_by_id::<VersionedSessionPool<String>>("org.orion.postgresql")
                .expect("pg pool")
                .cache_id(),
            "org.orion.postgresql"
        );
        assert_eq!(
            registry
                .get_by_id::<VersionedSessionPool<String>>("org.orion.sqlite")
                .expect("sqlite pool")
                .cache_id(),
            "org.orion.sqlite"
        );

        // Start a slow creator on pg_pool, call evict_connector("orders-db") while creator is in flight,
        // and verify the stale creator cannot repopulate the cache!
        let id = sample_id(1, "sha256:sec-evict");
        let (start_tx, start_rx) = oneshot::channel::<()>();
        let (finish_tx, finish_rx) = oneshot::channel::<()>();
        let pg_clone = pg_pool.clone();
        let id_clone = id.clone();

        let task = tokio::spawn(async move {
            pg_clone
                .acquire(&id_clone, || async move {
                    let _ = start_tx.send(());
                    let _ = finish_rx.await;
                    Ok("stale-after-evict".to_string())
                })
                .await
        });

        start_rx.await.expect("creator started");
        registry.evict_connector("orders-db").await;
        let _ = finish_tx.send(());

        let res = task.await.expect("joined");
        assert!(res.is_err(), "creator started before evict must be fenced");
        assert_eq!(pg_pool.cached_count(), 0);
    }
}
