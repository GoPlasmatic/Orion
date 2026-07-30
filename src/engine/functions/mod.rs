pub mod channel_call;
pub mod connector_helpers;
pub mod http_call;
pub mod http_common;
pub mod publish_kafka;
pub mod schema;

pub mod cache_read;
pub mod cache_write;
pub mod data_query;
pub mod data_write;
pub mod db_read;
pub mod db_write;
pub mod mongo_read;

use std::future::Future;
use std::time::Duration;

use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::message::Message;
use serde_json::Value;

/// Convert a dataflow `HttpMethod` to a reqwest `Method`.
///
/// `HttpMethod::as_str` (dataflow-rs 3.1) is tied by an upstream test to the
/// enum's own `Deserialize` spelling, so the token is always a valid method
/// name and `from_bytes` cannot actually fail. The fallback keeps this
/// infallible rather than asserting on that.
pub fn to_reqwest_method(method: &dataflow_rs::HttpMethod) -> reqwest::Method {
    reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET)
}

/// Extract the channel name from a message's metadata.
pub fn extract_channel(message: &Message) -> &str {
    message
        .metadata()
        .get("channel")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
}

/// How a retry loop is bounded (F8).
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub retry_delay_ms: u64,
    /// Wall-clock ceiling for the whole loop, attempts and backoff included.
    /// Without it a single `http_call` could run ~127 s under a 30 s channel
    /// timeout: individual backoffs are capped at 60 s but total elapsed was
    /// never checked.
    pub deadline: Option<Duration>,
}

pub async fn retry_with_policy<F, Fut>(
    policy: RetryPolicy,
    label: &str,
    mut operation: F,
) -> dataflow_rs::Result<Value>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = dataflow_rs::Result<Value>>,
{
    let mut last_error = None;
    // tokio's Instant: shares a (pausable) clock with the backoff sleeps
    // below, so the retry deadline stays coherent under a paused test clock.
    let started = tokio::time::Instant::now();

    const MAX_BACKOFF_MS: u64 = 60_000;

    for attempt in 0..=policy.max_retries {
        if attempt > 0 {
            let delay = policy
                .retry_delay_ms
                .saturating_mul(1u64.checked_shl(attempt - 1).unwrap_or(u64::MAX))
                .min(MAX_BACKOFF_MS);
            // Sleeping past the deadline just to fail is wasted latency the
            // caller is already waiting on.
            if let Some(deadline) = policy.deadline
                && started.elapsed() + Duration::from_millis(delay) >= deadline
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }

        match operation().await {
            Ok(val) => return Ok(val),
            Err(e) => {
                let deadline_left = policy
                    .deadline
                    .is_none_or(|deadline| started.elapsed() < deadline);
                if e.retryable() && attempt < policy.max_retries && deadline_left {
                    tracing::warn!(
                        attempt = attempt + 1,
                        max = policy.max_retries,
                        error = %e,
                        "{} failed, retrying",
                        label
                    );
                    last_error = Some(e);
                    continue;
                }
                return Err(e);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| DataflowError::Unknown("Retry loop exhausted".into())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Five one-assert tests used to sit here, one per variant, and a new
    /// upstream variant would have gone untested. Driving off `HttpMethod::ALL`
    /// covers whatever the enum holds, and the `unwrap_or(GET)` fallback in
    /// `to_reqwest_method` means a name reqwest cannot parse is silent — so
    /// assert the round trip, not just that a `Method` came back.
    #[test]
    fn every_http_method_maps_to_the_same_reqwest_verb() {
        for method in dataflow_rs::HttpMethod::ALL {
            assert_eq!(
                to_reqwest_method(method).as_str(),
                method.as_str(),
                "{method} did not round-trip through reqwest::Method"
            );
        }
    }

    #[test]
    fn test_extract_channel_with_channel() {
        let mut message = Message::from_value(&serde_json::json!({"key": "val"}));
        dataflow_rs::engine::utils::set_nested_value(
            &mut message.context,
            "metadata.channel",
            dataflow_rs::datavalue::OwnedDataValue::from("orders".to_string()),
        );
        assert_eq!(extract_channel(&message), "orders");
    }

    #[test]
    fn test_extract_channel_without_channel() {
        let message = Message::from_value(&serde_json::json!({}));
        assert_eq!(extract_channel(&message), "unknown");
    }

    /// Old `retry_with_backoff` shape: undeadlined policy. The production
    /// callers all use `retry_with_policy` directly.
    async fn retry<F, Fut>(max_retries: u32, delay_ms: u64, op: F) -> dataflow_rs::Result<Value>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = dataflow_rs::Result<Value>>,
    {
        retry_with_policy(
            RetryPolicy {
                max_retries,
                retry_delay_ms: delay_ms,
                deadline: None,
            },
            "test",
            op,
        )
        .await
    }

    #[tokio::test]
    async fn test_retry_succeeds_first_try() {
        let result = retry(3, 1, || async { Ok(serde_json::json!({"ok": true})) }).await;
        assert!(result.is_ok());
        assert_eq!(result.expect("test"), serde_json::json!({"ok": true}));
    }

    #[tokio::test]
    async fn test_retry_fails_then_succeeds() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();
        let result = retry(3, 1, move || {
            let c = counter_clone.clone();
            async move {
                let attempt = c.fetch_add(1, Ordering::SeqCst);
                if attempt < 2 {
                    Err(DataflowError::Io("transient".to_string()))
                } else {
                    Ok(serde_json::json!({"attempt": attempt}))
                }
            }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_retry_non_retryable_fails_immediately() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();
        let result = retry(3, 1, move || {
            let c = counter_clone.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(DataflowError::Validation("bad input".to_string()))
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_retry_exhausts_retries() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();
        let result = retry(2, 1, move || {
            let c = counter_clone.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(DataflowError::Io("always fails".to_string()))
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    /// F8: the loop must stop retrying once the deadline is spent, rather
    /// than running attempts + backoff far past the caller's budget.
    #[tokio::test]
    async fn test_retry_policy_honours_deadline() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = attempts.clone();
        let start = std::time::Instant::now();
        let result = retry_with_policy(
            RetryPolicy {
                max_retries: 10,
                retry_delay_ms: 200,
                deadline: Some(Duration::from_millis(250)),
            },
            "deadline test",
            || {
                let seen = seen.clone();
                async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    Err::<Value, _>(DataflowError::Timeout("nope".into()))
                }
            },
        )
        .await;
        assert!(result.is_err());
        // 10 retries at 200ms exponential backoff would take minutes.
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "loop must stop at the deadline, took {:?}",
            start.elapsed()
        );
        assert!(
            attempts.load(Ordering::SeqCst) < 5,
            "deadline must cut the attempt count, got {}",
            attempts.load(Ordering::SeqCst)
        );
    }
}
