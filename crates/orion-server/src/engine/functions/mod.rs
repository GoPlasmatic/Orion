pub mod channel_call;
pub mod connector_helpers;
pub mod http_call;
pub mod http_common;
pub mod publish_kafka;
pub mod schema;
pub mod secret_ref;
pub mod stub;

pub mod cache_read;
pub mod cache_write;
pub mod crypto;
pub mod data_query;
pub mod data_write;
pub mod db_read;
pub mod db_write;
pub mod jwt_sign;
pub mod jwt_verify;
pub mod mongo_aggregate;
pub mod mongo_common;
pub mod mongo_read;
pub mod mongo_write;
pub mod send_email;
pub mod storage_head;
pub mod storage_presign;

use dataflow_rs::engine::message::Message;
// Only the `#[cfg(test)]` harness below needs this now that the retry loop
// itself lives upstream.
#[cfg(test)]
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

/// The retry loop `http_call` runs, re-exported from dataflow-rs.
///
/// This was Orion's own until dataflow-rs 3.7, which absorbed it: same three
/// policy fields, same exponential backoff under a 60s per-sleep ceiling, same
/// whole-loop deadline with a backoff that would cross it skipped rather than
/// slept, and the same `tokio::time::Instant` so a paused test clock stays
/// coherent with the sleeps. The crate has always classified errors by
/// [`DataflowError::retryable`]; it now ships the loop that acts on it, and
/// upstream is where that pair belongs — the classification and the mechanism
/// reading it are one decision.
///
/// [`retry_with_attempts`] is the same loop reporting how many attempts it
/// made, which is the only way a caller can say "this succeeded, but not
/// first time".
///
/// [`DataflowError::retryable`]: dataflow_rs::DataflowError::retryable
pub use dataflow_rs::{RetryPolicy, retry_with_attempts, retry_with_policy};

/// Test-only shared harness: run `tasks` through a real engine with `fns`
/// registered, seed `data` (unless null) into the message context, and return
/// the message's final `data` — or the first task error, formatted. The
/// per-handler test modules each used to hand-roll these ~25 lines.
#[cfg(test)]
pub(crate) async fn run_test_tasks(
    fns: std::collections::HashMap<String, dataflow_rs::BoxedFunctionHandler>,
    tasks: Value,
    data: Value,
) -> Result<Value, String> {
    let workflow: dataflow_rs::Workflow = serde_json::from_value(serde_json::json!({
        "id": "w", "name": "w", "condition": true, "tasks": tasks
    }))
    .map_err(|e| e.to_string())?;
    let engine = dataflow_rs::Engine::new(vec![workflow], fns).map_err(|e| e.to_string())?;
    let mut message = dataflow_rs::Message::from_value(&serde_json::json!({}));
    if !data.is_null() {
        dataflow_rs::engine::utils::set_nested_value(
            &mut message.context,
            "data",
            dataflow_rs::datavalue::OwnedDataValue::from(&data),
        );
    }
    engine
        .process_message(&mut message)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(err) = message.errors().first() {
        return Err(format!("{err:?}"));
    }
    Ok(message.data().into())
}

/// One-task convenience over [`run_test_tasks`].
#[cfg(test)]
pub(crate) async fn run_test_task(
    name: &str,
    handler: dataflow_rs::BoxedFunctionHandler,
    input: Value,
    data: Value,
) -> Result<Value, String> {
    let mut fns: std::collections::HashMap<String, dataflow_rs::BoxedFunctionHandler> =
        Default::default();
    fns.insert(name.to_string(), handler);
    run_test_tasks(
        fns,
        serde_json::json!([{"id": "t", "name": "t", "function": {"name": name, "input": input}}]),
        data,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use dataflow_rs::engine::error::DataflowError;
    use std::sync::Arc;
    use std::time::Duration;

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
