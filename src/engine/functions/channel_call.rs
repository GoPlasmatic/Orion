use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::message::Message;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use dataflow_rs::engine::utils::set_nested_value;
use datavalue::OwnedDataValue;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::RwLock;

/// Metadata key prefix for internal Orion tracking fields.
const ORION_META_PREFIX: &str = "_orion_";
/// Metadata key for current call depth.
const META_CALL_DEPTH: &str = "_orion_call_depth";
/// Metadata key for the call chain (array of channel names).
const META_CALL_CHAIN: &str = "_orion_call_chain";

/// Input configuration for the channel_call function.
#[derive(Debug, Deserialize)]
pub struct ChannelCallInput {
    pub channel: String,
    #[serde(default)]
    pub channel_logic: Option<Value>,
    #[serde(default)]
    pub response_path: Option<String>,
    #[serde(default)]
    pub data: Option<Value>,
    #[serde(default)]
    pub data_logic: Option<Value>,
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
    type Input = ChannelCallInput;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &ChannelCallInput,
    ) -> dataflow_rs::Result<TaskOutcome> {
        // Resolve target channel name (static or dynamic via JSONLogic).
        let target_channel = if let Some(ref logic) = input.channel_logic {
            let compiled = ctx
                .datalogic()
                .compile(logic)
                .map_err(|e| DataflowError::LogicEvaluation(e.to_string()))?;
            let result: Value = ctx
                .datalogic()
                .session()
                .eval_into(&compiled, &ctx.message().context)
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
        let parent_depth = ctx
            .message()
            .metadata()
            .get(META_CALL_DEPTH)
            .and_then(|v| v.as_i64())
            .map(|n| n as u64)
            .unwrap_or(0);

        let parent_chain: Vec<String> = ctx
            .message()
            .metadata()
            .get(META_CALL_CHAIN)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        if parent_depth >= self.max_call_depth as u64 {
            return Err(DataflowError::Validation(format!(
                "channel_call: max call depth {} exceeded (chain: {})",
                self.max_call_depth,
                format_chain(&parent_chain, &target_channel),
            )));
        }

        if parent_chain.contains(&target_channel) {
            return Err(DataflowError::Validation(format!(
                "channel_call: cycle detected: {}",
                format_chain(&parent_chain, &target_channel),
            )));
        }

        // Resolve data to send.
        let call_data: Value = if let Some(ref logic) = input.data_logic {
            let compiled = ctx
                .datalogic()
                .compile(logic)
                .map_err(|e| DataflowError::LogicEvaluation(e.to_string()))?;
            ctx.datalogic()
                .session()
                .eval_into(&compiled, &ctx.message().context)
                .map_err(|e| DataflowError::LogicEvaluation(e.to_string()))?
        } else if let Some(ref data) = input.data {
            data.clone()
        } else {
            // Forward the original payload (not context.data which may be empty).
            // Bridge OwnedDataValue → serde_json::Value once.
            (&*ctx.message().payload_arc().clone()).into()
        };

        // Build a child message for the target channel.
        let mut child_message = Message::from_value(&call_data);

        // Propagate metadata from parent (overriding "channel" key with the new target).
        if let Some(parent_meta) = ctx.message().metadata().as_object() {
            for (k, v) in parent_meta {
                if k == "channel" {
                    continue;
                }
                set_nested_value(
                    &mut child_message.context,
                    &format!("metadata.{k}"),
                    v.clone(),
                );
            }
        }

        // Set tracking metadata on child (after propagation so we override parent values).
        let child_depth = parent_depth + 1;
        let mut child_chain = parent_chain;
        child_chain.push(target_channel.clone());

        set_nested_value(
            &mut child_message.context,
            &format!("metadata.{META_CALL_DEPTH}"),
            OwnedDataValue::from_i64(child_depth as i64),
        );
        let chain_value: Value = serde_json::Value::Array(
            child_chain
                .iter()
                .map(|s| Value::String(s.clone()))
                .collect(),
        );
        set_nested_value(
            &mut child_message.context,
            &format!("metadata.{META_CALL_CHAIN}"),
            OwnedDataValue::from(&chain_value),
        );

        // Get current engine snapshot and process with timeout.
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
                    format!("channel_call to '{target_channel}' failed: {e}"),
                    None,
                )
            })?,
            Err(_) => {
                return Err(DataflowError::Timeout(format!(
                    "channel_call to '{target_channel}' timed out after {}ms",
                    timeout.as_millis()
                )));
            }
        }

        // Strip internal tracking metadata from the child's result before merging.
        // The bridge from OwnedDataValue to serde_json::Value is the easiest way
        // to filter; we then convert the parts we care about back.
        let result_data_json: Value = child_message.data().into();

        if let Some(ref response_path) = input.response_path {
            ctx.set_json(response_path, &result_data_json);
        } else {
            ctx.set_json("data", &result_data_json);
        }

        Ok(TaskOutcome::Success)
    }
}

/// Format a call chain for error messages: "A -> B -> C"
fn format_chain(chain: &[String], target: &str) -> String {
    let mut parts: Vec<&str> = chain.iter().map(|s| s.as_str()).collect();
    parts.push(target);
    parts.join(" -> ")
}

// Silence the unused-constant warning while we don't yet strip parent metadata in v3.
#[allow(dead_code)]
const _: &str = ORION_META_PREFIX;
