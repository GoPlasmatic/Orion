//! Failed-credential backoff, shared by every surface that checks one.
//!
//! Three surfaces check a presented credential: the admin API key, a channel's
//! `auth.mode = "api_key"` / `"hmac"`, and a trace's capability token. Only the
//! first had a failed-attempt budget. The channel path — the *public* one, on
//! the data plane, reachable by anyone who knows a channel name — had none, so
//! `auth.keys` faced unlimited online guessing at whatever rate the channel's
//! own rate limit allowed (and that limit is off by default).
//!
//! This lives above nothing and below everything that authenticates: `channel`
//! is below `server`, so the tracker could not stay in `server::admin_auth`
//! once the channel guard needed it. `module_layering_test` is what said so.

use std::time::Duration;

use dashmap::DashMap;
use tokio::time::Instant;

/// Consecutive failures tolerated from one client before a lockout starts.
/// A human fat-fingering a key gets a few tries; a script does not.
const FAILURES_BEFORE_LOCKOUT: u32 = 5;
/// First lockout, doubling per subsequent failure.
const LOCKOUT_BASE: Duration = Duration::from_millis(500);
/// Ceiling on the doubling, so a sustained attack cannot lock a shared NAT
/// egress address out for an unbounded time.
const LOCKOUT_MAX: Duration = Duration::from_secs(30);
/// Idle period after which a client's failure record is forgotten.
const FAILURE_TTL: Duration = Duration::from_secs(300);
/// Map size that triggers a stale sweep before the next insert.
const EVICT_THRESHOLD: usize = 10_000;

#[derive(Debug, Clone, Copy)]
struct FailureRecord {
    consecutive: u32,
    /// Monotonic — a wall-clock step must not shorten or extend a lockout.
    locked_until: Option<Instant>,
    last_seen: Instant,
}

/// Per-client failed-admin-auth tracker with exponential backoff.
///
/// Without this, `admin_auth.api_keys` faced unlimited guessing attempts: the
/// middleware returns 401 without calling `next.run`, so before the S16 layer
/// reorder the rate limiter never even saw the request — and the limiter is
/// off by default regardless (proposal S12).
#[derive(Debug, Default)]
pub struct FailedAuthTracker {
    clients: DashMap<String, FailureRecord>,
}

impl FailedAuthTracker {
    /// Remaining lockout for `client`, if any.
    pub fn locked_for(&self, client: &str) -> Option<Duration> {
        let rec = self.clients.get(client)?;
        let until = rec.locked_until?;
        until.checked_duration_since(Instant::now())
    }

    /// Record a failed attempt and return the lockout it triggered, if any.
    pub fn record_failure(&self, client: &str) -> Option<Duration> {
        let now = Instant::now();
        // An attacker cycling source addresses would otherwise grow the map
        // without bound; sweeping on the way in keeps it proportional to the
        // number of *recently* failing clients.
        if self.clients.len() >= EVICT_THRESHOLD {
            self.evict_stale();
        }
        let mut entry = self
            .clients
            .entry(client.to_string())
            .or_insert(FailureRecord {
                consecutive: 0,
                locked_until: None,
                last_seen: now,
            });
        // A long-idle record is a fresh start, not a continuation.
        if now.duration_since(entry.last_seen) > FAILURE_TTL {
            entry.consecutive = 0;
            entry.locked_until = None;
        }
        entry.consecutive = entry.consecutive.saturating_add(1);
        entry.last_seen = now;

        if entry.consecutive < FAILURES_BEFORE_LOCKOUT {
            return None;
        }
        let steps = entry.consecutive - FAILURES_BEFORE_LOCKOUT;
        let backoff = LOCKOUT_BASE
            .checked_mul(1u32.checked_shl(steps.min(16)).unwrap_or(u32::MAX))
            .unwrap_or(LOCKOUT_MAX)
            .min(LOCKOUT_MAX);
        entry.locked_until = Some(now + backoff);
        Some(backoff)
    }

    /// Clear a client's history after a successful authentication.
    pub fn record_success(&self, client: &str) {
        self.clients.remove(client);
    }

    /// Drop records that have been idle past their TTL. Called opportunistically
    /// so an attacker cycling source addresses cannot grow the map without bound.
    fn evict_stale(&self) {
        let now = Instant::now();
        self.clients
            .retain(|_, rec| now.duration_since(rec.last_seen) <= FAILURE_TTL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn backoff_starts_only_after_a_grace_period() {
        let t = FailedAuthTracker::default();
        for _ in 1..FAILURES_BEFORE_LOCKOUT {
            assert!(
                t.record_failure("1.2.3.4").is_none(),
                "a few typos must not lock anyone out"
            );
            assert!(t.locked_for("1.2.3.4").is_none());
        }
        let first = t.record_failure("1.2.3.4").expect("lockout starts");
        assert_eq!(first, LOCKOUT_BASE);
        assert!(t.locked_for("1.2.3.4").is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_doubles_and_is_capped() {
        let t = FailedAuthTracker::default();
        let mut last = Duration::ZERO;
        for _ in 0..40 {
            if let Some(d) = t.record_failure("1.2.3.4") {
                assert!(d >= last, "backoff must not shrink");
                last = d;
            }
        }
        assert_eq!(last, LOCKOUT_MAX, "backoff must saturate, not overflow");
    }

    #[tokio::test(start_paused = true)]
    async fn lockout_expires_on_the_monotonic_clock() {
        let t = FailedAuthTracker::default();
        for _ in 0..FAILURES_BEFORE_LOCKOUT {
            t.record_failure("1.2.3.4");
        }
        assert!(t.locked_for("1.2.3.4").is_some());
        tokio::time::advance(LOCKOUT_BASE + Duration::from_millis(1)).await;
        assert!(
            t.locked_for("1.2.3.4").is_none(),
            "the lockout must lift once it elapses"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn success_clears_the_record_and_clients_are_independent() {
        let t = FailedAuthTracker::default();
        for _ in 0..FAILURES_BEFORE_LOCKOUT {
            t.record_failure("1.2.3.4");
        }
        assert!(t.locked_for("1.2.3.4").is_some());
        // A different client is unaffected by the first one's lockout.
        assert!(t.locked_for("5.6.7.8").is_none());

        t.record_success("1.2.3.4");
        assert!(t.locked_for("1.2.3.4").is_none());
        assert!(
            t.record_failure("1.2.3.4").is_none(),
            "the counter must restart after a success"
        );
    }
}
