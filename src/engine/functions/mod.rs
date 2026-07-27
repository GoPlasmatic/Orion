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
use std::sync::Arc;
use std::time::Duration;

use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::message::Message;
use dataflow_rs::engine::task_context::TaskContext;
use serde_json::Value;

use crate::connector::circuit_breaker::CircuitBreaker;

/// Resolve a URL path from a static string or a pre-compiled JSONLogic expression.
///
/// In v3 the engine pre-compiles JSONLogic from the typed `HttpCallConfig`
/// at startup, so handlers pass the cached `Arc<Logic>` rather than the raw
/// `serde_json::Value`. Evaluation uses the engine inside the
/// [`TaskContext`].
pub fn resolve_path(
    static_path: &Option<String>,
    compiled_logic: Option<&datalogic_rs::Logic>,
    ctx: &TaskContext<'_>,
) -> dataflow_rs::Result<Option<String>> {
    if let Some(compiled) = compiled_logic {
        let result: Value = ctx
            .datalogic()
            .session()
            .eval_into(compiled, &ctx.message().context)
            .map_err(|e| DataflowError::LogicEvaluation(e.to_string()))?;
        let path_str = if let Some(s) = result.as_str() {
            s.to_string()
        } else {
            serde_json::to_string(&result).map_err(|e| {
                DataflowError::function_execution(
                    format!("Failed to serialize resolved path: {e}"),
                    None,
                )
            })?
        };
        Ok(Some(path_str))
    } else {
        Ok(static_path.clone())
    }
}

/// Convert a dataflow HttpMethod to a reqwest Method.
pub fn to_reqwest_method(
    method: &dataflow_rs::engine::functions::integration::HttpMethod,
) -> reqwest::Method {
    match method {
        dataflow_rs::engine::functions::integration::HttpMethod::Get => reqwest::Method::GET,
        dataflow_rs::engine::functions::integration::HttpMethod::Post => reqwest::Method::POST,
        dataflow_rs::engine::functions::integration::HttpMethod::Put => reqwest::Method::PUT,
        dataflow_rs::engine::functions::integration::HttpMethod::Patch => reqwest::Method::PATCH,
        dataflow_rs::engine::functions::integration::HttpMethod::Delete => reqwest::Method::DELETE,
    }
}

/// Extract the channel name from a message's metadata.
pub fn extract_channel(message: &Message) -> &str {
    message
        .metadata()
        .get("channel")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
}

/// Execute an operation wrapped with circuit breaker + retry.
pub async fn execute_with_circuit_breaker<F, Fut>(
    breaker: &Arc<CircuitBreaker>,
    connector: &str,
    channel: &str,
    policy: RetryPolicy,
    label: &str,
    operation: F,
) -> dataflow_rs::Result<Value>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = dataflow_rs::Result<Value>>,
{
    if !breaker.check() {
        crate::metrics::record_circuit_breaker_rejection(connector, channel);
        // Carries through the engine to a 503 CIRCUIT_OPEN (see
        // `crate::errors::CIRCUIT_OPEN_MARKER`); `function_execution` used to
        // land in the catch-all and surface as a 500 ENGINE_ERROR.
        return Err(crate::errors::circuit_open_dataflow_error(
            connector, channel,
        ));
    }

    let start = std::time::Instant::now();
    let result = retry_with_policy(policy, label, operation).await;
    let duration_secs = start.elapsed().as_secs_f64();

    match &result {
        Ok(_) => {
            breaker.record_success();
            crate::metrics::record_connector_request(connector, channel, "ok");
        }
        Err(_) => {
            crate::metrics::record_connector_request(connector, channel, "error");
            if breaker.record_failure() {
                tracing::warn!(
                    connector = connector,
                    channel = channel,
                    "Circuit breaker tripped"
                );
                crate::metrics::record_circuit_breaker_trip(connector, channel);
            }
        }
    }
    crate::metrics::record_connector_duration(connector, channel, duration_secs);

    result
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
    let started = std::time::Instant::now();

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
    use crate::connector::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn test_to_reqwest_method_get() {
        use dataflow_rs::engine::functions::integration::HttpMethod;
        assert_eq!(to_reqwest_method(&HttpMethod::Get), reqwest::Method::GET);
    }

    #[test]
    fn test_to_reqwest_method_post() {
        use dataflow_rs::engine::functions::integration::HttpMethod;
        assert_eq!(to_reqwest_method(&HttpMethod::Post), reqwest::Method::POST);
    }

    #[test]
    fn test_to_reqwest_method_put() {
        use dataflow_rs::engine::functions::integration::HttpMethod;
        assert_eq!(to_reqwest_method(&HttpMethod::Put), reqwest::Method::PUT);
    }

    #[test]
    fn test_to_reqwest_method_patch() {
        use dataflow_rs::engine::functions::integration::HttpMethod;
        assert_eq!(
            to_reqwest_method(&HttpMethod::Patch),
            reqwest::Method::PATCH
        );
    }

    #[test]
    fn test_to_reqwest_method_delete() {
        use dataflow_rs::engine::functions::integration::HttpMethod;
        assert_eq!(
            to_reqwest_method(&HttpMethod::Delete),
            reqwest::Method::DELETE
        );
    }

    #[test]
    fn test_extract_channel_with_channel() {
        let mut message = Message::from_value(&serde_json::json!({"key": "val"}));
        dataflow_rs::engine::utils::set_nested_value(
            &mut message.context,
            "metadata.channel",
            datavalue::OwnedDataValue::from("orders".to_string()),
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
        let result = retry(3, 1, || async {
            Ok(serde_json::json!({"ok": true}))
        })
        .await;
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

    #[tokio::test]
    async fn test_execute_with_circuit_breaker_success() {
        let config = CircuitBreakerConfig {
            enabled: true,
            failure_threshold: 5,
            recovery_timeout_secs: 30,
            ..Default::default()
        };
        let breaker = Arc::new(CircuitBreaker::new(config));

        let result = execute_with_circuit_breaker(
            &breaker,
            "test-connector",
            "test-channel",
            RetryPolicy {
                max_retries: 0,
                retry_delay_ms: 1,
                deadline: None,
            },
            "test",
            || async { Ok(serde_json::json!({"result": "ok"})) },
        )
        .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_execute_with_circuit_breaker_open_rejects() {
        let config = CircuitBreakerConfig {
            enabled: true,
            failure_threshold: 1,
            recovery_timeout_secs: 300,
            ..Default::default()
        };
        let breaker = Arc::new(CircuitBreaker::new(config));

        breaker.record_failure();

        let result = execute_with_circuit_breaker(
            &breaker,
            "test-connector",
            "test-channel",
            RetryPolicy {
                max_retries: 0,
                retry_delay_ms: 1,
                deadline: None,
            },
            "test",
            || async { Ok(serde_json::json!({"should": "not reach"})) },
        )
        .await;

        let err = result.expect_err("test");
        assert!(err.to_string().contains("Circuit breaker open"));
        // F5: the carrier must be retryable and recognisable as CIRCUIT_OPEN.
        assert!(err.retryable(), "an open breaker is a retry-later signal");
        assert!(matches!(err, DataflowError::Http { status: 503, .. },));
    }

    #[tokio::test]
    async fn test_execute_with_circuit_breaker_records_failure() {
        let config = CircuitBreakerConfig {
            enabled: true,
            failure_threshold: 5,
            recovery_timeout_secs: 300,
            ..Default::default()
        };
        let breaker = Arc::new(CircuitBreaker::new(config));

        let result: Result<Value, _> = execute_with_circuit_breaker(
            &breaker,
            "test-connector",
            "test-channel",
            RetryPolicy {
                max_retries: 0,
                retry_delay_ms: 1,
                deadline: None,
            },
            "test",
            || async { Err(DataflowError::Io("network error".to_string())) },
        )
        .await;

        assert!(result.is_err());
        assert!(breaker.check());
    }
}
