use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::message::Message;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::RwLock;

/// Metadata key for current call depth.
const META_CALL_DEPTH: &str = "_orion_call_depth";
/// Metadata key for the call chain (array of channel names).
const META_CALL_CHAIN: &str = "_orion_call_chain";

/// Input configuration for the channel_call function.
#[derive(Debug, Deserialize)]
pub struct ChannelCallInput {
    /// Static target channel. Defaulted rather than required because the schema
    /// (and the docs) declare it optional when `channel_logic` is given — a
    /// `channel_logic`-only task passed admin validation and then failed engine
    /// construction with `missing field 'channel'`, taking every channel down
    /// with it (proposal F23). The empty default is rejected at call time by the
    /// existing target check below.
    #[serde(default)]
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
    /// Registry lookup for the target channel's ingress contract (F14).
    /// Held as the shared Arc and consulted lazily per call, since the
    /// registry is populated by reload after the engine is built.
    pub channel_registry: Arc<crate::channel::ChannelRegistry>,
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
        crate::engine::profile::record("channel_call", None, async move {
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

            // Build the child metadata as JSON once: parent metadata (minus
            // the parent's "channel" key) plus call-tracking keys. Used both
            // as the validation_logic context and as the metadata merged into
            // the child message, so validation sees what the workflow sees.
            let child_depth = parent_depth + 1;
            let mut child_chain = parent_chain;
            child_chain.push(target_channel.clone());

            let mut child_meta: Value = ctx.message().metadata().into();
            if !child_meta.is_object() {
                child_meta = serde_json::json!({});
            }
            // F4: the child runs as the target channel — override the
            // parent's "channel" so connector metrics and circuit-breaker
            // keys attribute to the channel actually executing.
            child_meta["channel"] = Value::String(target_channel.clone());
            child_meta[META_CALL_DEPTH] = serde_json::json!(child_depth);
            child_meta[META_CALL_CHAIN] = serde_json::json!(child_chain);

            // F14: enforce the target channel's ingress contract for
            // in-process calls — validation_logic, backpressure, and
            // timeout_ms. CORS, dedup, and the response cache are
            // HTTP-transport concerns and don't apply here.
            // F35: refuse a quarantined target rather than calling it with
            // none of its guards.
            let target_runtime = self
                .channel_registry
                .require_serviceable(&target_channel)
                .await
                .map_err(|e| {
                    DataflowError::function_execution(
                        format!("channel_call to '{target_channel}': {e}"),
                        None,
                    )
                })?;
            // A target absent from the registry is not an active channel.
            // Refuse loudly: `process_message_for_channel` on an unknown
            // channel matches zero workflows and reports success, which
            // would silently drop the call (a typo'd channel name would
            // "work" with response_path never populated).
            if target_runtime.is_none() {
                return Err(DataflowError::function_execution(
                    format!("channel_call to '{target_channel}': channel not found or not active"),
                    None,
                ));
            }
            crate::channel::guards::validate_input(
                &target_channel,
                &target_runtime,
                &call_data,
                &child_meta,
                ctx.datalogic(),
            )
            .map_err(|e| {
                DataflowError::Validation(format!("channel_call to '{target_channel}': {e}"))
            })?;
            let _backpressure_permit =
                crate::channel::guards::acquire_backpressure(&target_channel, &target_runtime)
                    .map_err(|e| {
                        DataflowError::function_execution(
                            format!("channel_call to '{target_channel}': {e}"),
                            None,
                        )
                    })?;

            // Build a child message for the target channel.
            let mut child_message = Message::from_value(&call_data);
            crate::engine::utils::merge_metadata(&mut child_message, &child_meta);

            // Get current engine snapshot and process with timeout.
            // Timeout precedence: explicit input > target channel's
            // timeout_ms > engine default.
            let engine = crate::engine::acquire_engine_read(&self.engine).await;
            let timeout_ms = input
                .timeout_ms
                .or_else(|| {
                    target_runtime
                        .as_ref()
                        .and_then(|c| c.parsed_config.timeout_ms)
                })
                .unwrap_or(self.default_timeout_ms);
            let timeout = Duration::from_millis(timeout_ms);

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
        })
        .await
    }
}

/// Format a call chain for error messages: "A -> B -> C"
fn format_chain(chain: &[String], target: &str) -> String {
    let mut parts: Vec<&str> = chain.iter().map(|s| s.as_str()).collect();
    parts.push(target);
    parts.join(" -> ")
}
