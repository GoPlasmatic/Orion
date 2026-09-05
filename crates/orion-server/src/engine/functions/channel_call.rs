use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
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
    /// The target channel (JSONLogic), compiled once at engine build.
    ///
    /// One field, not the `channel` / `channel_logic` pair it used to be.
    /// dataflow-rs 3.9 collapsed exactly this shape on its own configs, for the
    /// reason that applies here too: a static spelling *is* JSONLogic for
    /// itself, folded once at build and free per message, so a second field to
    /// say "this one is an expression" was only ever describing the type of a
    /// value the compiler can see for itself. `channel_logic` stays as a serde
    /// alias, so a workflow written against the pair keeps loading — supplying
    /// both is a duplicate-field error rather than a precedence rule.
    ///
    /// `Option`, and defaulted, rather than required: a task naming no channel
    /// must fail at *call* time with the message below, not at engine
    /// construction, which for Orion is a whole-instance failure that takes
    /// every channel down over one stored row (proposal F23). The authoring-time
    /// refusal is the schema's `required: true`.
    #[serde(default, alias = "channel_logic")]
    pub channel: Option<Template>,
    /// Dotted path where the called channel's response is written. Named
    /// `output` to match the other nine handlers (proposal F43);
    /// `response_path` stays accepted so 0.3.x workflows keep loading — an
    /// *accepted alternate spelling* with no removal date, not a deprecation
    /// (F59): `http_call`'s twin alias lives on dataflow-rs's
    /// `HttpCallConfig`, which Orion cannot retire on its own, and the two
    /// functions must not drift apart. See "accepted alternate spellings" in
    /// `docs/src/reference/support.md`.
    #[serde(default, alias = "response_path")]
    pub output: Option<Template>,
    /// The payload to send (JSONLogic). Omitted, the caller's own payload is
    /// forwarded. `data_logic` stays as a serde alias, as `channel` keeps
    /// `channel_logic`.
    #[serde(default, alias = "data_logic")]
    pub data: Option<Template>,
    /// Per-call timeout (JSONLogic), as `http_call`'s is.
    #[serde(default)]
    pub timeout_ms: Option<Template>,
}

/// Invokes another channel's workflow in-process (no HTTP round-trip).
pub struct ChannelCallHandler {
    /// The node's serving generation, consulted lazily per call — the handler
    /// is registered on an engine that is itself part of a generation, so it
    /// cannot hold one; it holds the handle and loads.
    ///
    /// One load per call gives the target's ingress contract (F14) and the
    /// engine that will run it from the same build. The child may be a
    /// *later* generation than its caller, which is deliberate: a reload
    /// mid-workflow is a config change the child is entitled to see, and both
    /// halves it does see agree with each other.
    pub runtime: Arc<crate::runtime::RuntimeHandle>,
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
        if let Some(t) = input.channel.as_mut() {
            check_channel_shape(t)?;
            t.compile(c, "channel_call.channel")?;
        }
        if let Some(t) = input.data.as_mut() {
            t.compile(c, "channel_call.data")?;
        }
        if let Some(t) = input.timeout_ms.as_mut() {
            t.compile(c, "channel_call.timeout_ms")?;
        }
        if let Some(t) = input.output.as_mut() {
            t.compile(c, "channel_call.output")?;
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
            let call_data: Value = if let Some(ref data) = input.data {
                input_json(data, ctx)?
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
            crate::engine::clear_error_context(&mut child_meta);
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
            //
            // One generation for this call: the target's guards here and the
            // engine that runs it below come from the same build.
            let generation = self.runtime.load();
            let target_runtime = generation
                .channels
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
            // D6: a cron channel is not callable. Its workflow is meant to run
            // once per occurrence, recorded in the ledger and serialised by its
            // singleton key; a `channel_call` would run it with none of that —
            // no occurrence row, and no lock, so a caller could trivially
            // overlap a `forbid` schedule with itself. The refusal is here
            // rather than in the guard matrix because it is about *what the
            // channel is*, not about which guards this transport applies.
            if target_runtime
                .as_ref()
                .is_some_and(|runtime| runtime.cron.is_some())
            {
                return Err(DataflowError::function_execution(
                    format!(
                        "channel_call to '{target_channel}': it is a cron channel, which                          runs only on its own schedule. Call the workflow it names                          directly, or give the shared work a channel of its own."
                    ),
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
                // Authenticated at the edge; this call presents no credential.
                auth_backoff: None,
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
                // The caller is a workflow, not a user agent: there is no
                // browser to redirect and no callback to complete.
                oauth: None,
            })
            .await
            .map_err(|e| guard_refusal(&target_channel, e))?;
            let _backpressure_permit = admission.backpressure_permit;

            // Timeout precedence: explicit input > target channel's
            // timeout_ms > engine default (the last two resolved by the
            // guard chain, so every transport agrees on them).
            let task_timeout_ms = match input.timeout_ms.as_ref() {
                Some(t) => Some(t.resolve_u64(ctx, "channel_call 'timeout_ms'")?),
                None => None,
            };
            let timeout_ms = task_timeout_ms
                .or(admission.timeout_ms)
                .unwrap_or(self.default_timeout_ms);

            // F46: the shared post-admission step owns the deadline arm and
            // the message build, so the in-process call cannot drift from the
            // sync HTTP, trace-queue and Kafka paths. No trace capture and no
            // profile scope — this call already runs inside the caller's. No
            // routing bucket either: the child executes within its caller's
            // rollout decision rather than drawing one of its own.
            let child = crate::engine::execute_admitted(
                &generation.engine,
                &target_channel,
                &call_data,
                &child_meta,
                crate::engine::ExecOpts {
                    timeout_ms: Some(timeout_ms),
                    ..Default::default()
                },
            )
            .await;

            match child.outcome {
                crate::engine::RunOutcome::Ok => {}
                // This arm did not exist before the step was shared: the
                // handler read only the outer `Result`, so a target whose
                // workflow failed its tasks reported success and the caller
                // merged whatever half-finished `data` it left behind.
                crate::engine::RunOutcome::WorkflowErrors(summary) => {
                    return Err(DataflowError::function_execution(
                        format!("channel_call to '{target_channel}' failed: {summary}"),
                        None,
                    ));
                }
                crate::engine::RunOutcome::EngineError(e) => {
                    return Err(DataflowError::function_execution(
                        format!("channel_call to '{target_channel}' failed: {e}"),
                        None,
                    ));
                }
                crate::engine::RunOutcome::Timeout(ms) => {
                    return Err(DataflowError::Timeout(format!(
                        "channel_call to '{target_channel}' timed out after {ms}ms"
                    )));
                }
            }

            // Strip internal tracking metadata from the child's result before merging.
            // The bridge from OwnedDataValue to serde_json::Value is the easiest way
            // to filter; we then convert the parts we care about back.
            let result_data_json: Value = child.message.data().into();

            let output = match input.output.as_ref() {
                Some(t) => t.resolve_string(ctx)?,
                None => "data".to_string(),
            };
            ctx.set_json(&output, &result_data_json);

            Ok(TaskOutcome::Success)
        })
        .await
    }
}

/// Resolve the target channel name.
pub(super) fn resolve_target(
    ctx: &TaskContext<'_>,
    input: &ChannelCallInput,
) -> dataflow_rs::Result<String> {
    let Some(channel) = input.channel.as_ref() else {
        return Err(DataflowError::Validation(
            "channel_call requires 'channel'".into(),
        ));
    };
    // Deliberately not `resolve_string`: a non-string result here is an
    // authoring mistake worth reporting, not something to coerce into a channel
    // name that cannot exist. A statically authored name is a constant on the
    // compiled template, so this costs no evaluation for the ordinary case.
    let result: Value = input_json(channel, ctx)?;
    let target = result.as_str().ok_or_else(|| {
        DataflowError::Validation("channel_call 'channel' must evaluate to a string".to_string())
    })?;

    if target.is_empty() {
        return Err(DataflowError::Validation(
            "channel_call: target channel name must not be empty".into(),
        ));
    }
    Ok(target.to_string())
}

/// Refuse a `channel` that cannot name a channel whatever the message says.
///
/// While `channel` was a `String`, serde caught `"channel": 7` when the input
/// was parsed, and the load-time screen quarantined that one row rather than
/// letting it serve — which is the whole of F33/F41. A `Template` accepts any
/// JSON, so that check has to be made here or the row loads clean and fails on
/// every request instead.
///
/// The line is the authored shape: a non-string *scalar* is unambiguously
/// itself in JSONLogic, so it is a channel name that is not a string and never
/// will be. An object or an array may be an operator call, and what it
/// evaluates to is not knowable until a message arrives — `resolve_target`
/// reports those.
fn check_channel_shape(channel: &Template) -> dataflow_rs::Result<()> {
    match channel.as_json() {
        Value::Object(_) | Value::Array(_) | Value::String(_) => Ok(()),
        other => Err(DataflowError::Validation(format!(
            "channel_call 'channel' must be a channel name or an expression \
             producing one, not {other}"
        ))),
    }
}

/// A template's value for this message as `serde_json::Value`, through
/// `resolve` so a folded constant is served from the cache rather than
/// re-evaluated.
fn input_json(template: &Template, ctx: &TaskContext<'_>) -> dataflow_rs::Result<Value> {
    Ok((&template.resolve(ctx)?).into())
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
        description: "Target channel to invoke (JSONLogic), so one task can route by \
                      message content. (Was `channel_logic`; still accepted, but not \
                      alongside `channel`.)",
        kind: FieldKind::String,
        required: true,
        template_at: &[""],
        alias: Some("channel_logic"),
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "data",
        description: "Payload to pass to the target channel (JSONLogic). Omit to forward \
                      the caller's own payload. (Was `data_logic`; still accepted, but not \
                      alongside `data`.)",
        kind: FieldKind::Any,
        template_at: &[""],
        alias: Some("data_logic"),
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where the called channel's response is stored. Defaults to \"data\". (Was `response_path` before 1.0; still accepted.)",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "timeout_ms",
        description: "Per-call timeout in milliseconds.",
        kind: FieldKind::Number,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];
