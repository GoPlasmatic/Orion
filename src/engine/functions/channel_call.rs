use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::functions::config::FunctionConfig;
use dataflow_rs::engine::message::{Change, Message};
use datalogic_rs::DataLogic;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::RwLock;

use super::http_common::{get_nested, set_nested};

/// Metadata key prefix for internal Orion tracking fields.
const ORION_META_PREFIX: &str = "_orion_";
/// Metadata key for current call depth.
const META_CALL_DEPTH: &str = "_orion_call_depth";
/// Metadata key for the call chain (array of channel names).
const META_CALL_CHAIN: &str = "_orion_call_chain";

/// Input configuration for the channel_call function.
#[derive(Debug, Deserialize)]
pub struct ChannelCallInput {
    /// Target channel name to invoke.
    pub channel: String,
    /// Optional JSONLogic expression to resolve the channel name dynamically.
    #[serde(default)]
    pub channel_logic: Option<Value>,
    /// Optional path to store the response data in the message context.
    #[serde(default)]
    pub response_path: Option<String>,
    /// Optional data override — if absent, forwards the current message data.
    #[serde(default)]
    pub data: Option<Value>,
    /// Optional JSONLogic expression to compute data to send.
    #[serde(default)]
    pub data_logic: Option<Value>,
    /// Optional timeout in milliseconds for this specific channel_call invocation.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Invokes another channel's workflow in-process (no HTTP round-trip).
pub struct ChannelCallHandler {
    pub engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    pub max_call_depth: u32,
    pub default_timeout_ms: u64,
}

#[async_trait]
impl AsyncFunctionHandler for ChannelCallHandler {
    async fn execute(
        &self,
        message: &mut Message,
        config: &FunctionConfig,
        datalogic: Arc<DataLogic>,
    ) -> dataflow_rs::Result<(usize, Vec<Change>)> {
        let input_value = match config {
            FunctionConfig::Custom { input, .. } => input,
            _ => {
                return Err(DataflowError::Validation(
                    "Expected Custom config for channel_call".into(),
                ));
            }
        };

        let input: ChannelCallInput = serde_json::from_value(input_value.clone())
            .map_err(|e| DataflowError::Validation(format!("Invalid channel_call config: {e}")))?;

        // Resolve target channel name (static or dynamic via JSONLogic)
        let target_channel = if let Some(ref logic) = input.channel_logic {
            let context = message.get_context_arc();
            let compiled = datalogic
                .compile(logic)
                .map_err(|e| DataflowError::LogicEvaluation(e.to_string()))?;
            let result = datalogic
                .evaluate(&compiled, context)
                .map_err(|e| DataflowError::LogicEvaluation(e.to_string()))?;
            result.as_str().map(|s| s.to_string()).ok_or_else(|| {
                DataflowError::Validation("channel_logic must evaluate to a string".to_string())
            })?
        } else {
            input.channel.clone()
        };

        if target_channel.is_empty() {
            return Err(DataflowError::Validation(
                "channel_call: target channel name must not be empty".into(),
            ));
        }

        // --- Cycle detection and depth tracking ---
        let parent_depth = message
            .metadata()
            .get(META_CALL_DEPTH)
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let parent_chain: Vec<String> = message
            .metadata()
            .get(META_CALL_CHAIN)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        // Check depth limit
        if parent_depth >= self.max_call_depth as u64 {
            return Err(DataflowError::Validation(format!(
                "channel_call: max call depth {} exceeded (chain: {})",
                self.max_call_depth,
                format_chain(&parent_chain, &target_channel),
            )));
        }

        // Check for cycle
        if parent_chain.contains(&target_channel) {
            return Err(DataflowError::Validation(format!(
                "channel_call: cycle detected: {}",
                format_chain(&parent_chain, &target_channel),
            )));
        }

        // Resolve data to send
        let call_data = if let Some(ref logic) = input.data_logic {
            let context = message.get_context_arc();
            let compiled = datalogic
                .compile(logic)
                .map_err(|e| DataflowError::LogicEvaluation(e.to_string()))?;
            datalogic
                .evaluate(&compiled, context)
                .map_err(|e| DataflowError::LogicEvaluation(e.to_string()))?
        } else if let Some(ref data) = input.data {
            data.clone()
        } else {
            // Forward the original payload (not context.data which may be empty)
            (*message.payload).clone()
        };

        // Build a child message for the target channel.
        // Data goes into payload — the target workflow should use parse_json
        // (or another parse_* function) to load it into context.data.
        let mut child_message = Message::from_value(&call_data);

        // Propagate metadata from parent, overriding channel
        if let Some(parent_meta) = message.metadata().as_object()
            && let Some(child_meta) = child_message.metadata_mut().as_object_mut()
        {
            for (k, v) in parent_meta {
                if k != "channel" {
                    child_meta.insert(k.clone(), v.clone());
                }
            }
        }

        // Set tracking metadata on child (after propagation so we override parent values)
        let child_depth = parent_depth + 1;
        let mut child_chain = parent_chain;
        child_chain.push(target_channel.clone());

        child_message.metadata_mut()[META_CALL_DEPTH] = serde_json::json!(child_depth);
        child_message.metadata_mut()[META_CALL_CHAIN] = serde_json::json!(child_chain);

        // Get current engine and process with timeout
        let engine = crate::engine::acquire_engine_read(&self.engine).await;
        let timeout = Duration::from_millis(input.timeout_ms.unwrap_or(self.default_timeout_ms));

        let process_result = tokio::time::timeout(
            timeout,
            engine.process_message_for_channel(&target_channel, &mut child_message),
        )
        .await;

        match process_result {
            Ok(inner) => inner.map_err(|e| {
                DataflowError::function_execution(
                    format!("channel_call to '{}' failed: {}", target_channel, e),
                    None,
                )
            })?,
            Err(_) => {
                return Err(DataflowError::Timeout(format!(
                    "channel_call to '{}' timed out after {}ms",
                    target_channel,
                    timeout.as_millis()
                )));
            }
        }

        // Strip internal tracking metadata from child result
        if let Some(meta) = child_message.metadata_mut().as_object_mut() {
            meta.retain(|k, _| !k.starts_with(ORION_META_PREFIX));
        }

        // Store result at response_path (or merge back into parent).
        let mut changes = Vec::new();
        let result_data = child_message.data().clone();

        if let Some(ref response_path) = input.response_path {
            let old_value = get_nested(&message.context, response_path);
            set_nested(&mut message.context, response_path, result_data.clone());
            message.invalidate_context_cache();

            changes.push(Change {
                path: Arc::from(response_path.as_str()),
                old_value: Arc::new(old_value),
                new_value: Arc::new(result_data),
            });
        } else {
            // No response_path — merge result into message data
            let old_value = message.data().clone();
            *message.data_mut() = result_data.clone();
            message.invalidate_context_cache();

            changes.push(Change {
                path: Arc::from("data"),
                old_value: Arc::new(old_value),
                new_value: Arc::new(result_data),
            });
        }

        Ok((200, changes))
    }
}

/// Format a call chain for error messages: "A -> B -> C"
fn format_chain(chain: &[String], target: &str) -> String {
    let mut parts: Vec<&str> = chain.iter().map(|s| s.as_str()).collect();
    parts.push(target);
    parts.join(" -> ")
}
