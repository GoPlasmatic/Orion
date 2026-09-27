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
/// Hard ceiling on the tracked-client map. Reaching it triggers a sweep: idle
/// records first, then — because a distributed campaign or a spoofed forwarded
/// header keeps every record recent, so the idle sweep frees nothing — the
/// oldest records outright, down to `TRIM_TO`. This is what *bounds* the map:
/// `record_failure` never refuses a fresh client, so without a hard cap an
/// attacker cycling source identities grows it without limit. Evicting a
/// record only forgives that address's count; at this size an attacker must
/// still out-run every genuine failing client to be the one dropped.
const MAX_TRACKED: usize = 50_000;
/// Size the map is trimmed back to when it hits `MAX_TRACKED`, so the sweep
/// runs about once per `MAX_TRACKED - TRIM_TO` inserts rather than an O(n)
/// scan on every insert past the ceiling.
const TRIM_TO: usize = 40_000;

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
        // without bound. Sweeping at the ceiling keeps it proportional to the
        // recently failing clients, and falls back to a hard cap when they are
        // all recent.
        if self.clients.len() >= MAX_TRACKED {
            self.evict();
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

    /// Bring the map back under its ceiling. Idle records first — under normal
    /// churn this alone keeps it small, and it never evicts an active client.
    /// If that frees too little (the distributed-guessing case, where every
    /// record is recent), the oldest `last_seen` records are dropped until the
    /// map is at `TRIM_TO`, so it can never grow past `MAX_TRACKED`.
    fn evict(&self) {
        let now = Instant::now();
        self.clients
            .retain(|_, rec| now.duration_since(rec.last_seen) <= FAILURE_TTL);
        if self.clients.len() <= TRIM_TO {
            return;
        }
        // Snapshot (age, key) with the shard guards released before any
        // removal, so this never deadlocks against a concurrent insert; a key
        // already gone by the time we remove it is a harmless no-op.
        let mut by_age: Vec<(Instant, String)> = self
            .clients
            .iter()
            .map(|entry| (entry.last_seen, entry.key().clone()))
            .collect();
        let cut = by_age.len().saturating_sub(TRIM_TO);
        if cut == 0 {
            return;
        }
        // Partition so the `cut` oldest records sit in `[..cut]`; drop them.
        by_age.select_nth_unstable_by(cut - 1, |a, b| a.0.cmp(&b.0));
        for (_, key) in &by_age[..cut] {
            self.clients.remove(key);
        }
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

    #[tokio::test(start_paused = true)]
    async fn the_map_is_hard_capped_against_identity_cycling() {
        let t = FailedAuthTracker::default();
        // Every client is distinct and, on the paused clock, equally recent —
        // so the idle sweep can free nothing. This is the distributed-guessing
        // / spoofed-header case, and the map must still stay under its ceiling
        // rather than grow one entry per identity forever.
        for i in 0..(MAX_TRACKED + 5_000) {
            t.record_failure(&format!("client-{i}"));
        }
        assert!(
            t.clients.len() <= MAX_TRACKED,
            "map grew to {} past the {MAX_TRACKED} ceiling",
            t.clients.len()
        );
    }
}
