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
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::contract::{ConnectorError, EffectStatus, EffectiveConnectionIdentity};
use super::identity::{ConnectionId, Digest, Revision};

/// Open trait for any connection-pool cache that participates in per-connector
/// and cluster-resync blanket eviction (`V2-11.1`). Implement it with
/// `#[async_trait::async_trait]`.
#[async_trait]
pub trait ConnectionPoolCache: Send + Sync {
    /// Stable driver/cache identifier, unique on a node (e.g. `"org.orion.postgresql"`).
    fn cache_id(&self) -> &str;

    /// Evict all cached sessions/pools belonging to `connector_name`.
    async fn evict(&self, connector_name: &str);

    /// Evict every cached session/pool in this cache (used on cluster epoch resync).
    async fn evict_all(&self);
}

/// One pool cache as the composition registers it: erased for eviction, and
/// kept typed so [`ResourcePoolRegistry::get_by_id`] can hand it back.
#[derive(Clone)]
pub struct PoolEntry {
    cache_id: String,
    type_id: TypeId,
    typed: Arc<dyn Any + Send + Sync>,
    cache: Arc<dyn ConnectionPoolCache>,
}

impl PoolEntry {
    pub fn of<C: ConnectionPoolCache + 'static>(cache: Arc<C>) -> Self {
        Self {
            cache_id: cache.cache_id().to_string(),
            type_id: TypeId::of::<C>(),
            typed: cache.clone(),
            cache,
        }
    }

    pub fn cache_id(&self) -> &str {
        &self.cache_id
    }
}

/// Registry of provider-owned connection-pool caches (`V2-11.1`).
///
/// Lets Community foundation providers and Enterprise providers register
/// typed pool caches and receive per-connector and blanket eviction
/// notifications without adding variants to [`crate::connector::PoolSlot`].
/// Built once, whole, by [`super::CompositionBuilder::build`], and frozen:
/// a `cache_id` is unique on a node, so a typed lookup names the one pool it
/// means.
#[derive(Default, Clone)]
pub struct ResourcePoolRegistry {
    entries: Arc<BTreeMap<String, PoolEntry>>,
}

impl std::fmt::Debug for ResourcePoolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourcePoolRegistry")
            .field("caches", &self.entries.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ResourcePoolRegistry {
    /// A registry of `entries`, refusing the first `cache_id` seen twice.
    pub fn new(entries: impl IntoIterator<Item = PoolEntry>) -> Result<Self, String> {
        let mut map = BTreeMap::new();
        for entry in entries {
            if map.contains_key(&entry.cache_id) {
                return Err(entry.cache_id);
            }
            map.insert(entry.cache_id.clone(), entry);
        }
        Ok(Self {
            entries: Arc::new(map),
        })
    }

    /// The pool registered as `cache_id`, when it is a `C` (`TD-28`).
    pub fn get_by_id<C: Send + Sync + 'static>(&self, cache_id: &str) -> Option<Arc<C>> {
        let entry = self.entries.get(cache_id)?;
        if entry.type_id != TypeId::of::<C>() {
            return None;
        }
        Arc::downcast::<C>(entry.typed.clone()).ok()
    }

    /// The registered cache ids, sorted.
    pub fn cache_ids(&self) -> BTreeSet<String> {
        self.entries.keys().cloned().collect()
    }

    /// Number of registered pool caches.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Evict `connector_name` across all registered provider pool caches.
    pub async fn evict_connector(&self, connector_name: &str) {
        for entry in self.entries.values() {
            entry.cache.evict(connector_name).await;
        }
    }

    /// Evict all entries across all registered provider pool caches.
    pub async fn evict_all(&self) {
        for entry in self.entries.values() {
            entry.cache.evict_all().await;
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
    min_revision: BTreeMap<(Digest, ConnectionId), Revision>,
    /// Shared capacity semaphores keyed by stable `(provider_digest, connection_id)` so
    /// retained revisions and rotated secrets share one capacity ceiling (`CTX-08`).
    capacity: BTreeMap<(Digest, ConnectionId), Arc<Semaphore>>,
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
        next_min_revision: Option<Revision>,
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
                .get(identity.connection_id.as_str())
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
            .get(identity.connection_id.as_str())
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

#[async_trait]
impl<S: Clone + Send + Sync + 'static> ConnectionPoolCache for VersionedSessionPool<S> {
    fn cache_id(&self) -> &str {
        &self.cache_id
    }

    async fn evict(&self, connector_name: &str) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let epoch = guard
            .connector_eviction_epochs
            .entry(connector_name.to_string())
            .or_insert(0);
        *epoch = epoch.saturating_add(1);
        guard
            .sessions
            .retain(|k, _| k.connection_id != connector_name);
    }

    async fn evict_all(&self) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.global_eviction_epoch = guard.global_eviction_epoch.saturating_add(1);
        guard.sessions.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    fn sample_id(revision: u64, secret_hash: &str) -> EffectiveConnectionIdentity {
        EffectiveConnectionIdentity {
            provider_digest: crate::crypto::sha256_digest(b"orders-provider"),
            connection_id: "orders-db".into(),
            revision: Revision::new(revision).expect("a revision"),
            secret_hash: secret_hash.to_string(),
        }
    }

    #[tokio::test]
    async fn ctx_08_session_isolation_late_creator_fencing_and_shared_capacity() {
        let pool = Arc::new(VersionedSessionPool::<String>::new(
            "org.orion.postgresql",
            2,
        ));

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
        pool.invalidate(&slow_id, Revision::new(2));
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
        let pg_pool = Arc::new(VersionedSessionPool::<String>::new(
            "org.orion.postgresql",
            2,
        ));
        let sqlite_pool = Arc::new(VersionedSessionPool::<String>::new("org.orion.sqlite", 2));
        let registry = ResourcePoolRegistry::new([
            PoolEntry::of(pg_pool.clone()),
            PoolEntry::of(sqlite_pool.clone()),
        ])
        .expect("distinct cache ids");
        // A second pool under a cache id already held is refused.
        assert_eq!(
            ResourcePoolRegistry::new([
                PoolEntry::of(pg_pool.clone()),
                PoolEntry::of(Arc::new(VersionedSessionPool::<u8>::new(
                    "org.orion.postgresql",
                    1
                ))),
            ])
            .err()
            .as_deref(),
            Some("org.orion.postgresql")
        );
        // The typed lookup checks the type as well as the id.
        assert!(
            registry
                .get_by_id::<VersionedSessionPool<u8>>("org.orion.postgresql")
                .is_none()
        );

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
