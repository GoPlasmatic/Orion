use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU32, Ordering};

use serde::{Deserialize, Serialize};

const STATE_CLOSED: u8 = 0;
const STATE_OPEN: u8 = 1;
const STATE_HALF_OPEN: u8 = 2;

/// Configuration for circuit breakers. Disabled by default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CircuitBreakerConfig {
    pub enabled: bool,
    pub failure_threshold: u32,
    pub recovery_timeout_secs: u64,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            failure_threshold: 5,
            recovery_timeout_secs: 30,
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
    opened_at: AtomicI64,
    config: CircuitBreakerConfig,
}

impl CircuitBreaker {
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            state: AtomicU8::new(STATE_CLOSED),
            failure_count: AtomicU32::new(0),
            opened_at: AtomicI64::new(0),
            config,
        }
    }

    /// Check if requests are allowed. Returns `true` if allowed, `false` if circuit is open.
    pub fn check(&self) -> bool {
        let state = self.state.load(Ordering::Acquire);
        match state {
            STATE_CLOSED => true,
            STATE_OPEN => {
                let opened = self.opened_at.load(Ordering::Acquire);
                let now = chrono::Utc::now().timestamp_millis();
                let cooldown_ms = (self.config.recovery_timeout_secs * 1000) as i64;
                if now - opened >= cooldown_ms {
                    // Try to transition to HalfOpen — only one thread wins the CAS
                    let _ = self.state.compare_exchange(
                        STATE_OPEN,
                        STATE_HALF_OPEN,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    true
                } else {
                    false
                }
            }
            STATE_HALF_OPEN => true,
            _ => true,
        }
    }

    /// Record a successful request.
    pub fn record_success(&self) {
        self.failure_count.store(0, Ordering::Release);
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
                // Probe failed — back to Open
                self.opened_at
                    .store(chrono::Utc::now().timestamp_millis(), Ordering::Release);
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
                    self.opened_at
                        .store(chrono::Utc::now().timestamp_millis(), Ordering::Release);
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
}
