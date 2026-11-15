//! Per-channel rate-limit backends (multi-instance-ha A4).
//!
//! Single node: governor's in-process GCRA (`local`) — precise and free.
//! Cluster: a fixed window in the cluster's shared store (the state
//! database, or an edition's), so a configured limit holds across all
//! replicas combined instead of multiplying by the node count. The window is
//! derived from **the store's own clock**, so every replica ages into the
//! same bucket no matter how its local clock drifts. Fixed windows trade up to 2x burst at a window boundary for
//! simplicity — the configured burst allowance absorbs it in practice.

use std::sync::Arc;

use async_trait::async_trait;
use orion_extension_api::SharedStateStore;

use super::{KeyedLimiter, build_keyed_limiter};
use crate::errors::OrionError;

#[async_trait]
pub trait RateLimitBackend: Send + Sync {
    /// `Ok(true)` = allow, `Ok(false)` = reject with 429. `Err` = the backing
    /// store could not answer; the **caller** resolves it through the
    /// channel's `rate_limit.on_backend_error` policy (N7) — `allow` fails
    /// open, `deny` refuses with 503. Fail-open is the per-channel default,
    /// not a contract of this trait. Takes the key by value:
    /// callers already own one, and governor's keyed store wants `&String`.
    async fn check(&self, key: String) -> Result<bool, OrionError>;

    /// The verdict in place, without a future, for a backend that can give
    /// one — the in-process governor, which is a few atomic operations.
    /// `None` means this backend must be asked through [`Self::check`].
    ///
    /// [`Self::check`] is an `async_trait` method, so every call boxes a
    /// future; the per-channel limiters are consulted on every request, and
    /// the local one never needs to await. Callers try this first.
    ///
    /// Takes `&String` because governor's keyed store is keyed by `String`
    /// and looks up by `&K`.
    fn check_now(&self, _key: &String) -> Option<bool> {
        None
    }

    /// Drop keys whose limiter state has fully replenished and release the
    /// slack their storage held. Governor's in-process keyed store only shrinks
    /// when asked, so a channel keyed by client identity would otherwise grow
    /// one entry per address (or header value) ever seen and never shrink. The
    /// supervised limiter-prune task calls this periodically; a store that ages
    /// its own keys out (the cluster's shared store) leaves it a no-op.
    fn prune(&self) {}
}

/// In-process governor limiter (today's behaviour; N replicas = N× the limit).
pub struct LocalRateLimitBackend {
    limiter: KeyedLimiter,
}

impl LocalRateLimitBackend {
    pub fn new(rps: u32, burst: u32) -> Self {
        Self {
            limiter: build_keyed_limiter(rps, burst),
        }
    }
}

#[async_trait]
impl RateLimitBackend for LocalRateLimitBackend {
    async fn check(&self, key: String) -> Result<bool, OrionError> {
        Ok(self.limiter.check_key(&key).is_ok())
    }

    fn check_now(&self, key: &String) -> Option<bool> {
        Some(self.limiter.check_key(key).is_ok())
    }

    fn prune(&self) {
        // `retain_recent` drops keys back at full capacity; `shrink_to_fit`
        // then returns the map's spare capacity to the allocator.
        self.limiter.retain_recent();
        self.limiter.shrink_to_fit();
    }
}

/// Shared fixed-window limiter on the cluster's shared store.
///
/// One count per `(scope:key, second)` ([`SharedStateStore::hit_window`]),
/// so the count is the fleet's. Window limit = rps + burst, mapping the
/// per-channel `requests_per_second`/`burst` config onto a one-second
/// window. The second is the **store's** clock, not the node's: skewed
/// nodes would otherwise land in different windows and the shared limit
/// The burst a limiter gets when its config leaves it unset: half a second's
/// worth of requests, plus one so a rate of 1/s still admits a pair.
///
/// One function for the per-channel burst and the platform route-group
/// limiter, which had the expression copied (M-14).
pub fn default_burst(requests_per_second: u32) -> u32 {
    requests_per_second / 2 + 1
}

/// would silently multiply by the number of skew groups.
pub struct SharedRateLimitBackend {
    store: Arc<dyn SharedStateStore>,
    scope: String,
    limit_per_window: u32,
}

impl SharedRateLimitBackend {
    pub fn new(store: Arc<dyn SharedStateStore>, scope: String, rps: u32, burst: u32) -> Self {
        Self {
            store,
            scope,
            limit_per_window: rps.saturating_add(burst).max(1),
        }
    }
}

#[async_trait]
impl RateLimitBackend for SharedRateLimitBackend {
    async fn check(&self, key: String) -> Result<bool, OrionError> {
        match self
            .store
            .hit_window(&format!("{}:{key}", self.scope))
            .await
        {
            Ok(count) => Ok(count <= i64::from(self.limit_per_window)),
            // N7: not resolved here — the per-channel `on_backend_error`
            // policy lives with the caller, which also logs and records the
            // error metric with the channel in scope.
            Err(e) => Err(OrionError::internal(format!(
                "rate-limit backend '{}': cluster store '{}': {}",
                self.scope,
                self.store.name(),
                e.message()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_local_backend_enforces_burst() {
        let backend = LocalRateLimitBackend::new(1, 2);
        // Burst capacity of 2 → first two pass, third rejected.
        assert!(backend.check("ip-1".to_string()).await.expect("test"));
        assert!(backend.check("ip-1".to_string()).await.expect("test"));
        assert!(!backend.check("ip-1".to_string()).await.expect("test"));
        // Independent key unaffected.
        assert!(backend.check("ip-2".to_string()).await.expect("test"));
    }

    #[tokio::test]
    async fn test_local_backend_prune_is_safe_and_preserves_limiting() {
        let backend = LocalRateLimitBackend::new(1, 2);
        // Touch several distinct keys, then prune. A key at full capacity is
        // dropped; the sweep must not panic or reset a key that is still
        // limited.
        for k in 0..100 {
            let _ = backend.check(format!("ip-{k}")).await.expect("test");
        }
        backend.prune();
        // A pruned-then-reused key still gets its full burst.
        assert!(backend.check("ip-fresh".to_string()).await.expect("test"));
        assert!(backend.check("ip-fresh".to_string()).await.expect("test"));
        assert!(!backend.check("ip-fresh".to_string()).await.expect("test"));
    }
}
