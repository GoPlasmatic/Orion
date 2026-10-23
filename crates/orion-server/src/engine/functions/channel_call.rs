//! `channel_call` — run another channel's workflows in-process, under that
//! channel's ingress guards, and write what they left in `data`.
//!
//! Two implementations, one set of refusals: [`ChannelCallHandler`] serves
//! on dataflow-rs 3, and [`ChannelCallFunction`] is the dataflow-rs 4
//! function, which passes the run's deadline and cancellation on to the
//! child it starts (R60) and a stopped child's error on with its code
//! (I184). Both read the call position, judge the target and word every
//! refusal through the helpers below, so the two cannot refuse differently.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use dataflow_rs::Path;
use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{CompileCx, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec};
use dataflow_rs::program::IssueCode;
use dataflow_rs::run::{AuditMode, CancelToken, Instant, RunStatus};
use dataflow_rs::{Ctx, Expr, Function, Message, Outcome, RunOptions};
use dataflow_rs_v3::engine::error::DataflowError;
use dataflow_rs_v3::engine::functions::AsyncFunctionHandler;
use dataflow_rs_v3::engine::task_context::TaskContext;
use dataflow_rs_v3::engine::task_outcome::TaskOutcome;
use dataflow_rs_v3::{Template, TemplateCompiler};
use serde::Deserialize;
use serde_json::Value;

use super::schema::{FieldKind, FieldSchema};
use crate::channel::ChannelRuntimeConfig;
use crate::engine::kit::{self, read};
use crate::engine::{ErrorClass, HandlerError};
use crate::runtime::RuntimeGeneration;

/// This function's name: the one place it is written, for both engines
/// (F48).
pub const NAME: &str = "channel_call";

/// The refusal of a task that names no target.
const CHANNEL_REQUIRED: &str = "channel_call requires 'channel'";

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
    /// The process's expression engine, which compiled the target channel's
    /// guards and evaluates them on this ingress as on every other (I188).
    pub exprs: dataflow_rs::expr::ExprEngine,
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
    fn compile_input(input: &mut Self::Input, c: &TemplateCompiler) -> dataflow_rs_v3::Result<()> {
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
    ) -> dataflow_rs_v3::Result<TaskOutcome> {
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
            let target_runtime = Some(serviceable_target(&generation, &target_channel)?);
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
                exprs: &self.exprs,
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
                    return Err(failed(&target_channel, &summary).into());
                }
                // dataflow-rs 4's function passes the child's error on with
                // its own code instead (I184).
                crate::engine::RunOutcome::EngineError(e) => {
                    return Err(failed(&target_channel, &e).into());
                }
                crate::engine::RunOutcome::Timeout(ms) => {
                    return Err(timed_out(&target_channel, ms).into());
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
) -> dataflow_rs_v3::Result<String> {
    let Some(channel) = input.channel.as_ref() else {
        return Err(DataflowError::Validation(CHANNEL_REQUIRED.into()));
    };
    // Deliberately not `resolve_string`: a non-string result here is an
    // authoring mistake worth reporting, not something to coerce into a channel
    // name that cannot exist. A statically authored name is a constant on the
    // compiled template, so this costs no evaluation for the ordinary case.
    let result: Value = input_json(channel, ctx)?;
    Ok(target_name(result.as_str())?)
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
fn check_channel_shape(channel: &Template) -> dataflow_rs_v3::Result<()> {
    match channel_shape_problem(channel.as_json()) {
        Some(problem) => Err(DataflowError::Validation(problem)),
        None => Ok(()),
    }
}

/// [`check_channel_shape`]'s rule on the authored JSON, for both engines:
/// why `channel` can never name a channel, or `None` when it may.
fn channel_shape_problem(channel: &Value) -> Option<String> {
    match channel {
        Value::Object(_) | Value::Array(_) | Value::String(_) => None,
        // No channel at all, as serde read 1.12's `Option<Template>`, which
        // failed every call with this, and which only dataflow-rs 4 sees.
        Value::Null => Some(CHANNEL_REQUIRED.to_string()),
        other => Some(format!(
            "channel_call 'channel' must be a channel name or an expression \
             producing one, not {other}"
        )),
    }
}

/// A template's value for this message as `serde_json::Value`, through
/// `resolve` so a folded constant is served from the cache rather than
/// re-evaluated.
fn input_json(template: &Template, ctx: &TaskContext<'_>) -> dataflow_rs_v3::Result<Value> {
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

// -- dataflow-rs 4 --

/// The `metadata` root, which a call reads its position and its caller's
/// identity from and hands its child, amended.
static METADATA: LazyLock<Path> =
    LazyLock::new(|| "metadata".parse().expect("`metadata` is a path"));

/// `channel_call`'s input on dataflow-rs 4, its fields in
/// `CHANNEL_CALL_FIELDS`' order (I179), each read as 1.12 read it (I183).
///
/// The keys are the canonical ones. `channel_logic`, `data_logic` and
/// `response_path`, 1.12's serde aliases, are rewritten by the loader
/// before a definition is parsed (I186), so here they are unknown keys like
/// any other. `channel` is required, so a task naming no target is refused
/// at load, where 1.12 failed each call (I183); a non-string scalar is
/// refused at load as 1.12's `check_channel_shape` refused it. `data` and
/// `timeout_ms` written as `null` are unset ([`read::present`]), as serde
/// read 1.12's `Option<Template>`.
#[derive(Debug, FunctionConfig)]
#[config(check = ChannelCallConfig::check)]
pub struct ChannelCallConfig {
    /// The target channel: its name, or an expression giving one.
    channel: Expr<OwnedDataValue>,
    /// The payload sent; unset forwards the caller's own payload.
    data: Option<Expr<OwnedDataValue>>,
    /// Where the called channel's `data` is written.
    #[output(default = "data")]
    output: Output,
    /// The call's own timeout, in milliseconds, read by
    /// [`read::template_u64`] (1.12's `Template::resolve_u64`); unset takes
    /// the target channel's, then `engine.default_channel_call_timeout_ms`.
    timeout_ms: Option<Expr<OwnedDataValue>>,
}

impl ChannelCallConfig {
    /// What 1.12 refused when it built its engine: a `channel` that can name
    /// no channel whatever the message says.
    fn check(cfg: &Self, cx: &mut CompileCx<'_>) {
        if let Some(problem) = channel_shape_problem(cfg.channel.raw()) {
            cx.at("channel", |cx| cx.issue(IssueCode::InputParse, problem));
        }
    }
}

/// `channel_call` as a dataflow-rs 4 function: the call `ChannelCallHandler`
/// makes, refused in the same order and the same words, with the child run
/// on the generation's dataflow-rs 4 engine.
///
/// **The child inherits the run's limits (R60).** Its deadline is the
/// earliest of the call's own timeout (the task's, else the target's, else
/// the default) and the run's deadline, and its token is a child of the
/// run's, so the child ends with its partial `Run` instead of being dropped
/// with the call. When the call's own timeout came first (strictly: a tie is
/// the run's), the call fails with 1.12's `TIMEOUT_ERROR`, retryable.
///
/// **A child that stopped passes its error on (I184):** `Error::wrap` keeps
/// its kind, so its code, and its retryability, under 1.12's "channel_call to
/// '…' failed" prefix, where 1.12 rewrapped it as `FUNCTION_ERROR`. That
/// includes a child the run's own deadline or token stopped: a call's `Err`
/// after its run's own limit has passed ends the run on that limit,
/// `TimedOut` or `Cancelled`, with the limit's record on the task (dataflow-rs
/// `daaf8c0`), so the run reports its own limit, not a failure of this task.
///
/// The child's metadata is the caller's with the call position advanced,
/// `channel` naming the target, and the caller's error records stripped
/// before the target's guards read it, as 1.12 cleared them; the child's run
/// resets its own error context anyway (break 20).
pub struct ChannelCallFunction {
    /// The node's serving generation, loaded once per call: the target's
    /// guards and the engine that runs it come from the same build.
    pub runtime: Arc<crate::runtime::RuntimeHandle>,
    /// The process's expression engine, which compiled the target channel's
    /// guards and evaluates them (I188).
    pub exprs: dataflow_rs::expr::ExprEngine,
    pub max_call_depth: u32,
    pub default_timeout_ms: u64,
}

impl Function for ChannelCallFunction {
    type Config = ChannelCallConfig;

    fn spec(&self) -> FunctionSpec {
        kit::spec(NAME)
    }

    async fn call(
        &self,
        ctx: &mut Ctx<'_>,
        cfg: &ChannelCallConfig,
    ) -> dataflow_rs::Result<Outcome> {
        let target = target_name(ctx.eval(&cfg.channel)?.as_str())?;
        // The caller's metadata, owned once: the child is handed it.
        let mut metadata = match ctx.get(&METADATA) {
            Some(metadata) if metadata.is_object() => metadata.into_owned(),
            _ => OwnedDataValue::Object(Vec::new()),
        };
        let (depth, mut chain) = call_position(&metadata);
        check_position(self.max_call_depth, depth, &chain, &target)?;
        let data = match read::present(&cfg.data) {
            Some(expr) => ctx.eval(expr)?.into_owned(),
            // The caller's own payload, not its `data`, which may be empty.
            None => ctx.payload().clone(),
        };

        // The calling channel, read before `channel` is replaced below: it
        // is this call's caller identity, so a target's rate limit buckets
        // per calling channel rather than lumping every in-process caller
        // together (N16).
        let caller = metadata
            .get("channel")
            .and_then(OwnedDataValue::as_str)
            .unwrap_or(NAME)
            .to_string();
        // F4: the child runs as the target channel, so connector metrics and
        // circuit-breaker keys attribute to the channel actually executing.
        chain.push(target.clone());
        put(
            &mut metadata,
            "channel",
            OwnedDataValue::from(target.as_str()),
        );
        put(
            &mut metadata,
            META_CALL_DEPTH,
            OwnedDataValue::from(depth + 1),
        );
        put(&mut metadata, META_CALL_CHAIN, OwnedDataValue::from(chain));
        // #280: the child inherits the caller's metadata wholesale, so
        // without this its guards would read, and branch on, the caller's
        // failed tasks as if they were its own.
        strip_error_records(&mut metadata);

        // One generation for this call: the target's guards here and the
        // engine that runs it below come from the same build.
        let generation = self.runtime.load();
        let target_runtime = Some(serviceable_target(&generation, &target)?);
        let engine = generation
            .engine4
            .as_ref()
            .ok_or_else(|| not_found(&target))?;
        // Where the answer goes, before the guards spend anything on the
        // call: an output that names no path (`null` included, which 1.12
        // wrote to a key no response shows) runs nothing.
        let output = kit::output_path(ctx, &cfg.output, None)?;

        let admission = {
            // `validation_logic` is the one guard of this transport that
            // reads the message, as JSON; a target without one is admitted
            // with no copy of either.
            let view = target_runtime
                .as_ref()
                .is_some_and(|rt| rt.validation_logic.is_some())
                .then(|| (Value::from(&data), Value::from(&metadata)));
            let (view_data, view_metadata) = match &view {
                Some((data, metadata)) => (data, metadata),
                None => (&Value::Null, &Value::Null),
            };
            // The header view is the metadata inherited from the originating
            // request, so a target whose `rate_limit.key_logic` reads a
            // header still resolves one on this path.
            let header = |name: &str| {
                metadata
                    .get("headers")
                    .and_then(|headers| headers.get(name))
                    .and_then(OwnedDataValue::as_str)
                    .map(str::to_string)
            };
            crate::channel::guards::admit(crate::channel::guards::GuardRequest {
                transport: crate::channel::guards::Transport::ChannelCall,
                channel: &target,
                runtime: &target_runtime,
                data: view_data,
                metadata: view_metadata,
                exprs: &self.exprs,
                origin: None,
                caller_identity: &caller,
                header: &header,
                auth_backoff: None,
                raw_body: None,
                dedup_key_fallback: None,
                dedup_owner: None,
                default_timeout_ms: Some(self.default_timeout_ms),
                max_timeout_ms: None,
                oauth: None,
            })
            .await
            .map_err(|e| guard_refused(&target, e))?
        };
        let _backpressure_permit = admission.backpressure_permit;

        // Timeout precedence: the task's > the target channel's > the
        // engine default (the last two resolved by the guard chain).
        let task_timeout_ms = match read::present(&cfg.timeout_ms) {
            Some(expr) => {
                let value = ctx.eval(expr)?;
                Some(read::template_u64(&value, "channel_call 'timeout_ms'")?)
            }
            None => None,
        };
        let timeout_ms = task_timeout_ms
            .or(admission.timeout_ms)
            .unwrap_or(self.default_timeout_ms);

        let limits = ChildLimits::new(own_deadline(timeout_ms), ctx.deadline());
        let token = ctx.cancel().map(CancelToken::child);
        // No routing bucket: the child executes within its caller's rollout
        // decision rather than drawing one of its own. No audit trail and no
        // trace: the call already runs inside its caller's.
        let mut options = RunOptions::default()
            .channel(&target)
            .capture_changes(false)
            .audit(AuditMode::Off);
        if let Some(deadline) = limits.deadline {
            options = options.deadline(deadline);
        }
        if let Some(token) = &token {
            options = options.cancel(token);
        }
        if let Some(accountant) = ctx.accountant() {
            options = options.accountant(accountant);
        }
        let mut child = Message::from_parts(data, OwnedDataValue::Null, metadata);
        let run = engine.run_with(&mut child, &options).await;

        // Zero matches, read from the run, not inferred from the registry
        // (07's E13). A target the engine holds no workflow for, so that
        // none was even considered, is refused as one not found. A target
        // whose workflows were considered and all declined answers with the
        // child's untouched `data`, as 1.12 answered it.
        if run.matched() == 0 && run.workflows().next().is_none() {
            return Err(not_found(&target).into());
        }
        match run.status() {
            RunStatus::Completed => {}
            RunStatus::CompletedWithErrors => {
                return Err(failed(&target, summary(child.errors())).into());
            }
            RunStatus::TimedOut if limits.own => {
                return Err(timed_out(&target, timeout_ms).into());
            }
            // A workflow's error, or the run's own deadline or token handed
            // down: the child's error, passed on. After the run's own limit
            // the engine ends this run on that limit instead (`daaf8c0`).
            _ => {
                if let Err(e) = run.into_result() {
                    return Err(e.wrap(format_args!("channel_call to '{target}' failed")));
                }
            }
        }
        ctx.set(&output, child.context().data().clone())?;
        Ok(Outcome::Ok)
    }
}

/// The deadline and cancellation a child's run inherits, and whose deadline
/// it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChildLimits {
    /// The child's deadline: the earlier of the call's own and the run's.
    deadline: Option<Instant>,
    /// Whether it is the call's own: strictly before the run's, so a tie is
    /// the run's.
    own: bool,
}

impl ChildLimits {
    fn new(own: Option<Instant>, run: Option<Instant>) -> Self {
        match (own, run) {
            (Some(own), Some(run)) if own < run => Self {
                deadline: Some(own),
                own: true,
            },
            (_, Some(run)) => Self {
                deadline: Some(run),
                own: false,
            },
            (own, None) => Self {
                deadline: own,
                own: own.is_some(),
            },
        }
    }
}

/// `ms` from now on the clock the engine's limits read (Tokio's natively,
/// so a paused test clock agrees with them), or `None` past the clock's
/// range: an unbounded call, as `tokio::time::timeout` gave 1.12 one.
fn own_deadline(ms: u64) -> Option<Instant> {
    tokio::time::Instant::now()
        .into_std()
        .checked_add(Duration::from_millis(ms))
}

/// `metadata` without the caller's error records: the key at Orion's
/// error-context path ([`crate::engine::ERROR_CONTEXT`]), read as the
/// engine reads it, when that path is under `metadata`.
fn strip_error_records(metadata: &mut OwnedDataValue) {
    let path = crate::engine::ERROR_CONTEXT.path();
    if path.root() != dataflow_rs::expr::Root::Metadata {
        return;
    }
    let root = path.root().as_str().to_string();
    let mut context = OwnedDataValue::Object(vec![(root, std::mem::take(metadata))]);
    let removed = path.remove(&mut context).is_ok();
    debug_assert!(removed, "an error context's path is never a root");
    if let OwnedDataValue::Object(mut roots) = context
        && let Some((_, stripped)) = roots.pop()
    {
        *metadata = stripped;
    }
}

/// `key` set to `value` in `object`, in place when it is there and last
/// when it is not, as `serde_json`'s `IndexMap` did for 1.12's metadata.
fn put(object: &mut OwnedDataValue, key: &str, value: OwnedDataValue) {
    if let OwnedDataValue::Object(entries) = object {
        match entries.iter_mut().find(|(k, _)| k == key) {
            Some((_, slot)) => *slot = value,
            None => entries.push((key.to_string(), value)),
        }
    }
}

/// The `code: message; code: message` summary of a child's error records,
/// as `execute_admitted` builds it for 1.12's `WorkflowErrors`.
fn summary(errors: &[dataflow_rs::run::ErrorRecord]) -> String {
    errors
        .iter()
        .map(|e| format!("{}: {}", e.code, e.message))
        .collect::<Vec<_>>()
        .join("; ")
}

/// [`guard_refusal`] on dataflow-rs 4's error: a `validation_logic`
/// rejection stays the caller's `VALIDATION_ERROR`, and every other refusal
/// keeps the status the guard chose, so the caller sees `429`/`503` rather
/// than a generic `500`.
fn guard_refused(target: &str, e: crate::errors::OrionError) -> dataflow_rs::Error {
    let (status, _code, detail) = e.response_parts();
    let message = format!("channel_call to '{target}': {detail}");
    if status == axum::http::StatusCode::BAD_REQUEST {
        HandlerError::new(ErrorClass::CallerInput, message).into()
    } else {
        crate::errors::service::channel_refused(status, message)
    }
}

// -- What both engines share --
//
// The call position, the target's judgement and every refusal 1.12 words,
// once, as `HandlerError`s of the class its `DataflowError` variant maps to:
// `CallerInput` is 1.12's `Validation`, `Backend` its `function_execution`
// and `Timeout` its `Timeout`, so dataflow-rs 3 rebuilds the variant and
// dataflow-rs 4 records its code and its text (I184).

/// Where a message stands in a call chain, as its metadata records it: the
/// depth (`_orion_call_depth`, a number) and the channels called so far
/// (`_orion_call_chain`, its strings). Anything else reads as the top of a
/// chain: depth 0, no channels.
fn call_position(metadata: &OwnedDataValue) -> (u64, Vec<String>) {
    let depth = metadata
        .get(META_CALL_DEPTH)
        .and_then(OwnedDataValue::as_i64)
        .map(|n| n as u64)
        .unwrap_or(0);
    let chain = metadata
        .get(META_CALL_CHAIN)
        .and_then(OwnedDataValue::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    (depth, chain)
}

/// The recursion guards: a call one past `max_depth` deep, or to a channel
/// already on the chain, is refused before anything about the target is
/// looked up.
fn check_position(
    max_depth: u32,
    depth: u64,
    chain: &[String],
    target: &str,
) -> Result<(), HandlerError> {
    if depth >= u64::from(max_depth) {
        return Err(HandlerError::new(
            ErrorClass::CallerInput,
            format!(
                "channel_call: max call depth {max_depth} exceeded (chain: {})",
                format_chain(chain, target),
            ),
        ));
    }
    if chain.iter().any(|called| called == target) {
        return Err(HandlerError::new(
            ErrorClass::CallerInput,
            format!(
                "channel_call: cycle detected: {}",
                format_chain(chain, target)
            ),
        ));
    }
    Ok(())
}

/// The target's name from what `channel` evaluated to. Deliberately not a
/// coercion: a non-string result is an authoring mistake worth reporting,
/// not something to turn into a channel name that cannot exist.
fn target_name(value: Option<&str>) -> Result<String, HandlerError> {
    match value {
        None => Err(HandlerError::new(
            ErrorClass::CallerInput,
            "channel_call 'channel' must evaluate to a string",
        )),
        Some("") => Err(HandlerError::new(
            ErrorClass::CallerInput,
            "channel_call: target channel name must not be empty",
        )),
        Some(name) => Ok(name.to_string()),
    }
}

/// The target's runtime config in `generation`, when it may be called: not
/// quarantined (F35), active, and not a cron channel (D6).
///
/// A target absent from the registry is not an active channel, and is
/// refused loudly: a run on an unknown channel matches zero workflows and
/// reports success, which would silently drop the call (a typo'd channel
/// name would "work" with `output` never populated).
///
/// A cron channel is not callable. Its workflow is meant to run once per
/// occurrence, recorded in the ledger and serialised by its singleton key; a
/// `channel_call` would run it with none of that — no occurrence row, and no
/// lock, so a caller could trivially overlap a `forbid` schedule with
/// itself. The refusal is here rather than in the guard matrix because it is
/// about *what the channel is*, not about which guards this transport
/// applies.
fn serviceable_target(
    generation: &RuntimeGeneration,
    target: &str,
) -> Result<Arc<ChannelRuntimeConfig>, HandlerError> {
    let runtime = generation
        .channels
        .require_serviceable(target)
        .map_err(|e| {
            HandlerError::new(
                ErrorClass::Backend,
                format!("channel_call to '{target}': {e}"),
            )
        })?
        .ok_or_else(|| not_found(target))?;
    if runtime.cron.is_some() {
        // 1.12's text, the two runs of spaces included (a line continuation
        // that lost its backslash): a client reads it, so it holds.
        return Err(HandlerError::new(
            ErrorClass::Backend,
            format!(
                "channel_call to '{target}': it is a cron channel, which                          runs only on its own schedule. Call the workflow it names                          directly, or give the shared work a channel of its own."
            ),
        ));
    }
    Ok(runtime)
}

/// The refusal of a target with no workflow to run it.
fn not_found(target: &str) -> HandlerError {
    HandlerError::new(
        ErrorClass::Backend,
        format!("channel_call to '{target}': channel not found or not active"),
    )
}

/// The refusal of a target whose run failed: its error records summarised,
/// or, on dataflow-rs 3, the error that stopped it.
fn failed(target: &str, why: impl std::fmt::Display) -> HandlerError {
    HandlerError::new(
        ErrorClass::Backend,
        format!("channel_call to '{target}' failed: {why}"),
    )
}

/// The refusal of a target whose run outlived the call's own timeout.
fn timed_out(target: &str, ms: u64) -> HandlerError {
    HandlerError::new(
        ErrorClass::Timeout,
        format!("channel_call to '{target}' timed out after {ms}ms"),
    )
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use dataflow_rs::function::testing;
    use dataflow_rs::observe::{Event, Interest, Observer};
    use dataflow_rs::program::Severity;
    use dataflow_rs::run::EngineConfig;
    use serde_json::json;

    use super::*;
    use crate::engine::functions::twin::{self, Failure, Seen};
    use crate::runtime::RuntimeHandle;

    /// `engine.max_channel_call_depth`'s default.
    const MAX_DEPTH: u32 = 10;

    // -- The world: one generation holding both engines --

    /// One channel of a [`World`]: its row's config, the tasks of the one
    /// workflow serving it (none for a channel no workflow serves), and
    /// whether it is a cron channel.
    #[derive(Clone)]
    struct Chan {
        name: &'static str,
        config: Value,
        tasks: Option<Value>,
        condition: Value,
        cron: bool,
    }

    /// A channel whose workflow runs `tasks`.
    fn chan(name: &'static str, tasks: Value) -> Chan {
        Chan {
            name,
            config: json!({}),
            tasks: Some(tasks),
            condition: json!(true),
            cron: false,
        }
    }

    impl Chan {
        fn config(mut self, config: Value) -> Self {
            self.config = config;
            self
        }

        fn condition(mut self, condition: Value) -> Self {
            self.condition = condition;
            self
        }

        fn unserved(mut self) -> Self {
            self.tasks = None;
            self
        }

        fn cron(mut self) -> Self {
            self.cron = true;
            self
        }

        fn named(mut self, name: &'static str) -> Self {
            self.name = name;
            self
        }

        fn row(&self) -> crate::storage::models::Channel {
            let now = chrono::Utc::now().naive_utc();
            crate::storage::models::Channel {
                tags_json: "[]".to_string(),
                channel_id: format!("ch_{}", self.name),
                version: 1,
                name: self.name.to_string(),
                description: None,
                channel_type: if self.cron { "async" } else { "sync" }.to_string(),
                protocol: if self.cron { "cron" } else { "http" }.to_string(),
                methods_json: None,
                route_pattern: None,
                topic: None,
                consumer_group: None,
                transport_config_json: if self.cron {
                    json!({"schedule": "0 15 2 * * *"}).to_string()
                } else {
                    "{}".to_string()
                },
                workflow_id: Some(self.name.to_string()),
                config_json: self.config.to_string(),
                status: "active".to_string(),
                priority: 0,
                created_at: now,
                updated_at: now,
            }
        }

        /// The workflow serving it, as both engines read it.
        fn workflow(&self) -> Option<Value> {
            self.tasks.as_ref().map(|tasks| {
                json!({
                    "id": self.name, "name": self.name, "channel": self.name,
                    "condition": self.condition, "tasks": tasks,
                })
            })
        }
    }

    /// A task calling `channel_call` with `input`.
    fn call(input: Value) -> Value {
        json!([{"id": "t", "name": "t", "function": {"name": NAME, "input": input}}])
    }

    /// Every run the dataflow-rs 4 engine finished, in order: its channel
    /// and how it ended.
    #[derive(Default)]
    struct Runs {
        started: Mutex<Vec<(String, String)>>,
        finished: Mutex<Vec<(String, String)>>,
    }

    impl Observer for Runs {
        fn interest(&self) -> Interest {
            Interest::NONE
        }

        fn on(&self, event: &Event<'_>) {
            match event {
                Event::RunStarted {
                    message_id,
                    channel,
                    ..
                } => self
                    .started
                    .lock()
                    .expect("the runs lock")
                    .push((message_id.to_string(), (*channel).unwrap_or("").to_string())),
                Event::RunFinished {
                    message_id, status, ..
                } => {
                    let channel = self
                        .started
                        .lock()
                        .expect("the runs lock")
                        .iter()
                        .find(|(id, _)| id == message_id)
                        .map(|(_, channel)| channel.clone())
                        .unwrap_or_default();
                    let status = match status {
                        RunStatus::Completed => "completed".to_string(),
                        RunStatus::CompletedWithErrors => "completed_with_errors".to_string(),
                        RunStatus::Stopped(e) => format!("stopped {}", e.code()),
                        RunStatus::TimedOut => "timed_out".to_string(),
                        RunStatus::Cancelled => "cancelled".to_string(),
                        _ => "other".to_string(),
                    };
                    self.finished
                        .lock()
                        .expect("the runs lock")
                        .push((channel, status));
                }
                _ => {}
            }
        }
    }

    impl Runs {
        /// Every finished run, as `(channel, status)`.
        fn finished(&self) -> Vec<(String, String)> {
            self.finished.lock().expect("the runs lock").clone()
        }

        /// The channels of every run started.
        fn channels(&self) -> Vec<String> {
            self.started
                .lock()
                .expect("the runs lock")
                .iter()
                .map(|(_, channel)| channel.clone())
                .collect()
        }
    }

    /// Waits as long as `ms` says (an hour by default), in both engines: a
    /// target that is still running when a limit fires.
    #[derive(Debug, FunctionConfig)]
    struct WaitConfig {
        #[config(default = 3_600_000)]
        ms: u64,
    }

    struct Wait;

    impl Function for Wait {
        type Config = WaitConfig;

        async fn call(&self, _: &mut Ctx<'_>, cfg: &WaitConfig) -> dataflow_rs::Result<Outcome> {
            tokio::time::sleep(Duration::from_millis(cfg.ms)).await;
            Ok(Outcome::Ok)
        }
    }

    struct WaitHandler;

    #[async_trait]
    impl AsyncFunctionHandler for WaitHandler {
        type Input = Value;

        async fn execute(
            &self,
            _: &mut TaskContext<'_>,
            input: &Value,
        ) -> dataflow_rs_v3::Result<TaskOutcome> {
            let ms = input.get("ms").and_then(Value::as_u64).unwrap_or(3_600_000);
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Ok(TaskOutcome::Success)
        }
    }

    /// Fails with a connection error, `IO_ERROR` and retryable, in both
    /// engines: a code that is not `FUNCTION_ERROR`, so passing it on shows.
    #[derive(Debug, FunctionConfig)]
    struct NoConfig {}

    struct Fail;

    impl Function for Fail {
        type Config = NoConfig;

        async fn call(&self, _: &mut Ctx<'_>, _: &NoConfig) -> dataflow_rs::Result<Outcome> {
            Err(HandlerError::new(ErrorClass::Connector, "the backend is down").into())
        }
    }

    struct FailHandler;

    #[async_trait]
    impl AsyncFunctionHandler for FailHandler {
        type Input = Value;

        async fn execute(
            &self,
            _: &mut TaskContext<'_>,
            _: &Value,
        ) -> dataflow_rs_v3::Result<TaskOutcome> {
            Err(HandlerError::new(ErrorClass::Connector, "the backend is down").into())
        }
    }

    /// Cancels the token it holds, then waits: a parent's token tripped
    /// while its child runs. dataflow-rs 4 only; 1.12 had no token.
    struct Trip(CancelToken);

    impl Function for Trip {
        type Config = NoConfig;

        async fn call(&self, _: &mut Ctx<'_>, _: &NoConfig) -> dataflow_rs::Result<Outcome> {
            self.0.cancel();
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(Outcome::Ok)
        }
    }

    /// One generation of a node serving `chans`, published on its handle:
    /// the channel estate, a dataflow-rs 3 engine and a dataflow-rs 4
    /// engine built from the same channels, each with `channel_call` itself
    /// registered, so calls chain.
    struct World {
        runtime: Arc<RuntimeHandle>,
        v3: Arc<dataflow_rs_v3::Engine>,
        v4: dataflow_rs::Engine,
        runs: Arc<Runs>,
        default_timeout_ms: u64,
    }

    /// The world's knobs: the engine default timeout, the channels
    /// quarantined, and the token `trip` cancels.
    struct Knobs {
        default_timeout_ms: u64,
        quarantined: Vec<&'static str>,
        trip: CancelToken,
        engine4: bool,
    }

    impl Default for Knobs {
        fn default() -> Self {
            Self {
                default_timeout_ms: 30_000,
                quarantined: Vec::new(),
                trip: CancelToken::new(),
                engine4: true,
            }
        }
    }

    async fn world(chans: &[Chan]) -> World {
        world_with(chans, Knobs::default()).await
    }

    async fn world_with(chans: &[Chan], knobs: Knobs) -> World {
        let exprs = kit::test_exprs();
        let runtime = crate::runtime::generation::test_handle();
        let client = reqwest::Client::new();
        let rows: Vec<_> = chans.iter().map(Chan::row).collect();
        let issues = knobs
            .quarantined
            .iter()
            .map(|name| crate::channel::ChannelLoadIssue {
                channel: name.to_string(),
                reason: "its workflow did not build".to_string(),
            })
            .collect();
        let connectors = crate::connector::ConnectorRegistry::new(Default::default());
        let channels = crate::channel::ChannelLoader::new()
            .build(
                &crate::channel::ChannelSnapshot::empty(),
                &rows,
                crate::channel::ReloadDeps {
                    vars: None,
                    connector_registry: &connectors,
                    cache_pool: &crate::connector::cache_backend::CachePool::new(4, 60, 128),
                    exprs: &exprs,
                    jwks: &Arc::new(crate::jwt::jwks::JwksCache::new(client.clone(), false)),
                    http_client: &client,
                    allow_private_token_urls: false,
                    instance_oauth_providers: &std::collections::BTreeMap::new(),
                    oidc_discovery: &Arc::new(crate::channel::oidc_discovery::DiscoveryCache::new(
                        client.clone(),
                        false,
                    )),
                    global_trace_storage: &crate::config::TraceStorageConfig::default(),
                    cron_enabled: true,
                },
                issues,
            )
            .await;

        let workflows: Vec<Value> = chans.iter().filter_map(Chan::workflow).collect();

        let mut handlers: std::collections::HashMap<String, dataflow_rs_v3::BoxedFunctionHandler> =
            Default::default();
        handlers.insert(
            NAME.to_string(),
            Box::new(ChannelCallHandler {
                runtime: runtime.clone(),
                exprs: exprs.clone(),
                max_call_depth: MAX_DEPTH,
                default_timeout_ms: knobs.default_timeout_ms,
            }),
        );
        handlers.insert("wait".to_string(), Box::new(WaitHandler));
        handlers.insert("fail".to_string(), Box::new(FailHandler));
        // 1.12 had no token to trip: on dataflow-rs 3 the target just waits.
        handlers.insert("trip".to_string(), Box::new(WaitHandler));
        let v3_workflows = workflows
            .iter()
            .map(|w| dataflow_rs_v3::Workflow::from_json(&w.to_string()).expect("a v3 workflow"));
        // Built as 1.12 built every engine: Orion's operators, its secrets
        // and its error context.
        let mut v3 = dataflow_rs_v3::Engine::builder().with_workflows(v3_workflows);
        for (name, handler) in handlers {
            v3 = v3.register_boxed(name, handler);
        }
        let v3 = crate::engine::operators::with_orion_engine_defaults(
            v3,
            &crate::engine::ResolvedSecrets::empty(),
        );
        let v3 = Arc::new(v3.build().expect("the v3 engine builds"));

        let runs = Arc::new(Runs::default());
        let mut registry = dataflow_rs::Registry::new(exprs.clone());
        registry.register(
            NAME,
            ChannelCallFunction {
                runtime: runtime.clone(),
                exprs: exprs.clone(),
                max_call_depth: MAX_DEPTH,
                default_timeout_ms: knobs.default_timeout_ms,
            },
        );
        registry.register("wait", Wait);
        registry.register("fail", Fail);
        registry.register("trip", Trip(knobs.trip));
        let config = EngineConfig::default()
            .error_context(crate::engine::ERROR_CONTEXT.clone())
            .observer(runs.clone());
        let mut builder = dataflow_rs::Engine::builder()
            .registry(registry)
            .config(config);
        for workflow in &workflows {
            builder = builder.workflow_json(workflow);
        }
        let v4 = builder.build().expect("the v4 engine builds");

        runtime.publish(
            v3.clone(),
            knobs.engine4.then(|| v4.clone()),
            Arc::new(channels),
            crate::engine::FunctionRegistry::builtin().clone(),
            Arc::new(crate::plugin::PluginSet::empty()),
            Arc::new(crate::model::ModelSet::empty()),
        );
        World {
            runtime,
            v3,
            v4,
            runs,
            default_timeout_ms: knobs.default_timeout_ms,
        }
    }

    /// The record a task `task` that failed with `failure` carries.
    fn record_of(task: Option<&str>, failure: &Failure) -> String {
        format!("Task {} error: {}", task.unwrap_or("?"), failure.message)
    }

    impl World {
        /// `channel` run on dataflow-rs 3 over `payload` and `metadata`, as
        /// `execute_admitted` builds the message.
        async fn v3(&self, channel: &str, payload: Value, metadata: Value) -> Seen {
            let mut message = dataflow_rs_v3::Message::builder()
                .payload_json(&payload)
                .metadata_json(&metadata)
                .capture_changes(false)
                .audit_mode(dataflow_rs_v3::AuditMode::Off)
                .build();
            match self
                .v3
                .process_message_for_channel(channel, &mut message)
                .await
            {
                Ok(()) => Ok(Value::from(message.data())),
                Err(e) => {
                    let record = message.errors().first().expect("the failure is recorded");
                    let failure = Failure {
                        code: record.code.clone(),
                        message: e.to_string(),
                        retryable: e.retryable(),
                        detail: record.detail.clone(),
                    };
                    assert_eq!(
                        record.message,
                        record_of(record.task_id.as_deref(), &failure)
                    );
                    Err(failure)
                }
            }
        }

        /// `channel` run on dataflow-rs 4 over `payload` and `metadata`,
        /// under `options` (the channel set here): the run and the message.
        async fn v4_run(
            &self,
            channel: &str,
            payload: Value,
            metadata: Value,
            options: RunOptions<'_>,
        ) -> (dataflow_rs::Run, Message) {
            let mut message = Message::from_parts(
                OwnedDataValue::from(&payload),
                OwnedDataValue::Null,
                OwnedDataValue::from(&metadata),
            );
            let options = options
                .channel(channel)
                .capture_changes(false)
                .audit(AuditMode::Off);
            let run = self.v4.run_with(&mut message, &options).await;
            (run, message)
        }

        /// [`Self::v4_run`] with no limits, as [`Seen`]: `data`, or the
        /// failure that stopped the run, its record checked.
        async fn v4(&self, channel: &str, payload: Value, metadata: Value) -> Seen {
            let (run, message) = self
                .v4_run(channel, payload, metadata, RunOptions::default())
                .await;
            seen(&run, &message)
        }
    }

    /// A dataflow-rs 4 run as [`Seen`]: `data` when every workflow finished,
    /// error records or not, as dataflow-rs 3's `Ok(())` reads it.
    fn seen(run: &dataflow_rs::Run, message: &Message) -> Seen {
        match run.status() {
            RunStatus::Completed | RunStatus::CompletedWithErrors => {
                Ok(Value::from(message.context().data()))
            }
            RunStatus::Stopped(e) => {
                let failure = twin::failure(e);
                let record = message.errors().last().expect("the failure is recorded");
                assert_eq!(
                    record.message,
                    record_of(record.task_id.as_deref(), &failure)
                );
                assert_eq!(record.code, failure.code);
                Err(failure)
            }
            other => unreachable!("the run ended {other:?}"),
        }
    }

    /// The same run on both engines, each in a world of its own, so a
    /// guard's state (a rate limit's tokens) is not shared: what each saw.
    async fn both(chans: &[Chan], channel: &str, payload: Value, metadata: Value) -> (Seen, Seen) {
        let v3 = world(chans)
            .await
            .v3(channel, payload.clone(), metadata.clone())
            .await;
        let v4 = world(chans).await.v4(channel, payload, metadata).await;
        (v3, v4)
    }

    /// A target that greets the payload's `name`, as `channel_call_test`'s
    /// basic target does.
    fn greeter() -> Chan {
        chan(
            "target",
            json!([
                {"id": "t0", "name": "t0", "function": {"name": "parse_json",
                    "input": {"source": "payload", "target": "input"}}},
                {"id": "t1", "name": "t1", "function": {"name": "map", "input": {"mappings": [
                    {"path": "data.greeting",
                     "logic": {"cat": ["Hello, ", {"var": "data.input.name"}]}}
                ]}}}
            ]),
        )
    }

    // -- The config --

    /// Declaration order is the field table's (I179); `channel` is
    /// required (I183); `output` defaults to `data`; `data` and
    /// `timeout_ms` written as `null` are unset, as serde read 1.12's
    /// `Option<Template>`.
    #[test]
    fn the_config_is_the_field_table() {
        let fields: Vec<&str> = ChannelCallConfig::fields()
            .iter()
            .map(|f| &*f.name)
            .collect();
        let table: Vec<&str> = CHANNEL_CALL_FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(fields, table);

        let compile =
            |input: Value| testing::config_with::<ChannelCallConfig>(&kit::test_exprs(), input);
        let cfg = compile(json!({"channel": "orders"})).expect("compiles");
        assert_eq!(
            cfg.channel.constant(),
            Some(&OwnedDataValue::from("orders"))
        );
        assert_eq!(
            cfg.output.constant().map(ToString::to_string).as_deref(),
            Some("data")
        );
        assert!(cfg.data.is_none() && cfg.timeout_ms.is_none());
        let cfg = compile(json!({"channel": {"var": "data.to"}, "data": null,
                                 "output": null, "timeout_ms": null}))
        .expect("compiles");
        assert!(cfg.channel.constant().is_none());
        assert!(read::present(&cfg.data).is_none() && read::present(&cfg.timeout_ms).is_none());
        assert_eq!(
            cfg.output.constant().map(ToString::to_string).as_deref(),
            Some("data")
        );
    }

    /// What 1.12 refused when it built its engine is refused at load, in
    /// its words: a `channel` that is a non-string scalar
    /// (`check_channel_shape`, which `compile_input` ran). A `null` one,
    /// which 1.12 read as no channel and refused on every call, is refused
    /// with that call's words (a named break). A computed or literal name
    /// loads, as 1.12 loaded it.
    #[test]
    fn a_channel_that_can_name_no_channel_is_refused_at_load_in_1_12s_words() {
        for channel in [json!(7), json!(true), json!(1.5)] {
            let mut input: ChannelCallInput =
                serde_json::from_value(json!({ "channel": channel })).expect("1.12 reads it");
            let v3 = check_channel_shape(input.channel.as_mut().expect("set"))
                .expect_err("1.12 refused it at load");
            let DataflowError::Validation(expected) = v3 else {
                unreachable!("1.12 refused it as a validation error: {v3:?}");
            };
            let issues = testing::config_with::<ChannelCallConfig>(
                &kit::test_exprs(),
                json!({ "channel": channel }),
            )
            .expect_err("refused at load");
            let refused: Vec<_> = issues
                .iter()
                .filter(|i| i.severity == Severity::Rejected)
                .map(|i| (i.path.to_string(), i.code, i.message.clone()))
                .collect();
            assert_eq!(
                refused,
                [("channel".to_string(), IssueCode::InputParse, expected)],
                "{channel}"
            );
        }
        // `null` is no channel: 1.12 failed every call with its refusal.
        let input: ChannelCallInput =
            serde_json::from_value(json!({"channel": null})).expect("1.12 reads it");
        let datalogic = Arc::new(dataflow_rs_v3::datalogic_rs::Engine::new());
        let mut message = dataflow_rs_v3::Message::from_value(&json!({}));
        let v3 = resolve_target(&TaskContext::new(&mut message, &datalogic), &input)
            .expect_err("1.12 refused every call");
        let DataflowError::Validation(expected) = v3 else {
            unreachable!("1.12 refused it as a validation error: {v3:?}");
        };
        let issues =
            testing::config_with::<ChannelCallConfig>(&kit::test_exprs(), json!({"channel": null}))
                .expect_err("refused at load");
        assert_eq!(
            issues
                .iter()
                .map(|i| (i.path.to_string(), i.code, i.message.clone()))
                .collect::<Vec<_>>(),
            [("channel".to_string(), IssueCode::InputParse, expected)]
        );
        for channel in [json!("orders"), json!({"var": "data.to"}), json!(["a"])] {
            testing::config_with::<ChannelCallConfig>(
                &kit::test_exprs(),
                json!({ "channel": channel }),
            )
            .expect("loads, as on 1.12");
        }
    }

    /// The named load refusals (O11a's CHANGELOG): `channel` missing
    /// (I183), and an unknown key (break 2a), which 1.12's `deny_unknown:
    /// false` accepted and ignored. 1.12's aliases are unknown keys here,
    /// since the loader rewrites them first (I186).
    #[test]
    fn a_missing_channel_and_an_unknown_key_are_refused_at_load() {
        let rejected = |input: Value| -> Vec<(String, IssueCode)> {
            testing::config_with::<ChannelCallConfig>(&kit::test_exprs(), input)
                .expect_err("refused")
                .into_iter()
                .filter(|i| i.severity == Severity::Rejected)
                .map(|i| (i.path.to_string(), i.code))
                .collect()
        };
        assert_eq!(
            rejected(json!({})),
            [("channel".to_string(), IssueCode::InputParse)]
        );
        let unknown = rejected(json!({"channel": "orders", "chanel_timeout": 5}));
        assert_eq!(unknown.len(), 1, "{unknown:?}");
        assert_eq!(unknown[0].0, "chanel_timeout");
        let alias = rejected(json!({"channel_logic": "orders"}));
        assert!(
            alias.iter().any(|(path, _)| path == "channel_logic")
                && alias.iter().any(|(path, _)| path == "channel"),
            "{alias:?}"
        );
    }

    /// A literal `output` that names no path is refused at load: one that
    /// is not a string, which 1.12 refused at authoring (`TYPE_MISMATCH`),
    /// and a string naming no context root, `""` included (O3's ruled class),
    /// which 1.12 wrote to a key no response shows. Named breaks for O11a.
    #[test]
    fn a_literal_output_naming_no_path_is_refused_at_load() {
        let authoring = crate::engine::FunctionRegistry::builtin().validate_input(
            NAME,
            &json!({"channel": "a", "output": 7}),
            "tasks[0]",
        );
        assert!(
            authoring.iter().any(|e| e.code == "TYPE_MISMATCH"),
            "{authoring:?}"
        );
        for (output, code) in [
            (json!(7), IssueCode::InputParse),
            (json!(""), IssueCode::InvalidPath),
            (json!("result"), IssueCode::InvalidPath),
        ] {
            let issues = testing::config_with::<ChannelCallConfig>(
                &kit::test_exprs(),
                json!({"channel": "a", "output": output}),
            )
            .expect_err("refused at load");
            assert_eq!(
                issues
                    .iter()
                    .map(|i| (i.path.to_string(), i.code))
                    .collect::<Vec<_>>(),
                [("output".to_string(), code)],
                "{output}"
            );
        }
    }

    // -- The call --

    /// The basic call, `channel_call_test`'s first: the child runs on the
    /// generation's dataflow-rs 4 engine, on the target channel, and what it
    /// left in `data` is written at `output`, as 1.12 wrote it.
    #[tokio::test]
    async fn the_child_runs_on_engine4_and_its_data_is_written_at_output() {
        let chans = [
            greeter(),
            chan(
                "caller",
                call(json!({"channel": "target", "output": "data.result"})),
            ),
        ];
        let (v3, v4) = both(&chans, "caller", json!({"name": "World"}), json!({})).await;
        assert_eq!(v4, v3);
        assert_eq!(
            v4.expect("the call answers")["result"]["greeting"],
            "Hello, World"
        );

        let world = world(&chans).await;
        world
            .v4("caller", json!({"name": "World"}), json!({}))
            .await
            .expect("answers");
        // Both runs reported to the dataflow-rs 4 engine's observer: the
        // child ran on the generation's `engine4`.
        assert!(world.runtime.load().engine4.is_some());
        assert_eq!(world.runs.channels(), ["caller", "target"]);
    }

    /// Without `engine4` there is no workflow to run the target: the call
    /// is refused as 1.12 refused a target nothing serves, and nothing runs.
    #[tokio::test]
    async fn a_generation_without_engine4_runs_no_child() {
        let chans = [
            greeter(),
            chan("caller", call(json!({"channel": "target"}))),
        ];
        let world = world_with(
            &chans,
            Knobs {
                engine4: false,
                ..Knobs::default()
            },
        )
        .await;
        let failure = world
            .v4("caller", json!({}), json!({}))
            .await
            .expect_err("refused");
        let expected: DataflowError = not_found("target").into();
        assert_eq!(failure.message, expected.to_string());
        assert_eq!(failure.code, "FUNCTION_ERROR");
        assert_eq!(world.runs.channels(), ["caller"], "no child ran");
    }

    /// `data` is what is sent, and without it the caller's payload is
    /// forwarded; the output is the default `data`, or computed; every field
    /// read from the message, as 1.12 read it.
    #[tokio::test]
    async fn the_fields_are_read_as_1_12_read_them() {
        let echo = chan(
            "echo",
            json!([{"id": "p", "name": "p", "function": {"name": "parse_json",
                "input": {"source": "payload", "target": "got"}}}]),
        );
        for input in [
            json!({"channel": "echo"}),
            json!({"channel": "echo", "output": "data.r"}),
            json!({"channel": "echo", "data": {"var": "data.parsed.inner"}, "output": "data.r"}),
            json!({"channel": "echo", "data": {"cat": ["a", "b"]}, "output": "temp_data.r"}),
            json!({"channel": {"cat": ["ec", "ho"]}, "output": {"cat": ["data.", "x"]}}),
            json!({"channel": "echo", "output": "data.r", "timeout_ms": 5000}),
            json!({"channel": "echo", "output": "data.r", "timeout_ms": {"var": "data.parsed.ms"}}),
            json!({"channel": "echo", "output": "data.r", "timeout_ms": 2.5}),
        ] {
            let caller = chan(
                "caller",
                json!([
                    {"id": "p", "name": "p", "function": {"name": "parse_json",
                        "input": {"source": "payload", "target": "parsed"}}},
                    {"id": "t", "name": "t", "function": {"name": NAME, "input": input.clone()}}
                ]),
            );
            let chans = [echo.clone(), caller];
            let payload = json!({"inner": {"k": 1}, "ms": 5000});
            let (v3, v4) = both(&chans, "caller", payload, json!({})).await;
            assert_eq!(v4, v3, "{input}");
            assert!(v4.is_ok(), "{input}: {v4:?}");
        }
    }

    /// The call position as 1.12 advanced it: the child's metadata holds the
    /// depth plus one, the chain with the target on the end, `channel`
    /// naming the target, and the rest of the caller's metadata, its
    /// headers included.
    #[tokio::test]
    async fn the_child_metadata_carries_the_call_position_as_1_12s_did() {
        let probe = chan(
            "probe",
            json!([{"id": "m", "name": "m", "function": {"name": "map", "input": {"mappings": [
                {"path": "data.depth", "logic": {"var": "metadata._orion_call_depth"}},
                {"path": "data.chain", "logic": {"var": "metadata._orion_call_chain"}},
                {"path": "data.channel", "logic": {"var": "metadata.channel"}},
                {"path": "data.header", "logic": {"var": "metadata.headers.x-trace"}},
                {"path": "data.vars", "logic": {"var": "metadata.vars"}}
            ]}}}]),
        );
        let chans = [
            probe,
            chan(
                "caller",
                call(json!({"channel": "probe", "output": "data.seen"})),
            ),
        ];
        for metadata in [
            json!({}),
            json!({"headers": {"x-trace": "abc"}, "vars": {"region": "eu"}}),
            json!({"_orion_call_depth": 2, "_orion_call_chain": ["edge", "mid"]}),
            json!({"_orion_call_depth": "two", "_orion_call_chain": "edge"}),
        ] {
            let (v3, v4) = both(&chans, "caller", json!({}), metadata.clone()).await;
            assert_eq!(v4, v3, "{metadata}");
            assert!(v4.is_ok(), "{metadata}");
        }
        let (_, v4) = both(
            &chans,
            "caller",
            json!({}),
            json!({"_orion_call_depth": 2, "_orion_call_chain": ["edge"]}),
        )
        .await;
        assert_eq!(
            v4.expect("answers")["seen"],
            json!({"depth": 3, "chain": ["edge", "probe"], "channel": "probe"})
        );
    }

    /// The recursion guards refuse at the call they fire in, in 1.12's
    /// words: a self-call, a chain that comes back, and a depth past the
    /// maximum, each on the first hop (an injected position, as
    /// `channel_call_test` injects one).
    #[tokio::test]
    async fn the_recursion_guards_refuse_as_1_12s_did() {
        let chans = [
            chan("self", call(json!({"channel": "self"}))),
            chan("caller", call(json!({"channel": "target"}))),
            greeter(),
        ];
        for (channel, metadata) in [
            ("caller", json!({"_orion_call_chain": ["edge", "target"]})),
            ("caller", json!({"_orion_call_depth": MAX_DEPTH})),
            (
                "caller",
                json!({"_orion_call_depth": MAX_DEPTH + 5, "_orion_call_chain": ["a"]}),
            ),
        ] {
            let (v3, v4) = both(&chans, channel, json!({}), metadata.clone()).await;
            assert_eq!(v4, v3, "{metadata}");
            assert_eq!(v4.expect_err("refused").code, "VALIDATION_ERROR");
        }
    }

    /// A refusal one hop down is a stopped child, so it is passed on with
    /// its own code and retryability (I184): a self-call is refused in the
    /// child as a cycle, `VALIDATION_ERROR`, which the caller now records
    /// under 1.12's prefix, where 1.12 rewrapped it as `FUNCTION_ERROR`.
    #[tokio::test]
    async fn a_cycle_found_in_the_child_is_passed_on_with_its_code() {
        let chans = [chan("self", call(json!({"channel": "self"})))];
        let (v3, v4) = both(&chans, "self", json!({}), json!({})).await;
        let (v3, v4) = (v3.expect_err("refused"), v4.expect_err("refused"));
        // The child's refusal, as the child's run stopped with it: the
        // same call at the child's position.
        let inner = world(&chans)
            .await
            .v4(
                "self",
                json!({}),
                json!({"_orion_call_depth": 1, "_orion_call_chain": ["self"]}),
            )
            .await
            .expect_err("the child refuses the cycle");
        assert_eq!(inner.code, "VALIDATION_ERROR");
        assert_eq!(
            v4,
            Failure {
                code: inner.code.clone(),
                message: format!("channel_call to 'self' failed: {}", inner.message),
                retryable: inner.retryable,
                detail: inner.detail.clone(),
            }
        );
        // What moved: 1.12 recorded the same text as a function failure.
        assert_eq!(v3.code, "FUNCTION_ERROR");
        assert_eq!(
            v3.message,
            format!("Function execution error: {}", v4.message)
        );
    }

    /// A target the registry cannot serve is refused as 1.12 refused it,
    /// before the target is guarded or run: one not found (not a channel,
    /// or not active), one quarantined, and a cron channel.
    #[tokio::test]
    async fn a_target_the_estate_cannot_serve_is_refused_as_1_12_refused_it() {
        let chans = [
            chan(
                "caller",
                json!([
                    {"id": "p", "name": "p", "function": {"name": "parse_json",
                        "input": {"source": "payload", "target": "to"}}},
                    {"id": "t", "name": "t", "function": {"name": NAME,
                        "input": {"channel": {"var": "data.to"}}}}
                ]),
            ),
            greeter().cron().named("nightly"),
            greeter().named("broken"),
        ];
        let knobs = || Knobs {
            quarantined: vec!["broken"],
            ..Knobs::default()
        };
        for to in ["nowhere", "nightly", "broken"] {
            let v3 = world_with(&chans, knobs())
                .await
                .v3("caller", json!(to), json!({}))
                .await;
            let world = world_with(&chans, knobs()).await;
            let v4 = world.v4("caller", json!(to), json!({})).await;
            assert_eq!(v4, v3, "{to}");
            assert_eq!(v4.expect_err("refused").code, "FUNCTION_ERROR", "{to}");
            assert_eq!(world.runs.channels(), ["caller"], "{to}: nothing ran");
        }
    }

    /// Zero matches are read from the run. A target whose workflows were
    /// considered and all declined answers as 1.12's did, with the child's
    /// empty `data`; a target the engine has no workflow for at all is
    /// refused as not found, where 1.12, which read zero matches from the
    /// registry, would have written that empty `data` (a generation built
    /// from one set of rows has no such target).
    #[tokio::test]
    async fn zero_matches_are_read_from_the_run() {
        let declines = greeter().condition(json!(false));
        let chans = [
            declines,
            chan(
                "caller",
                call(json!({"channel": "target", "output": "data.r"})),
            ),
        ];
        let (v3, v4) = both(&chans, "caller", json!({"name": "x"}), json!({})).await;
        assert_eq!(v4, v3);
        assert_eq!(v4.expect("answers"), json!({"r": {}}));

        let chans = [
            greeter().unserved(),
            chan(
                "caller",
                call(json!({"channel": "target", "output": "data.r"})),
            ),
        ];
        let world = world(&chans).await;
        let failure = world
            .v4("caller", json!({}), json!({}))
            .await
            .expect_err("refused");
        let expected: DataflowError = not_found("target").into();
        assert_eq!(failure.code, "FUNCTION_ERROR");
        assert_eq!(failure.message, expected.to_string());
        assert_eq!(world.runs.channels(), ["caller", "target"]);
        // What 1.12 did with the same estate: the empty `data`, written.
        let v3 = world.v3("caller", json!({}), json!({})).await;
        assert_eq!(v3.expect("1.12 answered"), json!({"r": {}}));
    }

    /// The target's guards admit or refuse as 1.12's did: its
    /// `validation_logic` sees the call's data and the child's metadata,
    /// a rejection is the caller's `VALIDATION_ERROR`, and a rate limit's
    /// refusal keeps its status (`CHANNEL_RATE_LIMITED`, retryable).
    #[tokio::test]
    async fn the_targets_guards_admit_and_refuse_as_1_12s_did() {
        let guarded = greeter().named("guarded").config(json!({
            "validation_logic": {"and": [
                {"==": [{"var": "data.name"}, "Ada"]},
                {"==": [{"var": "metadata.channel"}, "guarded"]},
                {"==": [{"var": "metadata._orion_call_depth"}, 1]}
            ]}
        }));
        let limited = greeter()
            .named("limited")
            .config(json!({"rate_limit": {"requests_per_second": 1, "burst": 1}}));
        let caller = chan(
            "caller",
            json!([
                {"id": "a", "name": "a", "function": {"name": NAME,
                    "input": {"channel": {"var": "data.in.to"}, "data": {"var": "data.in.send"},
                              "output": "data.first"}}},
                {"id": "b", "name": "b", "function": {"name": NAME,
                    "input": {"channel": {"var": "data.in.to"}, "data": {"var": "data.in.send"},
                              "output": "data.second"}}}
            ]),
        );
        let caller = Chan {
            tasks: caller.tasks.map(|tasks| {
                let mut all = vec![
                    json!({"id": "p", "name": "p", "function": {"name": "parse_json",
                    "input": {"source": "payload", "target": "in"}}}),
                ];
                all.extend(tasks.as_array().cloned().unwrap_or_default());
                Value::Array(all)
            }),
            ..caller
        };
        let chans = [guarded, limited, caller];
        for (payload, code) in [
            (json!({"to": "guarded", "send": {"name": "Ada"}}), None),
            (
                json!({"to": "guarded", "send": {"name": "Bob"}}),
                Some("VALIDATION_ERROR"),
            ),
            (
                json!({"to": "limited", "send": {"name": "Ada"}}),
                Some(crate::errors::kind::CHANNEL_RATE_LIMITED),
            ),
        ] {
            let (v3, v4) = both(&chans, "caller", payload.clone(), json!({})).await;
            assert_eq!(v4, v3, "{payload}");
            match code {
                None => {
                    let data = v4.expect("admitted");
                    assert_eq!(data["first"]["greeting"], "Hello, Ada");
                    assert_eq!(data["second"]["greeting"], "Hello, Ada");
                }
                Some(code) => assert_eq!(v4.expect_err("refused").code, code, "{payload}"),
            }
        }
        let (_, v4) = both(
            &chans,
            "caller",
            json!({"to": "limited", "send": {"name": "Ada"}}),
            json!({}),
        )
        .await;
        assert!(
            v4.expect_err("the second call is over the limit").retryable,
            "a rate limit's refusal is retryable"
        );
    }

    /// What the message makes of `channel`, `data` and `timeout_ms` fails
    /// as 1.12 failed it: a name that is not a string, an empty one, an
    /// expression that throws, and a timeout that is not a number.
    #[tokio::test]
    async fn computed_fields_fail_as_1_12s_did() {
        for input in [
            json!({"channel": {"var": "data.in.n"}}),
            json!({"channel": {"var": "data.in.empty"}}),
            json!({"channel": {"var": "data.in.missing"}}),
            json!({"channel": {"throw": "boom"}}),
            json!({"channel": "target", "data": {"throw": "boom"}}),
            json!({"channel": "target", "timeout_ms": "soon"}),
            json!({"channel": "target", "timeout_ms": -1}),
            json!({"channel": "target", "timeout_ms": {"var": "data.in.empty"}}),
        ] {
            let caller = chan(
                "caller",
                json!([
                    {"id": "p", "name": "p", "function": {"name": "parse_json",
                        "input": {"source": "payload", "target": "in"}}},
                    {"id": "t", "name": "t", "function": {"name": NAME, "input": input.clone()}}
                ]),
            );
            let chans = [greeter(), caller];
            let (v3, v4) = both(&chans, "caller", json!({"n": 7, "empty": ""}), json!({})).await;
            assert_eq!(v4, v3, "{input}");
            assert!(v4.is_err(), "{input}");
        }
    }

    /// A computed `output` is resolved before the target is guarded or run
    /// (O3's ruling): one naming no path is refused and the target never
    /// runs. 1.12 ran the target and wrote at a key no response shows.
    #[tokio::test]
    async fn a_computed_output_naming_no_path_runs_nothing() {
        for (output, expected) in [
            (
                json!({"var": "data.in.n"}),
                Some(twin::output_refusal(json!(7))),
            ),
            (json!({"var": "data.in.nowhere"}), None),
            (json!({"var": "data.in.where"}), None),
        ] {
            let caller = chan(
                "caller",
                json!([
                    {"id": "p", "name": "p", "function": {"name": "parse_json",
                        "input": {"source": "payload", "target": "in"}}},
                    {"id": "t", "name": "t", "function": {"name": NAME,
                        "input": {"channel": "target", "output": output.clone()}}}
                ]),
            );
            let world = world(&[greeter(), caller]).await;
            let failure = world
                .v4("caller", json!({"n": 7, "where": "result"}), json!({}))
                .await
                .expect_err("refused");
            assert_eq!(failure.code, "VALIDATION_ERROR", "{output}");
            if let Some(expected) = expected {
                assert_eq!(failure, expected, "{output}");
            }
            assert_eq!(
                world.runs.channels(),
                ["caller"],
                "{output}: the target never ran"
            );
        }
    }

    /// A target that completes with error records fails the call as 1.12's
    /// `WorkflowErrors` did: `FUNCTION_ERROR`, the records summarised.
    #[tokio::test]
    async fn a_target_that_completes_with_errors_fails_the_call_as_1_12s_did() {
        let failing = chan(
            "failing",
            json!([{"id": "boom", "name": "boom", "continue_on_error": true,
                    "function": {"name": "fail", "input": {}}}]),
        );
        let chans = [failing, chan("caller", call(json!({"channel": "failing"})))];
        let (v3, v4) = both(&chans, "caller", json!({}), json!({})).await;
        assert_eq!(v4, v3);
        assert_eq!(v4.expect_err("failed").code, "FUNCTION_ERROR");
    }

    /// I184's named change: a target whose run stopped passes its error on,
    /// with its code and its retryability, under 1.12's "failed" prefix.
    /// 1.12 rewrapped it as a `FUNCTION_ERROR`, not retryable.
    #[tokio::test]
    async fn a_stopped_child_passes_its_error_on_with_its_code_and_retryability() {
        let failing = chan(
            "failing",
            json!([{"id": "boom", "name": "boom", "function": {"name": "fail", "input": {}}}]),
        );
        let chans = [failing, chan("caller", call(json!({"channel": "failing"})))];
        let world = world(&chans).await;
        let inner = world
            .v4("failing", json!({}), json!({}))
            .await
            .expect_err("the target stops");
        assert_eq!((inner.code.as_str(), inner.retryable), ("IO_ERROR", true));

        let (v3, v4) = both(&chans, "caller", json!({}), json!({})).await;
        let (v3, v4) = (v3.expect_err("failed"), v4.expect_err("failed"));
        assert_eq!(
            v4,
            Failure {
                code: inner.code.clone(),
                message: format!("channel_call to 'failing' failed: {}", inner.message),
                retryable: inner.retryable,
                detail: inner.detail.clone(),
            }
        );
        assert_eq!((v3.code.as_str(), v3.retryable), ("FUNCTION_ERROR", false));
        assert_eq!(
            v3.message,
            format!("Function execution error: {}", v4.message)
        );
    }

    /// Break 20: the child's run resets its own error context, so a child
    /// sees none of its caller's failure records, as 1.12's
    /// `clear_error_context` made sure; the caller's records stay its own.
    #[tokio::test]
    async fn the_childs_run_resets_its_own_error_context() {
        let probe = chan(
            "probe",
            json!([{"id": "m", "name": "m", "function": {"name": "map", "input": {"mappings": [
                {"path": "data.inherited",
                 "logic": {"if": [{"var": "metadata._orion_errors"}, "inherited", "none"]}}
            ]}}}]),
        );
        let caller = chan(
            "caller",
            json!([
                {"id": "boom", "name": "boom", "continue_on_error": true,
                 "function": {"name": "fail", "input": {}}},
                {"id": "t", "name": "t", "function": {"name": NAME,
                    "input": {"channel": "probe", "output": "temp_data.seen"}}},
                {"id": "m", "name": "m", "function": {"name": "map", "input": {"mappings": [
                    {"path": "data.seen", "logic": {"var": "temp_data.seen"}},
                    {"path": "data.own", "logic": {"var": "metadata._orion_errors.0.task_id"}}
                ]}}}
            ]),
        );
        let world = world(&[probe, caller]).await;
        let (run, message) = world
            .v4_run(
                "caller",
                json!({}),
                json!({"_orion_errors": [{"task_id": "forged"}]}),
                RunOptions::default(),
            )
            .await;
        assert!(
            matches!(run.status(), RunStatus::CompletedWithErrors),
            "{run:?}"
        );
        assert_eq!(
            Value::from(message.context().data()),
            json!({"seen": {"inherited": "none"}, "own": "boom"})
        );
    }

    /// The target's guards see none of the caller's error records, as 1.12
    /// cleared them before the guards (#280): a caller with a recorded
    /// failure under `continue` calls a target whose `validation_logic`
    /// rejects any message carrying one, and is admitted. On both engines.
    #[tokio::test]
    async fn the_targets_guards_see_none_of_the_callers_error_records() {
        let strict = greeter().named("strict").config(json!({
            "validation_logic": {"!": [{"var": "metadata._orion_errors"}]}
        }));
        let caller = chan(
            "caller",
            json!([
                {"id": "boom", "name": "boom", "continue_on_error": true,
                 "function": {"name": "fail", "input": {}}},
                {"id": "seen", "name": "seen", "function": {"name": "map", "input": {"mappings": [
                    {"path": "data.own", "logic": {"var": "metadata._orion_errors.0.task_id"}}
                ]}}},
                {"id": "t", "name": "t", "function": {"name": NAME,
                    "input": {"channel": "strict", "output": "data.r"}}}
            ]),
        );
        let chans = [strict, caller];
        let (v3, v4) = both(&chans, "caller", json!({"name": "Ada"}), json!({})).await;
        assert_eq!(v4, v3);
        let data = v4.expect("admitted");
        assert_eq!(data["own"], "boom", "the caller holds its own record");
        assert_eq!(data["r"]["greeting"], "Hello, Ada");
    }

    /// O25: the payload, the metadata and the result cross as values, with
    /// no JSON between them, so a tensor in the caller's payload or
    /// metadata reaches the target, and comes back, as a tensor. 1.12's
    /// JSON bridge turned each into an object.
    #[tokio::test]
    async fn values_cross_the_call_with_no_json_round_trip() {
        let tensor = || {
            OwnedDataValue::tensor(
                dataflow_rs::datavalue::OwnedDataTensor::from_slice::<f32>(vec![2], &[1.5, 2.5])
                    .expect("a tensor"),
            )
        };
        let target = chan(
            "target",
            json!([
                {"id": "p", "name": "p", "function": {"name": "parse_json",
                    "input": {"source": "payload", "target": "sent"}}},
                {"id": "m", "name": "m", "function": {"name": "map", "input": {"mappings": [
                    {"path": "data.meta", "logic": {"var": "metadata.t"}}
                ]}}}
            ]),
        );
        let world = world(&[
            target,
            chan(
                "caller",
                call(json!({"channel": "target", "output": "data.r"})),
            ),
        ])
        .await;
        let payload = tensor();
        let metadata = OwnedDataValue::Object(vec![("t".to_string(), tensor())]);
        let mut message = Message::from_parts(payload, OwnedDataValue::Null, metadata);
        let run = world
            .v4
            .run_with(&mut message, &RunOptions::default().channel("caller"))
            .await;
        assert!(matches!(run.status(), RunStatus::Completed), "{run:?}");
        let r = message
            .context()
            .get(&"data.r".parse().expect("a path"))
            .expect("the result is written");
        assert_eq!(r.get("sent"), Some(&tensor()));
        assert_eq!(r.get("meta"), Some(&tensor()));
        assert!(r.get("sent").is_some_and(OwnedDataValue::is_tensor));
    }

    // -- The run's limits, and the call's own --

    /// A world whose caller calls `target`, which waits, with the call's
    /// own timeout `timeout_ms` when set.
    async fn waiting(timeout_ms: Option<u64>, knobs: Knobs) -> World {
        let mut input = json!({"channel": "target"});
        if let Some(ms) = timeout_ms {
            input["timeout_ms"] = json!(ms);
        }
        let target = chan(
            "target",
            json!([{"id": "w", "name": "w", "function": {"name": "wait", "input": {}}}]),
        );
        world_with(&[target, chan("caller", call(input))], knobs).await
    }

    /// The run's deadline coming first, or with the call's own (a tie is
    /// the run's): the child reports `TimedOut` with its run, and the
    /// parent run ends `TimedOut` too, its task recording the run's
    /// deadline, not a failure of the target.
    #[tokio::test(start_paused = true)]
    async fn the_runs_deadline_during_the_child_ends_the_run_timed_out() {
        for (own, run) in [(10_000, 100), (100, 100), (30_000, 250)] {
            let world = waiting(Some(own), Knobs::default()).await;
            let deadline = tokio::time::Instant::now().into_std() + Duration::from_millis(run);
            let (report, message) = world
                .v4_run(
                    "caller",
                    json!({}),
                    json!({}),
                    RunOptions::default().deadline(deadline),
                )
                .await;
            assert!(
                matches!(report.status(), RunStatus::TimedOut),
                "{own}/{run}: {report:?}"
            );
            assert_eq!(
                world.runs.finished(),
                [
                    ("target".to_string(), "timed_out".to_string()),
                    ("caller".to_string(), "timed_out".to_string()),
                ],
                "{own}/{run}"
            );
            let record = message.errors().last().expect("the call is recorded");
            assert_eq!(record.code, "TIMEOUT_ERROR");
            assert_eq!(
                record.message,
                "Task t error: Timeout error: the run's deadline passed during `channel_call`"
            );
            assert!(
                message
                    .context()
                    .data()
                    .as_object()
                    .is_some_and(<[_]>::is_empty)
            );
        }
    }

    /// The run's token tripped while the child runs trips the child's
    /// (a child token): the child reports `Cancelled`, and the parent run
    /// ends `Cancelled` too.
    #[tokio::test(start_paused = true)]
    async fn the_runs_token_tripped_during_the_child_ends_the_run_cancelled() {
        let token = CancelToken::new();
        let target = chan(
            "target",
            json!([{"id": "trip", "name": "trip", "function": {"name": "trip", "input": {}}}]),
        );
        let world = world_with(
            &[target, chan("caller", call(json!({"channel": "target"})))],
            Knobs {
                trip: token.clone(),
                ..Knobs::default()
            },
        )
        .await;
        let far = tokio::time::Instant::now().into_std() + Duration::from_secs(60);
        let (report, message) = world
            .v4_run(
                "caller",
                json!({}),
                json!({}),
                RunOptions::default().cancel(&token).deadline(far),
            )
            .await;
        assert!(
            matches!(report.status(), RunStatus::Cancelled),
            "{report:?}"
        );
        assert_eq!(
            world.runs.finished(),
            [
                ("target".to_string(), "cancelled".to_string()),
                ("caller".to_string(), "cancelled".to_string()),
            ]
        );
        let record = message.errors().last().expect("the call is recorded");
        assert_eq!(record.code, "CANCELLED");
        assert_eq!(
            record.message,
            "Task t error: the run was cancelled during `channel_call`"
        );
    }

    /// A child that is handed no token of its own when the run has none,
    /// and the run's token, untripped, leaves the call alone.
    #[tokio::test(start_paused = true)]
    async fn an_untripped_token_and_a_far_deadline_change_nothing() {
        let chans = [
            greeter(),
            chan(
                "caller",
                call(json!({"channel": "target", "output": "data.r"})),
            ),
        ];
        let world = world(&chans).await;
        let token = CancelToken::new();
        let far = tokio::time::Instant::now().into_std() + Duration::from_secs(60);
        let (report, message) = world
            .v4_run(
                "caller",
                json!({"name": "Ada"}),
                json!({}),
                RunOptions::default().cancel(&token).deadline(far),
            )
            .await;
        assert_eq!(
            seen(&report, &message).expect("answers")["r"]["greeting"],
            "Hello, Ada"
        );
    }

    /// The call's own bound coming first (the task's `timeout_ms`, else
    /// the target's, else the engine default), strictly before the run's
    /// deadline or with none: the child reports `TimedOut`, and the call
    /// fails with 1.12's `TIMEOUT_ERROR`, retryable, in 1.12's words, which
    /// stops the parent run as any failure of its task does.
    #[tokio::test(start_paused = true)]
    async fn the_calls_own_timeout_first_fails_the_call_with_1_12s_timeout() {
        let cases: [(Option<u64>, Value, u64, Option<u64>); 4] = [
            (Some(100), json!({}), 30_000, None),
            (Some(100), json!({}), 30_000, Some(5_000)),
            (None, json!({"timeout_ms": 150}), 30_000, Some(5_000)),
            (None, json!({}), 200, None),
        ];
        for (task, config, default, run) in cases {
            let knobs = || Knobs {
                default_timeout_ms: default,
                ..Knobs::default()
            };
            let mut input = json!({"channel": "target"});
            if let Some(ms) = task {
                input["timeout_ms"] = json!(ms);
            }
            let target = chan(
                "target",
                json!([{"id": "w", "name": "w", "function": {"name": "wait", "input": {}}}]),
            )
            .config(config.clone());
            let chans = [target, chan("caller", call(input))];
            let v3 = world_with(&chans, knobs())
                .await
                .v3("caller", json!({}), json!({}))
                .await;
            let world = world_with(&chans, knobs()).await;
            assert_eq!(world.default_timeout_ms, default);
            let mut options = RunOptions::default();
            if let Some(ms) = run {
                options = options
                    .deadline(tokio::time::Instant::now().into_std() + Duration::from_millis(ms));
            }
            let (report, message) = world.v4_run("caller", json!({}), json!({}), options).await;
            let v4 = seen(&report, &message);
            assert_eq!(v4, v3, "{task:?} {config} {default}");
            let failure = v4.expect_err("the call timed out");
            assert_eq!(failure.code, "TIMEOUT_ERROR");
            assert!(failure.retryable);
            assert_eq!(
                world.runs.finished(),
                [
                    ("target".to_string(), "timed_out".to_string()),
                    ("caller".to_string(), "stopped TIMEOUT_ERROR".to_string()),
                ]
            );
        }
    }

    /// The run's token tripped from another task while the child runs: the
    /// child, holding a child of that token, reports `Cancelled`, the call
    /// passes its error on, and the engine ends the parent run `Cancelled`
    /// on its own token, recording the token's text on the call's task.
    #[tokio::test(start_paused = true)]
    async fn the_runs_token_tripped_from_another_task_ends_the_run_cancelled() {
        let world = waiting(None, Knobs::default()).await;
        let token = CancelToken::new();
        let canceller = {
            let token = token.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                token.cancel();
            })
        };
        let (report, message) = world
            .v4_run(
                "caller",
                json!({}),
                json!({}),
                RunOptions::default().cancel(&token),
            )
            .await;
        canceller.await.expect("the canceller ran");
        assert!(
            matches!(report.status(), RunStatus::Cancelled),
            "{report:?}"
        );
        assert_eq!(
            world.runs.finished(),
            [
                ("target".to_string(), "cancelled".to_string()),
                ("caller".to_string(), "cancelled".to_string()),
            ]
        );
        let record = message.errors().last().expect("the call is recorded");
        assert_eq!(record.code, "CANCELLED");
        assert_eq!(
            record.message,
            "Task t error: the run was cancelled during `channel_call`"
        );
    }

    /// A chain of calls, `caller` to `mid` to `target`: the run's deadline
    /// passing in the grandchild ends every run on the chain `TimedOut`,
    /// innermost first, and the run's token tripped there ends every one
    /// `Cancelled`: each call hands the limit down and passes its child's
    /// error on, and the engine ends each run on its own limit, since the
    /// error comes after it (dataflow-rs `daaf8c0`).
    #[tokio::test(start_paused = true)]
    async fn the_runs_limits_end_every_run_on_a_chain() {
        for trip in [false, true] {
            let token = CancelToken::new();
            let last = if trip { "trip" } else { "wait" };
            let chans = [
                chan(
                    "target",
                    json!([{"id": "w", "name": "w", "function": {"name": last, "input": {}}}]),
                ),
                chan("mid", call(json!({"channel": "target"}))),
                chan("caller", call(json!({"channel": "mid"}))),
            ];
            let world = world_with(
                &chans,
                Knobs {
                    trip: token.clone(),
                    ..Knobs::default()
                },
            )
            .await;
            let deadline = tokio::time::Instant::now().into_std() + Duration::from_millis(100);
            let (report, _) = world
                .v4_run(
                    "caller",
                    json!({}),
                    json!({}),
                    RunOptions::default().deadline(deadline).cancel(&token),
                )
                .await;
            let status = if trip { "cancelled" } else { "timed_out" };
            assert!(
                matches!(
                    (report.status(), trip),
                    (RunStatus::Cancelled, true) | (RunStatus::TimedOut, false)
                ),
                "{report:?}"
            );
            assert_eq!(
                world.runs.finished(),
                ["target", "mid", "caller"].map(|c| (c.to_string(), status.to_string())),
            );
        }
    }

    /// A call's own timeout in the middle of a chain is that call's
    /// failure, not the run's: `mid`'s call to `target` times out on its
    /// own 100 ms, well inside the run's deadline, so `mid` stops with 1.12's
    /// `TIMEOUT_ERROR`, and `caller`'s call passes that on with its code
    /// and retryability (I184), where 1.12 rewrapped it as a
    /// `FUNCTION_ERROR`.
    #[tokio::test(start_paused = true)]
    async fn a_nested_calls_own_timeout_is_passed_on_with_its_code() {
        let chans = [
            chan(
                "target",
                json!([{"id": "w", "name": "w", "function": {"name": "wait", "input": {}}}]),
            ),
            chan("mid", call(json!({"channel": "target", "timeout_ms": 100}))),
            chan("caller", call(json!({"channel": "mid"}))),
        ];
        let v3 = world(&chans)
            .await
            .v3("caller", json!({}), json!({}))
            .await
            .expect_err("1.12 failed the call");
        let world = world(&chans).await;
        let deadline = tokio::time::Instant::now().into_std() + Duration::from_secs(10);
        let (report, message) = world
            .v4_run(
                "caller",
                json!({}),
                json!({}),
                RunOptions::default().deadline(deadline),
            )
            .await;
        let v4 = seen(&report, &message).expect_err("the call failed");
        let inner: DataflowError = timed_out("target", 100).into();
        assert_eq!(
            v4,
            Failure {
                code: "TIMEOUT_ERROR".to_string(),
                message: format!("channel_call to 'mid' failed: {inner}"),
                retryable: true,
                detail: None,
            }
        );
        assert_eq!(
            world.runs.finished(),
            [
                ("target".to_string(), "timed_out".to_string()),
                ("mid".to_string(), "stopped TIMEOUT_ERROR".to_string()),
                ("caller".to_string(), "stopped TIMEOUT_ERROR".to_string()),
            ]
        );
        assert_eq!((v3.code.as_str(), v3.retryable), ("FUNCTION_ERROR", false));
        assert_eq!(
            v3.message,
            format!("Function execution error: {}", v4.message)
        );
    }

    /// The child's deadline is the earlier bound, the run's on a tie, and
    /// an own bound past the clock's range is no bound.
    #[test]
    fn the_childs_deadline_is_the_earlier_bound_and_a_tie_is_the_runs() {
        let now = std::time::Instant::now();
        let soon = now + Duration::from_millis(10);
        let late = now + Duration::from_millis(20);
        let limits = |own, run| ChildLimits::new(own, run);
        assert_eq!(
            limits(Some(soon), Some(late)),
            ChildLimits {
                deadline: Some(soon),
                own: true
            }
        );
        assert_eq!(
            limits(Some(late), Some(soon)),
            ChildLimits {
                deadline: Some(soon),
                own: false
            }
        );
        assert_eq!(
            limits(Some(soon), Some(soon)),
            ChildLimits {
                deadline: Some(soon),
                own: false
            }
        );
        assert_eq!(
            limits(Some(soon), None),
            ChildLimits {
                deadline: Some(soon),
                own: true
            }
        );
        assert_eq!(
            limits(None, Some(late)),
            ChildLimits {
                deadline: Some(late),
                own: false
            }
        );
        assert_eq!(
            limits(None, None),
            ChildLimits {
                deadline: None,
                own: false
            }
        );
        assert!(own_deadline(1).is_some());
    }

    /// The function is registered under its name by `register_functions`,
    /// and a call through `function::testing` with no generation to run on
    /// refuses as a target nothing serves.
    #[tokio::test]
    async fn a_call_through_testing_refuses_a_target_the_boot_generation_lacks() {
        let called = testing::call(
            ChannelCallFunction {
                runtime: crate::runtime::generation::test_handle(),
                exprs: kit::test_exprs(),
                max_call_depth: MAX_DEPTH,
                default_timeout_ms: 30_000,
            },
            json!({"channel": "orders"}),
        )
        .exprs(kit::test_exprs())
        .name(NAME)
        .await;
        let failure = twin::seen(&called).expect_err("refused");
        let expected: DataflowError = not_found("orders").into();
        assert_eq!(failure.message, expected.to_string());
    }

    #[tokio::test]
    async fn the_child_inherits_the_runs_accountant_without_resetting_budgets() {
        let chans = [
            greeter(),
            chan(
                "caller",
                call(json!({"channel": "target", "output": "data.result"})),
            ),
        ];
        let w = world(&chans).await;

        // 1. Unbounded SharedRunAccountant counts tasks and expression ops across
        //    both parent (`caller`: 1 task) and child (`target`: 2 tasks = 3 total).
        let acct = crate::engine::SharedRunAccountant::unbounded();
        let (run, msg) = w
            .v4_run(
                "caller",
                json!({"name": "World"}),
                json!({}),
                acct.apply(RunOptions::default()),
            )
            .await;
        assert_eq!(
            seen(&run, &msg).expect("succeeds")["result"]["greeting"],
            "Hello, World"
        );
        let snap = acct.snapshot();
        assert_eq!(
            snap.tasks_dispatched, 3,
            "parent (1 task) + child (2 tasks) share the same accountant"
        );
        assert!(
            snap.expr_ops_charged > 0,
            "child expression ops are reported to the shared accountant"
        );
        assert_eq!(snap.in_flight, 0);

        // 2. A root task ceiling of 2 allows the parent task (`channel_call`) and
        //    the child's first task (`t0`), then stops before the child's second
        //    task (`t1`) can run.
        let capped = crate::engine::SharedRunAccountant::new(crate::engine::RunBudgetLimits {
            max_tasks: Some(2),
            ..Default::default()
        });
        let (capped_run, capped_msg) = w
            .v4_run(
                "caller",
                json!({"name": "World"}),
                json!({}),
                capped.apply(RunOptions::default()),
            )
            .await;
        let err =
            seen(&capped_run, &capped_msg).expect_err("child task exceeds shared task ceiling");
        assert_eq!(err.code, "BUDGET_EXCEEDED");
        assert_eq!(capped.snapshot().tasks_dispatched, 3);
    }
}
