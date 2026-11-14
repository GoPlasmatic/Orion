//! The store a cluster's nodes share for hot-path state: dedup claims, the
//! response cache, namespace invalidation counters and fleet-wide rate
//! windows.
//!
//! Community's is the node's state database. An edition may register its
//! own ([`crate::Registrar::cluster_store`]), e.g. Redis. Coordination of record (breaker epochs, job leases, DLQ
//! claims, the config epoch) stays on the state database whatever is
//! registered here.

use async_trait::async_trait;

/// The cluster's shared hot-path store. The node uses it only with
/// `cluster.enabled = true`; a single node keeps all of this in process.
///
/// Every method is called from many nodes at once, so each must hold the
/// guarantee stated on it **across the fleet**, not just within one process.
/// `orion::testing::shared_store_contract` checks an implementation against
/// this contract.
///
/// # Semantics
///
/// - **Keys arrive unhashed**, as the node builds them. A store may prefix or
///   hash them (the database store hashes them to fit a fixed-width key).
/// - **Clock.** Every expiry and every window is judged on the *store's*
///   clock, never a node's: nodes with skewed clocks must still agree on
///   whether a key is live and which second a hit falls in.
/// - **TTLs** are whole seconds. `None` never expires. `Some(0)` is already
///   expired: the write is accepted and a later read finds nothing.
/// - **Errors.** [`StoreErrorKind::Invalid`] is the caller's mistake (a value
///   that cannot be incremented); [`StoreErrorKind::Unavailable`] is
///   everything else, including an answer the store could not decide. The
///   node hands a failure to the channel's `on_backend_error` policy (fail
///   open by default, `deny` answers 503); a response-cache read error is a
///   miss.
///
/// Implement it with `#[async_trait]`.
#[async_trait]
pub trait SharedStateStore: Send + Sync + 'static {
    /// The store's name, as readiness (`/readyz`'s `cluster_store`), logs
    /// and errors report it (`"database"`, `"redis"`).
    fn name(&self) -> &str;

    /// The value at `key`, unless absent or expired.
    async fn get(&self, key: &str) -> Result<Option<String>, StoreError>;

    /// The values at `keys`, **in the order asked**, `None` for each miss.
    async fn get_many(&self, keys: &[String]) -> Result<Vec<Option<String>>, StoreError>;

    /// Store `value` at `key`, replacing what is there (value and expiry),
    /// expiring after `ttl_secs` seconds (never, with `None`).
    async fn set(&self, key: &str, value: &str, ttl_secs: Option<u64>) -> Result<(), StoreError>;

    /// Delete `keys`, answering how many were present and live. An absent
    /// or expired key is not an error and does not count.
    async fn remove_many(&self, keys: &[String]) -> Result<u64, StoreError>;

    /// Claim `key` for `owner` for `window_secs` seconds.
    ///
    /// - `None`: the claim is won, because the key was free or its window
    ///   had passed (an expired claim can be taken over).
    /// - `Some(holder)`: someone holds it; `holder` is the owner string the
    ///   winner claimed with, so a redelivery that finds its own token knows
    ///   it may reprocess.
    ///
    /// Exactly one of any number of concurrent claimants wins. A claim whose
    /// outcome the store cannot decide is an [`StoreErrorKind::Unavailable`]
    /// error, **never** a win: a guessed win would run a duplicate.
    async fn claim(
        &self,
        key: &str,
        owner: &str,
        window_secs: u64,
    ) -> Result<Option<String>, StoreError>;

    /// Add `by` to the integer at `key` and answer the new value.
    ///
    /// - Atomic: no increment is ever lost, however many run at once.
    /// - A missing or expired key counts as `0`.
    /// - `ttl_secs` applies only when the key is created (or restarts after
    ///   expiring); incrementing a live key leaves its expiry alone.
    /// - A value that is not an integer, or a result that overflows `i64`, is
    ///   [`StoreErrorKind::Invalid`].
    async fn incr_by(&self, key: &str, by: i64, ttl_secs: Option<u64>) -> Result<i64, StoreError>;

    /// Count one hit against `key` in the current one-second window on the
    /// store's clock, answering the window's count after it. Windows are
    /// fixed and one second wide; the next second starts again at 1.
    ///
    /// The node allows `rps + burst` (at least 1) per window, so up to twice
    /// that may pass across a window boundary.
    async fn hit_window(&self, key: &str) -> Result<i64, StoreError>;

    /// Whether the store answers, for readiness and the boot check.
    async fn ping(&self) -> Result<(), StoreError>;

    /// Whether expired keys and finished windows must be deleted by the
    /// node ([`Self::sweep`], run periodically on every node). A store that
    /// expires its own keys (Redis) answers `false`, the default.
    fn needs_sweep(&self) -> bool {
        false
    }

    /// Delete what no reader can see any more. Runs on every node at once,
    /// so it may only remove what is already dead to every reader. Called
    /// only when [`Self::needs_sweep`] is `true`.
    async fn sweep(&self) -> Result<(), StoreError> {
        Ok(())
    }
}

/// Why a [`SharedStateStore`] call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct StoreError {
    kind: StoreErrorKind,
    message: String,
}

impl StoreError {
    /// The store did not answer, or could not decide the answer.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: StoreErrorKind::Unavailable,
            message: message.into(),
        }
    }

    /// The call cannot succeed as asked (e.g. incrementing text).
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: StoreErrorKind::Invalid,
            message: message.into(),
        }
    }

    pub fn kind(&self) -> StoreErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

/// The class of a [`StoreError`]. Non-exhaustive: treat an unknown kind as
/// [`Self::Unavailable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StoreErrorKind {
    /// The store did not answer, or could not decide the answer. Handed to
    /// the channel's `on_backend_error` policy.
    Unavailable,
    /// The call cannot succeed as asked: the node reports it as a
    /// validation error.
    Invalid,
}
