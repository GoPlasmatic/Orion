pub mod http_call;
pub mod http_common;
pub mod publish_kafka;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::message::Message;
use datalogic_rs::DataLogic;
use serde_json::Value;

use crate::connector::circuit_breaker::CircuitBreaker;

/// Resolve a URL path from a static string or a JSONLogic expression.
pub fn resolve_path(
    static_path: &Option<String>,
    path_logic: &Option<Value>,
    message: &mut Message,
    datalogic: &DataLogic,
) -> dataflow_rs::Result<Option<String>> {
    if let Some(logic) = path_logic {
        let context = message.get_context_arc();
        let compiled = datalogic
            .compile(logic)
            .map_err(|e| DataflowError::LogicEvaluation(e.to_string()))?;
        let result = datalogic
            .evaluate(&compiled, context)
            .map_err(|e| DataflowError::LogicEvaluation(e.to_string()))?;
        let path_str = if let Some(s) = result.as_str() {
            s.to_string()
        } else {
            serde_json::to_string(&result).map_err(|e| {
                DataflowError::function_execution(
                    format!("Failed to serialize resolved path: {}", e),
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
///
/// 1. Check breaker — reject early if open
/// 2. Run retry_with_backoff
/// 3. Record success/failure on the breaker
pub async fn execute_with_circuit_breaker<F, Fut>(
    breaker: &Arc<CircuitBreaker>,
    connector: &str,
    channel: &str,
    max_retries: u32,
    retry_delay_ms: u64,
    label: &str,
    operation: F,
) -> dataflow_rs::Result<Value>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = dataflow_rs::Result<Value>>,
{
    if !breaker.check() {
        crate::metrics::record_circuit_breaker_rejection(connector, channel);
        return Err(DataflowError::function_execution(
            format!(
                "Circuit breaker open for connector '{}' on channel '{}'",
                connector, channel
            ),
            None,
        ));
    }

    let result = retry_with_backoff(max_retries, retry_delay_ms, label, operation).await;

    match &result {
        Ok(_) => breaker.record_success(),
        Err(_) => {
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

    result
}

/// Execute an async operation with exponential backoff retry.
pub async fn retry_with_backoff<F, Fut>(
    max_retries: u32,
    retry_delay_ms: u64,
    label: &str,
    mut operation: F,
) -> dataflow_rs::Result<Value>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = dataflow_rs::Result<Value>>,
{
    let mut last_error = None;

    const MAX_BACKOFF_MS: u64 = 60_000; // cap at 60 seconds

    for attempt in 0..=max_retries {
        if attempt > 0 {
            let delay = retry_delay_ms
                .saturating_mul(1u64.checked_shl(attempt - 1).unwrap_or(u64::MAX))
                .min(MAX_BACKOFF_MS);
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }

        match operation().await {
            Ok(val) => return Ok(val),
            Err(e) => {
                if e.retryable() && attempt < max_retries {
                    tracing::warn!(
                        attempt = attempt + 1,
                        max = max_retries,
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
        if let Some(meta) = message.metadata_mut().as_object_mut() {
            meta.insert("channel".to_string(), Value::String("orders".to_string()));
        }
        assert_eq!(extract_channel(&message), "orders");
    }

    #[test]
    fn test_extract_channel_without_channel() {
        let message = Message::from_value(&serde_json::json!({}));
        assert_eq!(extract_channel(&message), "unknown");
    }

    #[test]
    fn test_resolve_path_static() {
        let datalogic = DataLogic::default();
        let mut message = Message::from_value(&serde_json::json!({}));
        let result =
            resolve_path(&Some("/users".to_string()), &None, &mut message, &datalogic).unwrap();
        assert_eq!(result, Some("/users".to_string()));
    }

    #[test]
    fn test_resolve_path_none() {
        let datalogic = DataLogic::default();
        let mut message = Message::from_value(&serde_json::json!({}));
        let result = resolve_path(&None, &None, &mut message, &datalogic).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_resolve_path_logic_expression() {
        let datalogic = DataLogic::default();
        let mut message = Message::from_value(&serde_json::json!({}));
        // Set data in context so JSONLogic can find it
        *message.data_mut() = serde_json::json!({"path": "/dynamic"});
        message.invalidate_context_cache();
        let logic = serde_json::json!({"var": "data.path"});
        let result = resolve_path(&None, &Some(logic), &mut message, &datalogic).unwrap();
        assert_eq!(result, Some("/dynamic".to_string()));
    }

    #[test]
    fn test_resolve_path_logic_non_string_result() {
        let datalogic = DataLogic::default();
        let mut message = Message::from_value(&serde_json::json!({}));
        *message.data_mut() = serde_json::json!({"id": 42});
        message.invalidate_context_cache();
        let logic = serde_json::json!({"var": "data.id"});
        let result = resolve_path(&None, &Some(logic), &mut message, &datalogic).unwrap();
        assert_eq!(result, Some("42".to_string()));
    }

    #[tokio::test]
    async fn test_retry_with_backoff_succeeds_first_try() {
        let result = retry_with_backoff(3, 1, "test", || async {
            Ok(serde_json::json!({"ok": true}))
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), serde_json::json!({"ok": true}));
    }

    #[tokio::test]
    async fn test_retry_with_backoff_fails_then_succeeds() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();
        let result = retry_with_backoff(3, 1, "test", move || {
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
    async fn test_retry_with_backoff_non_retryable_fails_immediately() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();
        let result = retry_with_backoff(3, 1, "test", move || {
            let c = counter_clone.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(DataflowError::Validation("bad input".to_string()))
            }
        })
        .await;
        assert!(result.is_err());
        // Non-retryable errors should not retry
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_retry_with_backoff_exhausts_retries() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();
        let result = retry_with_backoff(2, 1, "test", move || {
            let c = counter_clone.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(DataflowError::Io("always fails".to_string()))
            }
        })
        .await;
        assert!(result.is_err());
        // 1 initial + 2 retries = 3 total attempts
        assert_eq!(counter.load(Ordering::SeqCst), 3);
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
            0,
            1,
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

        // Trip the circuit breaker
        breaker.record_failure();

        let result = execute_with_circuit_breaker(
            &breaker,
            "test-connector",
            "test-channel",
            0,
            1,
            "test",
            || async { Ok(serde_json::json!({"should": "not reach"})) },
        )
        .await;

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Circuit breaker open")
        );
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
            0,
            1,
            "test",
            || async { Err(DataflowError::Io("network error".to_string())) },
        )
        .await;

        assert!(result.is_err());
        // Breaker should still be closed (threshold is 5)
        assert!(breaker.check());
    }
}
