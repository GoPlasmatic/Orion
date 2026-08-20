use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::message::Message;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use dataflow_rs::{Template, TemplateCompiler};
use serde::Deserialize;
use serde_json::Value;

use super::schema::{FieldKind, FieldSchema};

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
    /// JSONLogic naming the target channel, compiled once at engine build.
    ///
    /// A [`Template`] rather than a raw `Value`: this and `data_logic` were the
    /// only two `ctx.datalogic().compile(..)` calls left in the handler surface,
    /// so `channel_call` re-parsed and re-compiled both expressions **on every
    /// message** while `http_call` and `publish_kafka` got an `Arc<Logic>` for
    /// free from dataflow-rs's own typed configs. `Template` closes that
    /// asymmetry and evaluates on the worker's pooled arena instead of
    /// constructing a fresh one per call.
    #[serde(default)]
    pub channel_logic: Option<Template>,
    /// Dotted path where the called channel's response is written. Named
    /// `output` to match the other nine handlers (proposal F43);
    /// `response_path` stays accepted so 0.3.x workflows keep loading — an
    /// *accepted alternate spelling* with no removal date, not a deprecation
    /// (F59): `http_call`'s twin alias lives on dataflow-rs's
    /// `HttpCallConfig`, which Orion cannot retire on its own, and the two
    /// functions must not drift apart. See "accepted alternate spellings" in
    /// `docs/src/reference/support.md`.
    #[serde(default, alias = "response_path")]
    pub output: Option<String>,
    #[serde(default)]
    pub data: Option<Value>,
    /// JSONLogic producing the payload to send. Same `Template` treatment as
    /// [`ChannelCallInput::channel_logic`].
    #[serde(default)]
    pub data_logic: Option<Template>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Invokes another channel's workflow in-process (no HTTP round-trip).
pub struct ChannelCallHandler {
    pub engine: Arc<crate::engine::EngineHandle>,
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

    /// Compile both JSONLogic fields once, at engine construction.
    ///
    /// This moves a malformed expression from a per-message failure to a build
    /// failure — which for Orion is a *whole-instance* failure: boot aborts, or
    /// every channel on every node goes down on reload. `custom_input_parse_check`
    /// compiles the same two fields ahead of `Engine::new` so a bad expression
    /// stays a per-channel `ChannelLoadIssue` (F33/F41) instead.
    fn compile_input(input: &mut Self::Input, c: &TemplateCompiler) -> dataflow_rs::Result<()> {
        if let Some(t) = input.channel_logic.as_mut() {
            t.compile(c, "channel_call.channel_logic")?;
        }
        if let Some(t) = input.data_logic.as_mut() {
            t.compile(c, "channel_call.data_logic")?;
        }
        Ok(())
    }

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &ChannelCallInput,
    ) -> dataflow_rs::Result<TaskOutcome> {
        // F49: resolve the target *before* opening the profile scope, so the
        // sample can be labelled with it. `channel_call` used to pass `None`,
        // leaving `by_connector` blank for the one handler whose fan-out most
        // needs attribution — a workflow calling three channels showed three
        // unattributed `channel_call` entries and no way to tell which one was
        // slow. Resolution is pure (a JSONLogic eval over the message) and
        // takes `ctx` immutably, so it sits outside the body that borrows it
        // mutably anyway.
        let target_channel = resolve_target(ctx, input)?;
        let label = target_channel.clone();

        crate::engine::profile::record("channel_call", Some(&label), async move {
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
                logic.eval_into(ctx)?
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
            // #280: the child inherits the parent's metadata wholesale, so
            // without this it would start life carrying — and branching on —
            // the *parent's* failed tasks as if they were its own. Its own
            // failures are recorded by the engine as they happen.
            if let Some(map) = child_meta.as_object_mut() {
                map.remove(crate::engine::ERROR_CONTEXT_KEY);
            }
            // The calling channel, read before the override below replaces
            // it: it is this call's caller identity, so a target's rate limit
            // buckets per calling channel rather than lumping every
            // in-process caller together (N16).
            let calling_channel = child_meta
                .get("channel")
                .and_then(Value::as_str)
                .unwrap_or("channel_call")
                .to_string();
            // F4: the child runs as the target channel — override the
            // parent's "channel" so connector metrics and circuit-breaker
            // keys attribute to the channel actually executing.
            child_meta["channel"] = Value::String(target_channel.clone());
            child_meta[META_CALL_DEPTH] = serde_json::json!(child_depth);
            child_meta[META_CALL_CHAIN] = serde_json::json!(child_chain);

            // F14/N16: enforce the target channel's ingress contract for
            // in-process calls. Which guards that means is
            // `Transport::ChannelCall`'s row of the matrix, not a decision
            // taken here.
            // F35: refuse a quarantined target rather than calling it with
            // none of its guards.
            let target_runtime = self
                .channel_registry
                .require_serviceable(&target_channel)
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
            // "work" with `output` never populated).
            if target_runtime.is_none() {
                return Err(DataflowError::function_execution(
                    format!("channel_call to '{target_channel}': channel not found or not active"),
                    None,
                ));
            }
            // The header view is the metadata inherited from the originating
            // request, so a target whose `rate_limit.key_logic` reads a
            // header still resolves one on this path. Credential headers
            // arrive masked (S10), which narrows those buckets rather than
            // widening them.
            let header_lookup = |name: &str| {
                child_meta
                    .get("headers")
                    .and_then(|h| h.get(name))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            };
            let admission = crate::channel::guards::admit(crate::channel::guards::GuardRequest {
                transport: crate::channel::guards::Transport::ChannelCall,
                channel: &target_channel,
                runtime: &target_runtime,
                data: &call_data,
                metadata: &child_meta,
                datalogic: ctx.datalogic(),
                origin: None,
                caller_identity: &calling_channel,
                header: &header_lookup,
                // An in-process call presents no credential and signs no body; its
                // ingress authenticated at the edge (see `Transport::guards`).
                raw_body: None,
                dedup_key_fallback: None,
                // `Transport::ChannelCall` does not deduplicate, so no
                // claim is taken and the owner is moot.
                dedup_owner: None,
                default_timeout_ms: Some(self.default_timeout_ms),
                // `engine.default_channel_call_timeout_ms` is a default,
                // not a ceiling: an in-process call blocks only its own
                // caller, and the task's explicit `timeout_ms` outranks
                // both anyway.
                max_timeout_ms: None,
            })
            .await
            .map_err(|e| guard_refusal(&target_channel, e))?;
            let _backpressure_permit = admission.backpressure_permit;

            // Build a child message for the target channel.
            let mut child_message = Message::builder()
                .payload_json(&call_data)
                .metadata_json(&child_meta)
                .build();

            // Get current engine snapshot and process with timeout.
            // Timeout precedence: explicit input > target channel's
            // timeout_ms > engine default (the last two resolved by the
            // guard chain, so every transport agrees on them).
            let engine = self.engine.load();
            let timeout_ms = input
                .timeout_ms
                .or(admission.timeout_ms)
                .unwrap_or(self.default_timeout_ms);

            // F46: the shared runner owns the deadline arm, so the in-process
            // call cannot drift from the sync HTTP, trace-queue and Kafka
            // paths. No trace capture, and no profile scope — this call already
            // runs inside the caller's.
            match crate::engine::run_for_channel(
                &engine,
                &target_channel,
                &mut child_message,
                Some(timeout_ms),
                None,
                None,
            )
            .await
            {
                Ok((inner, _)) => inner.map_err(|e| {
                    DataflowError::function_execution(
                        format!("channel_call to '{target_channel}' failed: {e}"),
                        None,
                    )
                })?,
                Err(ms) => {
                    return Err(DataflowError::Timeout(format!(
                        "channel_call to '{target_channel}' timed out after {ms}ms"
                    )));
                }
            }

            // Strip internal tracking metadata from the child's result before merging.
            // The bridge from OwnedDataValue to serde_json::Value is the easiest way
            // to filter; we then convert the parts we care about back.
            let result_data_json: Value = child_message.data().into();

            let output = input.output.as_deref().unwrap_or("data");
            ctx.set_json(output, &result_data_json);

            Ok(TaskOutcome::Success)
        })
        .await
    }
}

/// Resolve the target channel name, static or dynamic via JSONLogic.
fn resolve_target(ctx: &TaskContext<'_>, input: &ChannelCallInput) -> dataflow_rs::Result<String> {
    let target = if let Some(ref logic) = input.channel_logic {
        // Deliberately not `eval_to_plain_string`: a non-string result here is
        // an authoring mistake worth reporting, not something to coerce into a
        // channel name that cannot exist.
        let result: Value = logic.eval_into(ctx)?;
        result.as_str().map(|s| s.to_string()).ok_or_else(|| {
            DataflowError::Validation("channel_logic must evaluate to a string".to_string())
        })?
    } else {
        input.channel.clone()
    };

    if target.is_empty() {
        return Err(DataflowError::Validation(
            "channel_call: target channel name must not be empty".into(),
        ));
    }
    Ok(target)
}

/// Map a target channel's guard refusal onto the error the calling workflow
/// sees.
///
/// A `validation_logic` rejection stays a `Validation` — the caller sent data
/// the target refuses, which the envelope reports as `400`. Every other
/// refusal (over the target's rate limit, at its concurrency cap, a
/// fail-closed dedup/rate-limit backend) keeps the status the guard chose,
/// so the caller sees `429`/`503` rather than the generic `500` a plain
/// function-execution error would have produced for a condition that is
/// nobody's bug and is worth retrying.
fn guard_refusal(target: &str, e: crate::errors::OrionError) -> DataflowError {
    let (status, _code, detail) = e.response_parts();
    let message = format!("channel_call to '{target}': {detail}");
    if status == axum::http::StatusCode::BAD_REQUEST {
        DataflowError::Validation(message)
    } else {
        crate::errors::channel_refused_dataflow_error(status, message)
    }
}

/// Format a call chain for error messages: "A -> B -> C"
fn format_chain(chain: &[String], target: &str) -> String {
    let mut parts: Vec<&str> = chain.iter().map(|s| s.as_str()).collect();
    parts.push(target);
    parts.join(" -> ")
}

// -- Input schema (F53) --
//
// The table describing this handler's `function.input` lives next to the
// handler it describes. It used to sit in `schema.rs` with the other nine,
// which is how every schema/handler divergence in the 1.0 audit happened:
// a field was added, renamed or made conditional here and the table saying
// so was in a different file.

pub(super) const CHANNEL_CALL_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "channel",
        description: "Target channel name to invoke. Mutually exclusive with channel_logic.",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "channel_logic",
        description: "JSONLogic expression evaluating to the target channel name.",
        kind: FieldKind::Any,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "data",
        description: "Static payload to pass to the target channel.",
        kind: FieldKind::Any,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "data_logic",
        description: "JSONLogic expression evaluating to the payload to pass.",
        kind: FieldKind::Any,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where the called channel's response is stored. Defaults to \"data\". (Was `response_path` before 1.0; still accepted.)",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "timeout_ms",
        description: "Per-call timeout in milliseconds.",
        kind: FieldKind::Number,
        required: false,
        resolvable: false,
        alias: None,
    },
];
