//! An in-process cluster store for the tests, standing in for an edition's.
//!
//! Community ships no cluster store (`cluster.enabled = true` needs one an
//! edition composes), so a test of cluster mode composes this one. It is
//! shared by every node built in one process, which is what the multi-node
//! harness (`tests/cluster`) needs: one `Arc` registered on each node is one
//! store for the fleet. Its clock is this process's, the only clock its
//! nodes have. It keeps the store contract
//! (`orion::testing::shared_store_contract`); each operation is atomic under
//! one lock, so concurrent claimants have exactly one winner.
//!
//! It counts what the nodes ask of it, and can be put `down`, for the tests
//! that judge what reaches the store.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use orion::extension_api::{SharedStateStore, StoreError, async_trait};

type Entries = HashMap<String, (String, Option<Instant>)>;

pub struct CountingStore {
    name: &'static str,
    entries: Mutex<Entries>,
    windows: Mutex<HashMap<(String, u64), i64>>,
    pub claim_calls: AtomicUsize,
    pub claims_won: AtomicUsize,
    pub sets: AtomicUsize,
    pub get_manys: AtomicUsize,
    pub window_hits: AtomicUsize,
    /// When set, every call (the ping included) fails as unavailable.
    pub down: AtomicBool,
}

impl Default for CountingStore {
    /// A store named `fake`.
    fn default() -> Self {
        Self::named("fake")
    }
}

impl CountingStore {
    pub fn named(name: &'static str) -> Self {
        Self {
            name,
            entries: Mutex::default(),
            windows: Mutex::default(),
            claim_calls: AtomicUsize::new(0),
            claims_won: AtomicUsize::new(0),
            sets: AtomicUsize::new(0),
            get_manys: AtomicUsize::new(0),
            window_hits: AtomicUsize::new(0),
            down: AtomicBool::new(false),
        }
    }

    fn check(&self) -> Result<(), StoreError> {
        if self.down.load(Ordering::SeqCst) {
            Err(StoreError::unavailable("the in-memory store is down"))
        } else {
            Ok(())
        }
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, Entries> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The live value under `key`, if any.
fn live(entries: &Entries, key: &str) -> Option<String> {
    entries.get(key).and_then(|(value, expires)| {
        expires
            .is_none_or(|at| at > Instant::now())
            .then(|| value.clone())
    })
}

fn expiry(ttl_secs: Option<u64>) -> Option<Instant> {
    ttl_secs.map(|secs| Instant::now() + Duration::from_secs(secs))
}

#[async_trait]
impl SharedStateStore for CountingStore {
    fn name(&self) -> &str {
        self.name
    }

    async fn get(&self, key: &str) -> Result<Option<String>, StoreError> {
        self.check()?;
        Ok(live(&self.entries(), key))
    }

    async fn get_many(&self, keys: &[String]) -> Result<Vec<Option<String>>, StoreError> {
        self.check()?;
        self.get_manys.fetch_add(1, Ordering::SeqCst);
        let entries = self.entries();
        Ok(keys.iter().map(|k| live(&entries, k)).collect())
    }

    async fn set(&self, key: &str, value: &str, ttl_secs: Option<u64>) -> Result<(), StoreError> {
        self.check()?;
        self.sets.fetch_add(1, Ordering::SeqCst);
        self.entries()
            .insert(key.to_string(), (value.to_string(), expiry(ttl_secs)));
        Ok(())
    }

    async fn remove_many(&self, keys: &[String]) -> Result<u64, StoreError> {
        self.check()?;
        let mut entries = self.entries();
        let live_count = keys.iter().filter(|k| live(&entries, k).is_some()).count();
        for key in keys {
            entries.remove(key);
        }
        Ok(live_count as u64)
    }

    async fn claim(
        &self,
        key: &str,
        owner: &str,
        window_secs: u64,
    ) -> Result<Option<String>, StoreError> {
        self.check()?;
        self.claim_calls.fetch_add(1, Ordering::SeqCst);
        let mut entries = self.entries();
        if let Some(holder) = live(&entries, key) {
            return Ok(Some(holder));
        }
        self.claims_won.fetch_add(1, Ordering::SeqCst);
        entries.insert(
            key.to_string(),
            (owner.to_string(), expiry(Some(window_secs))),
        );
        Ok(None)
    }

    async fn incr_by(&self, key: &str, by: i64, ttl_secs: Option<u64>) -> Result<i64, StoreError> {
        self.check()?;
        let mut entries = self.entries();
        let (next, expires) = match entries.get(key) {
            Some((value, expires)) if expires.is_none_or(|at| at > Instant::now()) => {
                let next = value
                    .parse::<i64>()
                    .map_err(|_| StoreError::invalid(format!("'{key}' is not an integer")))?
                    .checked_add(by)
                    .ok_or_else(|| StoreError::invalid(format!("'{key}' overflows")))?;
                (next, *expires)
            }
            _ => (by, expiry(ttl_secs)),
        };
        entries.insert(key.to_string(), (next.to_string(), expires));
        Ok(next)
    }

    async fn hit_window(&self, key: &str) -> Result<i64, StoreError> {
        self.check()?;
        self.window_hits.fetch_add(1, Ordering::SeqCst);
        let second = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut windows = self
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let hits = windows.entry((key.to_string(), second)).or_insert(0);
        *hits += 1;
        Ok(*hits)
    }

    async fn ping(&self) -> Result<(), StoreError> {
        self.check()
    }
}
