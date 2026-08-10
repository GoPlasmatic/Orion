use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU32, Ordering};

use serde::{Deserialize, Serialize};

const STATE_CLOSED: u8 = 0;
const STATE_OPEN: u8 = 1;
const STATE_HALF_OPEN: u8 = 2;

/// Concurrent probes admitted per half-open window.
const PROBE_PERMITS: u32 = 1;

/// Configuration for circuit breakers. Disabled by default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CircuitBreakerConfig {
    pub enabled: bool,
    pub failure_threshold: u32,
    pub recovery_timeout_secs: u64,
    /// Maximum number of tracked circuit breakers. Oldest entries are evicted when exceeded.
    pub max_breakers: usize,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            failure_threshold: 5,
            recovery_timeout_secs: 30,
            max_breakers: 10_000,
        }
    }
}

/// Lock-free circuit breaker using atomics.
///
/// State machine:
/// - Closed  -> Open     (failures >= threshold)
/// - Open    -> HalfOpen (cooldown elapsed)
/// - HalfOpen -> Closed  (probe succeeds)
/// - HalfOpen -> Open    (probe fails)
pub struct CircuitBreaker {
    state: AtomicU8,
    failure_count: AtomicU32,
    /// Probe permits available in the half-open state.
    ///
    /// Without this the breaker admitted *every* in-flight request the moment
    /// the cooldown elapsed, so the whole backlog stampeded a dependency that
    /// was still broken — one cycle of protection, then effectively closed
    /// until the first failure re-opened it (proposal F19). One permit is
    /// issued per half-open window; the probe's outcome decides the next state.
    probe_permits: AtomicU32,
    /// Milliseconds since `base` at which the breaker last opened.
    opened_at: AtomicI64,
    /// Monotonic reference captured at construction. Cooldowns are measured
    /// against this instead of the wall clock, so an NTP step can no longer
    /// shorten or extend them. `tokio::time::Instant` == `std::time::Instant`
    /// in production builds (the mock clock is dev-only `test-util`), and
    /// `now()` falls back to std outside a runtime.
    base: tokio::time::Instant,
    config: CircuitBreakerConfig,
}

impl CircuitBreaker {
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            state: AtomicU8::new(STATE_CLOSED),
            failure_count: AtomicU32::new(0),
            probe_permits: AtomicU32::new(0),
            opened_at: AtomicI64::new(0),
            base: tokio::time::Instant::now(),
            config,
        }
    }

    /// Milliseconds elapsed since construction — the breaker's clock.
    fn now_ms(&self) -> i64 {
        self.base.elapsed().as_millis() as i64
    }

    /// Check if requests are allowed. Returns `true` if allowed, `false` if circuit is open.
    pub fn check(&self) -> bool {
        let state = self.state.load(Ordering::Acquire);
        match state {
            STATE_CLOSED => true,
            STATE_OPEN => {
                let opened = self.opened_at.load(Ordering::Acquire);
                let now = self.now_ms();
                // Saturate, then clamp into i64: a huge configured timeout
                // means "never auto-recover". The unclamped `* 1000 as i64`
                // wrapped negative for large values, which made the cooldown
                // elapse instantly — the breaker half-opened the moment it
                // opened, exactly when it was configured most conservatively
                // (and the multiplication panicked in debug builds).
                let cooldown_ms = self
                    .config
                    .recovery_timeout_secs
                    .saturating_mul(1000)
                    .min(i64::MAX as u64) as i64;
                if now - opened >= cooldown_ms {
                    // Only one thread wins the CAS; it also mints the single
                    // probe permit for this half-open window, so the losers
                    // fall through to the permit check below and are refused.
                    if self
                        .state
                        .compare_exchange(
                            STATE_OPEN,
                            STATE_HALF_OPEN,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        self.probe_permits.store(PROBE_PERMITS, Ordering::Release);
                    }
                    self.take_probe_permit()
                } else {
                    false
                }
            }
            STATE_HALF_OPEN => self.take_probe_permit(),
            _ => true,
        }
    }

    /// Claim one half-open probe permit, or refuse. Compare-and-swap rather
    /// than `fetch_sub` so the counter cannot wrap below zero under load.
    fn take_probe_permit(&self) -> bool {
        let mut available = self.probe_permits.load(Ordering::Acquire);
        loop {
            if available == 0 {
                return false;
            }
            match self.probe_permits.compare_exchange_weak(
                available,
                available - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => available = actual,
            }
        }
    }

    /// Record a successful request.
    pub fn record_success(&self) {
        self.failure_count.store(0, Ordering::Release);
        self.probe_permits.store(0, Ordering::Release);
        // HalfOpen -> Closed
        let _ = self.state.compare_exchange(
            STATE_HALF_OPEN,
            STATE_CLOSED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// Record a failed request. Returns `true` if the circuit just tripped open.
    pub fn record_failure(&self) -> bool {
        let state = self.state.load(Ordering::Acquire);
        match state {
            STATE_HALF_OPEN => {
                // Probe failed — back to Open. Drop any unused permit so the
                // next cooldown expiry mints a fresh one rather than letting a
                // second request slip through this window.
                self.probe_permits.store(0, Ordering::Release);
                self.opened_at.store(self.now_ms(), Ordering::Release);
                let _ = self.state.compare_exchange(
                    STATE_HALF_OPEN,
                    STATE_OPEN,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                true
            }
            STATE_CLOSED => {
                let prev = self.failure_count.fetch_add(1, Ordering::AcqRel);
                if prev + 1 >= self.config.failure_threshold {
                    self.opened_at.store(self.now_ms(), Ordering::Release);
                    if self
                        .state
                        .compare_exchange(
                            STATE_CLOSED,
                            STATE_OPEN,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return true;
                    }
                }
                false
            }
            _ => false,
        }
    }

    /// Whether the breaker is currently closed (admitting traffic).
    pub fn is_closed(&self) -> bool {
        self.state.load(Ordering::Acquire) == STATE_CLOSED
    }

    /// Human-readable state name.
    pub fn state_name(&self) -> &str {
        match self.state.load(Ordering::Acquire) {
            STATE_CLOSED => "closed",
            STATE_OPEN => "open",
            STATE_HALF_OPEN => "half_open",
            _ => "unknown",
        }
    }

    /// Force-reset to Closed state.
    pub fn reset(&self) {
        self.failure_count.store(0, Ordering::Release);
        self.probe_permits.store(0, Ordering::Release);
        // Clear the open timestamp too: leaving it stale was harmless while
        // nothing read it after a reset, but it is dead state that any future
        // "time in open" metric would report wrong.
        self.opened_at.store(0, Ordering::Release);
        self.state.store(STATE_CLOSED, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(threshold: u32, recovery_secs: u64) -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            enabled: true,
            failure_threshold: threshold,
            recovery_timeout_secs: recovery_secs,
            ..Default::default()
        }
    }

    #[test]
    fn starts_closed() {
        let cb = CircuitBreaker::new(test_config(3, 30));
        assert_eq!(cb.state_name(), "closed");
        assert!(cb.check());
    }

    #[test]
    fn opens_after_threshold() {
        let cb = CircuitBreaker::new(test_config(3, 30));
        assert!(!cb.record_failure());
        assert!(!cb.record_failure());
        assert!(cb.record_failure()); // 3rd failure trips it
        assert_eq!(cb.state_name(), "open");
        assert!(!cb.check()); // should reject
    }

    /// A huge recovery timeout means "never auto-recover" — it must not
    /// overflow into a negative cooldown that half-opens the breaker the
    /// instant it opens.
    #[test]
    fn huge_recovery_timeout_stays_open() {
        let cb = CircuitBreaker::new(test_config(1, u64::MAX));
        assert!(cb.record_failure());
        assert_eq!(cb.state_name(), "open");
        assert!(!cb.check(), "an unelapsed cooldown must keep rejecting");
        assert_eq!(cb.state_name(), "open");
    }

    #[test]
    fn success_resets_failure_count() {
        let cb = CircuitBreaker::new(test_config(3, 30));
        cb.record_failure();
        cb.record_failure();
        cb.record_success();
        assert!(!cb.record_failure()); // count was reset, so 1 < 3
        assert_eq!(cb.state_name(), "closed");
    }

    #[test]
    fn half_open_on_cooldown() {
        let cb = CircuitBreaker::new(test_config(2, 0)); // 0s recovery
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state_name(), "open");

        // With 0s recovery, check() should transition to HalfOpen
        assert!(cb.check());
        assert_eq!(cb.state_name(), "half_open");
    }

    #[test]
    fn half_open_success_closes() {
        let cb = CircuitBreaker::new(test_config(2, 0));
        cb.record_failure();
        cb.record_failure();
        cb.check(); // -> HalfOpen
        cb.record_success();
        assert_eq!(cb.state_name(), "closed");
    }

    #[test]
    fn half_open_failure_reopens() {
        let cb = CircuitBreaker::new(test_config(2, 0));
        cb.record_failure();
        cb.record_failure();
        cb.check(); // -> HalfOpen
        assert!(cb.record_failure()); // probe fails, back to Open
        assert_eq!(cb.state_name(), "open");
    }

    #[test]
    fn reset_forces_closed() {
        let cb = CircuitBreaker::new(test_config(2, 60));
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state_name(), "open");
        cb.reset();
        assert_eq!(cb.state_name(), "closed");
        assert!(cb.check());
    }

    #[test]
    fn open_rejects_during_cooldown() {
        let cb = CircuitBreaker::new(test_config(2, 3600)); // 1hr recovery
        cb.record_failure();
        cb.record_failure();
        assert!(!cb.check()); // still in cooldown
        assert_eq!(cb.state_name(), "open");
    }

    // -- half-open probe gating (proposal F19) --------------------------

    #[tokio::test(start_paused = true)]
    async fn half_open_admits_exactly_one_probe() {
        // Before F19 the cooldown expiry admitted every in-flight request, so
        // the whole backlog stampeded a dependency that was still broken.
        let cb = CircuitBreaker::new(test_config(2, 30));
        cb.record_failure();
        assert!(cb.record_failure(), "breaker should trip");
        assert!(!cb.check(), "open breaker rejects during cooldown");

        tokio::time::advance(std::time::Duration::from_secs(31)).await;

        assert!(cb.check(), "the first caller after cooldown probes");
        assert_eq!(cb.state_name(), "half_open");
        for _ in 0..50 {
            assert!(
                !cb.check(),
                "only one probe may be in flight while half-open"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_probe_reopens_and_withholds_further_probes() {
        let cb = CircuitBreaker::new(test_config(1, 30));
        assert!(cb.record_failure());
        tokio::time::advance(std::time::Duration::from_secs(31)).await;

        assert!(cb.check(), "probe admitted");
        cb.record_failure();
        assert_eq!(cb.state_name(), "open");
        assert!(!cb.check(), "a failed probe restarts the cooldown");

        // The next window mints a fresh permit.
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        assert!(cb.check(), "a new window admits one probe again");
    }

    #[tokio::test(start_paused = true)]
    async fn a_successful_probe_closes_the_breaker_for_everyone() {
        let cb = CircuitBreaker::new(test_config(1, 30));
        assert!(cb.record_failure());
        tokio::time::advance(std::time::Duration::from_secs(31)).await;

        assert!(cb.check());
        cb.record_success();
        assert_eq!(cb.state_name(), "closed");
        // Closed means unrestricted again — the permit must not leak into it.
        for _ in 0..10 {
            assert!(cb.check(), "a closed breaker admits everything");
        }
    }
}
