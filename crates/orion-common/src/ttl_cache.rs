//! A TTL cache with per-key single-flight and serve-stale on refresh failure.
//!
//! Both the JWKS key sets and the OIDC discovery documents are public metadata
//! fetched from an issuer. The policy is the same and was written twice: a TTL
//! bounds staleness, a herd on expiry must cost one fetch, and a failed
//! refresh serves the last good copy rather than refusing every caller. This
//! is the one place that policy lives (M-14).

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex as AsyncMutex, RwLock};

/// One cached value and when it was fetched.
#[derive(Debug)]
pub struct Cached<V> {
    pub value: Arc<V>,
    pub fetched_at: Instant,
    pub ttl: Duration,
    /// When a forced refresh last failed for this key, if one did. A forced
    /// refetch is rate-limited by the caller against this.
    pub last_forced: Option<Instant>,
}

impl<V> Cached<V> {
    /// Whether this entry is still within its TTL.
    pub fn is_fresh(&self) -> bool {
        self.fetched_at.elapsed() < self.ttl
    }
}

/// A map of cached values with a per-key single-flight lock.
///
/// The flight map holds `Weak` pointers, so an entry lives exactly as long as
/// someone is fetching that key: the last holder to drop its `Arc` leaves a
/// dangling weak that the next acquire prunes. That bounds the map by
/// *concurrent fetches* rather than by keys ever seen.
pub struct TtlCache<K, V> {
    entries: RwLock<HashMap<K, Arc<Cached<V>>>>,
    flights: Mutex<HashMap<K, Weak<AsyncMutex<()>>>>,
}

impl<K, V> Default for TtlCache<K, V> {
    fn default() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            flights: Mutex::new(HashMap::new()),
        }
    }
}

impl<K, V> TtlCache<K, V> {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<K, V> TtlCache<K, V>
where
    K: Eq + Hash + Clone,
{
    /// The cached value for `key`, fresh or stale.
    pub async fn get<Q>(&self, key: &Q) -> Option<Arc<Cached<V>>>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.entries.read().await.get(key).cloned()
    }

    /// The cached value for `key` when it is still within its TTL.
    pub async fn fresh<Q>(&self, key: &Q) -> Option<Arc<Cached<V>>>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.get(key).await.filter(|entry| entry.is_fresh())
    }

    /// The single-flight lock for `key`, shared with anyone fetching it now.
    ///
    /// A `std::sync::Mutex` for the map: it guards plain data, is never held
    /// across an `.await`, and a panic mid-update cannot leave weak pointers
    /// inconsistent.
    pub fn flight<Q>(&self, key: &Q) -> Arc<AsyncMutex<()>>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ToOwned<Owned = K> + ?Sized,
    {
        let mut flights = self
            .flights
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Drop keys nobody is fetching any more. O(live fetches).
        flights.retain(|_, weak| weak.strong_count() > 0);
        if let Some(existing) = flights.get(key).and_then(Weak::upgrade) {
            return existing;
        }
        let fresh = Arc::new(AsyncMutex::new(()));
        flights.insert(key.to_owned(), Arc::downgrade(&fresh));
        fresh
    }

    /// Store `value` for `key`, returning the entry as stored.
    pub async fn store(
        &self,
        key: K,
        value: Arc<V>,
        ttl: Duration,
        last_forced: Option<Instant>,
    ) -> Arc<Cached<V>> {
        let entry = Arc::new(Cached {
            value,
            fetched_at: Instant::now(),
            ttl,
            last_forced,
        });
        self.entries.write().await.insert(key, Arc::clone(&entry));
        entry
    }

    /// Record that a forced refresh of a still-cached value failed at `at`,
    /// leaving the value and its fetch time untouched.
    ///
    /// This is what stops a flapping issuer being hammered by every caller
    /// that names a key the cached set does not carry.
    pub async fn mark_forced_failure<Q>(&self, key: &Q, at: Instant) -> Option<Arc<Cached<V>>>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ToOwned<Owned = K> + ?Sized,
    {
        let mut entries = self.entries.write().await;
        let old = entries.get(key)?;
        let entry = Arc::new(Cached {
            value: Arc::clone(&old.value),
            fetched_at: old.fetched_at,
            ttl: old.ttl,
            last_forced: Some(at),
        });
        entries.insert(key.to_owned(), Arc::clone(&entry));
        Some(entry)
    }

    /// The keys with a live flight, for a test that pins the map's bound.
    pub fn live_flights(&self) -> Vec<K> {
        self.flights
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_fresh_entry_is_served_and_an_expired_one_is_not() {
        let cache: TtlCache<String, u32> = TtlCache::new();
        cache
            .store(
                "key".to_string(),
                Arc::new(7),
                Duration::from_secs(60),
                None,
            )
            .await;
        assert_eq!(*cache.fresh("key").await.expect("fresh").value, 7);
        assert!(cache.get("key").await.is_some());
    }

    #[tokio::test]
    async fn one_flight_per_key_and_finished_flights_are_pruned() {
        let cache: TtlCache<String, u32> = TtlCache::new();
        let first = cache.flight("a");
        let second = cache.flight("a");
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(cache.live_flights(), ["a".to_string()]);
        drop(first);
        drop(second);
        // The next acquire prunes the weak that nobody holds.
        let _other = cache.flight("b");
        assert_eq!(cache.live_flights(), ["b".to_string()]);
    }

    #[tokio::test]
    async fn a_forced_failure_keeps_the_value_and_stamps_the_time() {
        let cache: TtlCache<String, u32> = TtlCache::new();
        cache
            .store(
                "key".to_string(),
                Arc::new(7),
                Duration::from_secs(60),
                None,
            )
            .await;
        let stamped = cache
            .mark_forced_failure("key", Instant::now())
            .await
            .expect("stored");
        assert_eq!(*stamped.value, 7);
        assert!(stamped.last_forced.is_some());
        assert_eq!(*cache.get("key").await.expect("still cached").value, 7);
    }
}
