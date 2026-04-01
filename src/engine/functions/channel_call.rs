use std::sync::Arc;

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
}

/// Invokes another channel's workflow in-process (no HTTP round-trip).
pub struct ChannelCallHandler {
    pub engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
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

        // Get current engine and process
        let engine = self.engine.read().await.clone();
        engine
            .process_message_for_channel(&target_channel, &mut child_message)
            .await
            .map_err(|e| {
                DataflowError::function_execution(
                    format!("channel_call to '{}' failed: {}", target_channel, e),
                    None,
                )
            })?;

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
