pub mod enrich;
pub mod http_call;
pub mod publish_kafka;

use std::future::Future;
use std::time::Duration;

use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::message::Message;
use datalogic_rs::DataLogic;
use serde_json::Value;

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
        Ok(Some(result.as_str().map(|s| s.to_string()).unwrap_or_else(
            || serde_json::to_string(&result).unwrap_or_default(),
        )))
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

    for attempt in 0..=max_retries {
        if attempt > 0 {
            let delay = retry_delay_ms * 2u64.pow(attempt - 1);
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
