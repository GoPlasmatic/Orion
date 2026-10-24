//! Retrying an operation that failed for a reason worth retrying.
//!
//! Vendored from dataflow-rs 3.15.0's `src/engine/retry.rs`
//! (<https://github.com/GoPlasmatic/dataflow-rs>, Apache-2.0), whose loop
//! `http_call` has run since dataflow-rs 3.7. dataflow-rs 4 ships no retry
//! loop — whether a call is worth repeating is the host's policy, since only
//! the host knows which calls are idempotent — so Orion keeps the loop here.
//! The behaviour is 3.15.0's: the same three policy fields, the same doubling
//! backoff under a 60s per-sleep ceiling, the same whole-loop deadline with a
//! backoff that would cross it skipped rather than slept, and the same
//! `tokio::time::Instant`, so a paused test clock stays coherent with the
//! sleeps.
//!
//! Two changes from the original: the loop is generic over [`Retryable`], and
//! its debug lines go through `tracing`, Orion's logger, rather than `log`.
//!
//! It is deliberately **not** engine-level automatic retry: the engine cannot
//! know which handlers are idempotent — an SMTP send that times out after
//! `DATA` is indistinguishable from one that succeeded, and retrying
//! duplicates the mail. Whether to retry stays a per-function, per-call-site
//! decision; this supplies the correct loop for those that opt in.

use std::fmt::Display;
use std::future::Future;
use std::time::Duration;
use tokio::time::Instant;

/// The longest a single backoff sleep may grow to, however many attempts have
/// failed. Without a ceiling, doubling reaches minutes and a caller waiting on
/// the result has no idea why.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// An error that says whether trying again may succeed.
pub trait Retryable {
    /// May a retry succeed?
    fn is_retryable(&self) -> bool;
}

/// The engine's classification, `Error::is_retryable`: a timeout, or what a
/// service error declares.
impl Retryable for dataflow_rs::Error {
    fn is_retryable(&self) -> bool {
        dataflow_rs::Error::is_retryable(self)
    }
}

/// How hard to retry, and for how long overall.
///
/// ```
/// use orion::engine::retry::RetryPolicy;
/// use std::time::Duration;
///
/// // Three retries, 100ms doubling, but never more than 5s in total.
/// let policy = RetryPolicy {
///     max_retries: 3,
///     retry_delay_ms: 100,
///     deadline: Some(Duration::from_secs(5)),
/// };
/// assert_eq!(policy.max_retries, 3);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries *after* the first attempt. `0` means try once and give up.
    pub max_retries: u32,
    /// Base delay. Doubles per attempt, capped at 60s.
    pub retry_delay_ms: u64,
    /// Wall-clock ceiling for the **whole** loop, sleeps included.
    ///
    /// Without this, a call with a 30s per-attempt timeout and capped backoff
    /// can run to roughly 127s under a 30s caller budget — the per-attempt
    /// bound says nothing about the total.
    pub deadline: Option<Duration>,
}

impl Default for RetryPolicy {
    /// Three retries, 100ms base delay, no deadline.
    fn default() -> Self {
        Self {
            max_retries: 3,
            retry_delay_ms: 100,
            deadline: None,
        }
    }
}

impl RetryPolicy {
    /// A policy that never retries. Useful as an explicit opt-out at a call
    /// site that takes a policy.
    pub fn none() -> Self {
        Self {
            max_retries: 0,
            retry_delay_ms: 0,
            deadline: None,
        }
    }

    /// The backoff before retry number `retry` (1-based), capped.
    fn backoff(&self, retry: u32) -> Duration {
        let doubled = self
            .retry_delay_ms
            .checked_mul(1u64 << retry.min(32).saturating_sub(1))
            .map_or(MAX_BACKOFF, Duration::from_millis);
        doubled.min(MAX_BACKOFF)
    }
}

/// Run `operation`, retrying while it fails retryably and budget remains.
///
/// Retries only when [`Retryable::is_retryable`] says so — a validation
/// error fails once and returns immediately, because trying it again cannot
/// help.
///
/// # Deadline
///
/// The deadline covers the whole loop, sleeps included. A backoff that would
/// cross it is **skipped** rather than slept: sleeping only to fail afterwards
/// spends latency the caller is already waiting on. The loop then ends with the
/// last error.
///
/// Timing uses [`tokio::time::Instant`], so the deadline stays coherent with
/// the sleeps under `tokio::time::pause()`.
///
/// # Errors
///
/// The last attempt's error, once retrying stops.
pub async fn retry_with_policy<T, E, F, Fut>(
    policy: RetryPolicy,
    label: &str,
    operation: F,
) -> Result<T, E>
where
    E: Retryable + Display,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    retry_with_attempts(policy, label, operation).await.0
}

/// As [`retry_with_policy`], also reporting how many attempts were made: the
/// only way a caller can say "this succeeded, but not first time".
pub async fn retry_with_attempts<T, E, F, Fut>(
    policy: RetryPolicy,
    label: &str,
    mut operation: F,
) -> (Result<T, E>, u32)
where
    E: Retryable + Display,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let started = Instant::now();
    let mut attempts = 0u32;
    let mut last: E;

    loop {
        attempts += 1;
        match operation().await {
            Ok(value) => return (Ok(value), attempts),
            Err(err) => last = err,
        }

        if !last.is_retryable() {
            tracing::debug!("{label}: not retryable after {attempts} attempt(s): {last}");
            return (Err(last), attempts);
        }

        let retry = attempts; // the retry we are about to consider, 1-based
        if retry > policy.max_retries {
            tracing::debug!("{label}: giving up after {attempts} attempt(s): {last}");
            return (Err(last), attempts);
        }

        let backoff = policy.backoff(retry);

        // The deadline covers the sleep too. Sleeping and *then* failing spends
        // latency the caller is already waiting on, so a backoff that would
        // cross the line ends the loop instead.
        if let Some(deadline) = policy.deadline {
            let elapsed = started.elapsed();
            if elapsed + backoff >= deadline {
                tracing::debug!(
                    "{label}: deadline {deadline:?} leaves no room for a {backoff:?} backoff \
                     after {elapsed:?}; stopping at {attempts} attempt(s)"
                );
                return (Err(last), attempts);
            }
        }

        tracing::debug!("{label}: retry {retry} in {backoff:?} after: {last}");
        tokio::time::sleep(backoff).await;
    }
}

/// dataflow-rs 3.15.0's own tests of the loop (`tests/retry.rs`), over the
/// engine's `Error`.
///
/// Every timing assertion runs under a paused clock, so the clock only
/// advances when a sleep asks it to. That makes the deadline arithmetic
/// exactly checkable rather than approximately.
#[cfg(test)]
mod tests {
    use super::*;
    use dataflow_rs::Error as DataflowError;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    type Result<T> = std::result::Result<T, DataflowError>;

    /// Counts calls and fails the first `fail_times` of them.
    fn flaky(
        fail_times: u32,
        err: fn() -> DataflowError,
    ) -> (
        Arc<AtomicU32>,
        impl FnMut() -> std::future::Ready<Result<u32>>,
    ) {
        let calls = Arc::new(AtomicU32::new(0));
        let c = Arc::clone(&calls);
        let op = move || {
            let n = c.fetch_add(1, Ordering::SeqCst) + 1;
            std::future::ready(if n > fail_times { Ok(n) } else { Err(err()) })
        };
        (calls, op)
    }

    fn transient() -> DataflowError {
        DataflowError::service("TIMEOUT_ERROR", "upstream").with_retryable(true)
    }

    fn permanent() -> DataflowError {
        DataflowError::service("VALIDATION_ERROR", "bad input")
    }

    #[tokio::test(start_paused = true)]
    async fn a_transient_failure_is_retried_until_it_succeeds() {
        let (calls, op) = flaky(2, transient);
        let out = retry_with_policy(
            RetryPolicy {
                max_retries: 3,
                retry_delay_ms: 100,
                deadline: None,
            },
            "svc",
            op,
        )
        .await;

        assert_eq!(out.expect("the third attempt succeeds"), 3);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "two failures, then success"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_non_retryable_error_returns_immediately() {
        let (calls, op) = flaky(u32::MAX, permanent);
        let out = retry_with_policy(
            RetryPolicy {
                max_retries: 5,
                retry_delay_ms: 100,
                deadline: None,
            },
            "svc",
            op,
        )
        .await;

        assert!(out.is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "retrying a validation error cannot help, so it must not be tried again"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retries_stop_at_max_retries() {
        let (calls, op) = flaky(u32::MAX, transient);
        let (out, attempts) = retry_with_attempts(
            RetryPolicy {
                max_retries: 3,
                retry_delay_ms: 10,
                deadline: None,
            },
            "svc",
            op,
        )
        .await;

        assert!(out.is_err());
        assert_eq!(attempts, 4, "the first attempt plus three retries");
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_doubles_and_then_caps() {
        // 100, 200, 400, 800 … capped at 60s. Measured on the paused clock, so
        // these are exact.
        let start = tokio::time::Instant::now();
        let (_, op) = flaky(u32::MAX, transient);
        let _ = retry_with_policy(
            RetryPolicy {
                max_retries: 3,
                retry_delay_ms: 100,
                deadline: None,
            },
            "svc",
            op,
        )
        .await;

        assert_eq!(
            start.elapsed(),
            Duration::from_millis(100 + 200 + 400),
            "three backoffs, doubling"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_is_capped_at_sixty_seconds() {
        let start = tokio::time::Instant::now();
        let (_, op) = flaky(u32::MAX, transient);
        let _ = retry_with_policy(
            // 40s, then 80s -> capped to 60s.
            RetryPolicy {
                max_retries: 2,
                retry_delay_ms: 40_000,
                deadline: None,
            },
            "svc",
            op,
        )
        .await;

        assert_eq!(
            start.elapsed(),
            Duration::from_secs(40 + 60),
            "the second backoff is clamped rather than doubling to 80s"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_backoff_that_would_cross_the_deadline_is_skipped_entirely() {
        // The load-bearing behaviour: sleeping and *then* failing spends latency the
        // caller is already waiting on. With a 250ms deadline the third backoff
        // (400ms) cannot fit, so the loop ends without sleeping it.
        let start = tokio::time::Instant::now();
        let (calls, op) = flaky(u32::MAX, transient);

        let (out, attempts) = retry_with_attempts(
            RetryPolicy {
                max_retries: 10,
                retry_delay_ms: 100,
                deadline: Some(Duration::from_millis(250)),
            },
            "svc",
            op,
        )
        .await;

        // t=0   attempt 1 fails; 100ms backoff fits inside 250ms, so sleep it.
        // t=100 attempt 2 fails; the next backoff is 200ms and 100+200 >= 250,
        //       so the loop ends here rather than sleeping into a failure.
        assert!(out.is_err());
        assert_eq!(attempts, 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            start.elapsed(),
            Duration::from_millis(100),
            "only the first backoff was slept"
        );
        assert!(
            start.elapsed() < Duration::from_millis(250),
            "and the loop came in under its own deadline rather than sleeping to 300ms \
             and failing anyway, which is what an unguarded backoff would do"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_deadline_bounds_the_whole_loop_not_each_attempt() {
        // Each attempt is instant here, so any overrun is the sleeps — which is the
        // case a per-attempt timeout cannot bound.
        let start = tokio::time::Instant::now();
        let (_, op) = flaky(u32::MAX, transient);

        let _ = retry_with_policy(
            RetryPolicy {
                max_retries: 20,
                retry_delay_ms: 1_000,
                deadline: Some(Duration::from_secs(10)),
            },
            "svc",
            op,
        )
        .await;

        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the loop finished inside its budget, got {:?}",
            start.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn max_retries_zero_tries_once() {
        let (calls, op) = flaky(u32::MAX, transient);
        let (out, attempts) = retry_with_attempts(RetryPolicy::none(), "svc", op).await;

        assert!(out.is_err());
        assert_eq!(attempts, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_service_error_is_retried_only_when_it_says_so() {
        for (retryable, expected_calls) in [(true, 3u32), (false, 1)] {
            let calls = Arc::new(AtomicU32::new(0));
            let c = Arc::clone(&calls);
            let op = move || {
                c.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Err::<u32, _>(
                    DataflowError::service("upstream", "boom").with_retryable(retryable),
                ))
            };

            let _ = retry_with_policy(
                RetryPolicy {
                    max_retries: 2,
                    retry_delay_ms: 1,
                    deadline: None,
                },
                "svc",
                op,
            )
            .await;

            assert_eq!(
                calls.load(Ordering::SeqCst),
                expected_calls,
                "a Service error declaring retryable={retryable} must be honoured"
            );
        }
    }

    /// A class-built error is retried
    /// exactly when it declares itself retryable, which is what the kit's
    /// class mapping sets for `IO_ERROR` and `TIMEOUT_ERROR`.
    #[tokio::test(start_paused = true)]
    async fn a_class_built_error_is_retried_by_its_own_retryability() {
        use crate::engine::{ErrorClass, HandlerError};

        for (class, expected_calls) in [
            (ErrorClass::Connector, 3u32),
            (ErrorClass::Timeout, 3),
            (ErrorClass::CallerInput, 1),
            (ErrorClass::Backend, 1),
        ] {
            let calls = Arc::new(AtomicU32::new(0));
            let c = Arc::clone(&calls);
            let op = move || {
                c.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Err::<u32, dataflow_rs::Error>(
                    HandlerError::new(class, "boom").into(),
                ))
            };
            let (out, attempts) = retry_with_attempts(
                RetryPolicy {
                    max_retries: 2,
                    retry_delay_ms: 1,
                    deadline: None,
                },
                "svc",
                op,
            )
            .await;
            assert!(out.is_err());
            assert_eq!(attempts, expected_calls, "{class:?}");
            assert_eq!(calls.load(Ordering::SeqCst), expected_calls, "{class:?}");
        }
    }
}
