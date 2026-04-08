use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;

/// In-memory idempotency store backed by `DashMap` with TTL-based expiry.
///
/// Keys are idempotency tokens extracted from a request header. Duplicate
/// submissions within the configured `window` are rejected with 409 Conflict.
pub struct DeduplicationStore {
    entries: DashMap<String, Instant>,
    window: Duration,
}

impl DeduplicationStore {
    /// Create a new store with a background cleanup task.
    pub fn new(window_secs: u64) -> Arc<Self> {
        let window = Duration::from_secs(window_secs);
        let store = Arc::new(Self {
            entries: DashMap::new(),
            window,
        });

        // Spawn a background task that purges expired entries.
        let weak = Arc::downgrade(&store);
        tokio::spawn(async move {
            let interval = Duration::from_secs(window_secs.max(1) / 2 + 1);
            loop {
                tokio::time::sleep(interval).await;
                let Some(store) = weak.upgrade() else {
                    break; // Store has been dropped — stop the cleanup task
                };
                store.purge_expired();
            }
        });

        store
    }

    /// Check whether `key` is new. Returns `true` if this is the first time
    /// (not a duplicate), `false` if a duplicate within the window.
    pub fn check_and_insert(&self, key: &str) -> bool {
        let now = Instant::now();
        if let Some(entry) = self.entries.get(key)
            && now.duration_since(*entry.value()) < self.window
        {
            return false; // duplicate
        }
        self.entries.insert(key.to_string(), now);
        true
    }

    fn purge_expired(&self) {
        let cutoff = Instant::now() - self.window;
        self.entries.retain(|_, ts| *ts > cutoff);
    }
}
