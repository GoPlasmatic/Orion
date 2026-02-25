pub mod enrich;
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
