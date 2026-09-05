use serde::{Deserialize, Serialize};

use crate::config::validation::require_nonzero;
use crate::errors::OrionError;

/// Scheduler capacity — not schedules.
///
/// A schedule is definition content: it lives in a cron channel's
/// `transport_config`, is versioned with the channel and travels in a package.
/// This section is the *instance's* side of the contract — how often it looks
/// for work, how much it takes at once, and how long a claim is good for — so
/// two nodes running the same definitions can be sized differently without the
/// definitions changing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CronConfig {
    /// Whether this node schedules and runs cron occurrences.
    ///
    /// Turning it off does **not** quietly disable a stored schedule: an active
    /// cron channel on a node with `enabled = false` is quarantined and
    /// `components.cron` reports `degraded`, and activating one is refused.
    /// Accepting an active schedule that never fires is the one outcome an
    /// operator cannot detect, so it is the one outcome this refuses to
    /// produce. Drafts, imports, exports and reads are unaffected, so an
    /// instance with the scheduler off is still a place to author and promote
    /// schedules.
    pub enabled: bool,

    /// How often the reconciler looks for due instants and the workers look for
    /// claimable occurrences.
    ///
    /// This is the floor on how late a run can be under normal operation, and
    /// it is what `misfire_grace_secs` has to cover.
    pub poll_interval_ms: u64,

    /// Occurrences this node may execute concurrently, across every cron
    /// channel.
    ///
    /// Separate from `trace_queue.workers` on purpose: a large catch-up must
    /// not be able to occupy the workers that externally submitted `/async`
    /// work depends on.
    pub workers: usize,

    /// Occurrences claimed per poll. A claim is one round trip, so this trades
    /// database chatter against how quickly a backlog drains.
    pub claim_batch_size: i64,

    /// How long a claim is good for before another node may take the
    /// occurrence over.
    ///
    /// The floor, not the whole story: a running attempt extends its claim to
    /// cover its own timeout (see `default_timeout_ms`), so this is what
    /// governs the window between a node dying and its work being recovered.
    pub claim_lease_secs: u64,

    /// How often a running attempt renews its claim and its singleton.
    ///
    /// Must be below `claim_lease_secs` — validated — or a healthy attempt
    /// loses its own lease between beats.
    pub heartbeat_interval_secs: u64,

    /// How late an occurrence may start before it counts as a *misfire* rather
    /// than as ordinary polling delay.
    ///
    /// Without it, a schedule polled once a second would report a missed
    /// occurrence on every single pass, because nothing ever starts in the same
    /// instant it was due.
    pub misfire_grace_secs: u64,

    /// This instance's ceiling on a `catch_up` replay, whatever a channel asks
    /// for. The effective bound is `min(channel.max_catch_up, this)`.
    pub max_catch_up: u32,

    /// The deadline for an occurrence whose channel declares no `timeout_ms`.
    ///
    /// A *default*, not a ceiling — unlike Kafka's and the trace queue's, which
    /// protect a shared poll loop and a shared worker pool. A cron worker holds
    /// nothing but its own slot, so a channel that genuinely needs six hours
    /// may say so. The default is finite because the singleton lease is sized
    /// from it: an untimed workflow would hold its key until the process died.
    pub default_timeout_ms: u64,

    /// How long shutdown waits for in-flight occurrences before cancelling them
    /// and leaving their claims to expire.
    pub shutdown_timeout_secs: u64,
}

impl Default for CronConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_interval_ms: 1000,
            workers: 4,
            claim_batch_size: 20,
            claim_lease_secs: 60,
            heartbeat_interval_secs: 15,
            misfire_grace_secs: 5,
            max_catch_up: 100,
            default_timeout_ms: 3_600_000,
            shutdown_timeout_secs: 30,
        }
    }
}

/// The instance ceiling on `max_catch_up`, matching the per-channel authoring
/// ceiling. Above this a single restored schedule could enqueue more work than
/// an operator can inspect.
const MAX_CATCH_UP_CEILING: u32 = 1000;

/// The floor on the poll interval. Below this the reconciler spends more time
/// asking the database whether anything is due than anything else does working.
const MIN_POLL_INTERVAL_MS: u64 = 100;

impl CronConfig {
    pub(crate) fn validate(&self) -> Result<(), OrionError> {
        // Every value below is still checked when the scheduler is off: a
        // nonsense `[cron]` section should fail at `validate-config`, not on the
        // day someone turns the scheduler on.
        require_nonzero(self.poll_interval_ms, "cron.poll_interval_ms")?;
        require_nonzero(self.workers as u64, "cron.workers")?;
        require_nonzero(self.claim_lease_secs, "cron.claim_lease_secs")?;
        require_nonzero(self.heartbeat_interval_secs, "cron.heartbeat_interval_secs")?;
        require_nonzero(self.default_timeout_ms, "cron.default_timeout_ms")?;
        require_nonzero(self.shutdown_timeout_secs, "cron.shutdown_timeout_secs")?;
        require_nonzero(self.max_catch_up as u64, "cron.max_catch_up")?;

        if self.poll_interval_ms < MIN_POLL_INTERVAL_MS {
            return Err(OrionError::Config {
                message: format!(
                    "cron.poll_interval_ms must be at least {MIN_POLL_INTERVAL_MS} \
                     (got {}) — below that the reconciler polls faster than it can \
                     usefully act",
                    self.poll_interval_ms
                ),
            });
        }
        if self.claim_batch_size < 1 {
            return Err(OrionError::Config {
                message: "cron.claim_batch_size must be >= 1".to_string(),
            });
        }
        // The invariant the whole lease scheme rests on. Renewing no more often
        // than the lease lasts means a healthy attempt races its own expiry
        // every beat, and loses it whenever a database round trip is slow —
        // handing its occurrence to another node while it is still running.
        if self.heartbeat_interval_secs >= self.claim_lease_secs {
            return Err(OrionError::Config {
                message: format!(
                    "cron.heartbeat_interval_secs ({}) must be below \
                     cron.claim_lease_secs ({}): an attempt that renews no more often \
                     than its lease lasts will lose work it is still running",
                    self.heartbeat_interval_secs, self.claim_lease_secs
                ),
            });
        }
        if self.max_catch_up > MAX_CATCH_UP_CEILING {
            return Err(OrionError::Config {
                message: format!(
                    "cron.max_catch_up must be at most {MAX_CATCH_UP_CEILING} (got {})",
                    self.max_catch_up
                ),
            });
        }
        // The grace window is what separates "late because we poll every
        // second" from "missed because nothing was running". Shorter than the
        // poll interval, every occurrence looks missed.
        if self.misfire_grace_secs * 1000 < self.poll_interval_ms {
            return Err(OrionError::Config {
                message: format!(
                    "cron.misfire_grace_secs ({}s) must cover cron.poll_interval_ms \
                     ({}ms): a grace window shorter than the polling interval reports \
                     a misfire for work that was merely waiting for the next poll",
                    self.misfire_grace_secs, self.poll_interval_ms
                ),
            });
        }
        Ok(())
    }

    /// The reconciler and worker poll interval as a `Duration`.
    pub fn poll_interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.poll_interval_ms)
    }

    /// The misfire grace window, as the planner takes it.
    pub fn misfire_grace(&self) -> chrono::Duration {
        chrono::Duration::seconds(self.misfire_grace_secs as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_valid() {
        assert!(CronConfig::default().validate().is_ok());
    }

    #[test]
    fn a_heartbeat_at_or_above_the_lease_is_refused() {
        for heartbeat in [60, 61] {
            let config = CronConfig {
                heartbeat_interval_secs: heartbeat,
                claim_lease_secs: 60,
                ..Default::default()
            };
            let message = config.validate().expect_err("must refuse").to_string();
            assert!(message.contains("heartbeat_interval_secs"), "{message}");
        }
        assert!(
            CronConfig {
                heartbeat_interval_secs: 59,
                claim_lease_secs: 60,
                ..Default::default()
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn a_grace_window_shorter_than_the_poll_interval_is_refused() {
        let config = CronConfig {
            poll_interval_ms: 5000,
            misfire_grace_secs: 1,
            ..Default::default()
        };
        let message = config.validate().expect_err("must refuse").to_string();
        assert!(message.contains("misfire_grace_secs"), "{message}");
    }

    #[test]
    fn the_bounds_are_enforced() {
        assert!(
            CronConfig {
                poll_interval_ms: 10,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            CronConfig {
                max_catch_up: MAX_CATCH_UP_CEILING + 1,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            CronConfig {
                claim_batch_size: 0,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }

    /// A disabled scheduler still has to have a coherent section: the failure
    /// should land at `validate-config`, not on the day it is switched on.
    #[test]
    fn a_disabled_scheduler_is_still_validated() {
        let config = CronConfig {
            enabled: false,
            workers: 0,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }
}
