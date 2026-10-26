//! `channel_call` — run another channel's workflows in-process, under that
//! channel's ingress guards, and write what they left in `data`.
//!
//! [`ChannelCallFunction`] passes the run's deadline and cancellation on to
//! the child it starts (R60) and a stopped child's error on with its code
//! (I184). It reads the call position, judges the target and words every
//! refusal through the helpers below, in 1.12's order and words.

use super::definition::{Definition, FieldOverlay, NoConnector, NullOutput, OrionFunction};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use dataflow_rs::Path;
use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{CompileCx, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec};
use dataflow_rs::program::IssueCode;
use dataflow_rs::run::{AuditMode, CancelToken, Instant, RunStatus};
use dataflow_rs::{Ctx, Expr, Function, Message, Outcome, RunOptions};
use serde_json::Value;

use super::schema::{FieldKind, RetrySafety};
#[cfg(test)]
use crate::kit;
use crate::kit::read;
use crate::{ErrorClass, HandlerError};

/// This function's name: the one place it is written (F48).
pub const NAME: &str = "channel_call";

/// The refusal of a task that names no target.
const CHANNEL_REQUIRED: &str = "channel_call requires 'channel'";

/// Metadata key for current call depth.
const META_CALL_DEPTH: &str = "_orion_call_depth";
/// Metadata key for the call chain (array of channel names).
const META_CALL_CHAIN: &str = "_orion_call_chain";

/// The rule on the authored JSON:
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

/// Format a call chain for error messages: "A -> B -> C"
fn format_chain(chain: &[String], target: &str) -> String {
    let mut parts: Vec<&str> = chain.iter().map(|s| s.as_str()).collect();
    parts.push(target);
    parts.join(" -> ")
}

// -- The node it dispatches into --

/// The node a `channel_call` dispatches into: which channels it serves, under
/// which guards, on which engine. The engine knows none of that; the serving
/// runtime implements this over its published generation.
pub trait ChannelCallHost: Send + Sync {
    /// `target`, from one generation, when it may be called: refused when no
    /// deployed channel has that name ([`not_found`]), or it is quarantined,
    /// inactive or a cron channel.
    fn resolve(&self, target: &str) -> Result<Box<dyn ChannelCallTarget>, HandlerError>;
}

/// One resolved target: its guards, and the engine that runs it, both from the
/// generation [`ChannelCallHost::resolve`] read.
#[async_trait::async_trait]
pub trait ChannelCallTarget: Send + Sync {
    /// The target's ingress guards, applied to the call as the
    /// `channel_call` transport: a refusal is already worded for the caller
    /// ([`guard_refused`]).
    async fn admit(&self, request: ChildRequest<'_>) -> dataflow_rs::Result<ChildAdmission>;

    /// The engine the child runs on.
    fn engine(&self) -> &dataflow_rs::Engine;
}

/// What the target's guards read of a call.
pub struct ChildRequest<'a> {
    /// The target channel.
    pub target: &'a str,
    /// The payload the child is handed.
    pub data: &'a OwnedDataValue,
    /// The child's metadata: the caller's, with the call position advanced.
    pub metadata: &'a OwnedDataValue,
    /// The calling channel, which a target's rate limit buckets by.
    pub caller: &'a str,
    /// `engine.default_channel_call_timeout_ms`.
    pub default_timeout_ms: u64,
}

/// What the target's guards granted.
pub struct ChildAdmission {
    /// The target channel's timeout, when it sets one.
    pub timeout_ms: Option<u64>,
    /// Held until the child's run ends: the target's backpressure permit.
    pub permit: Option<Box<dyn std::any::Any + Send>>,
}

// -- The function --

/// The `metadata` root, which a call reads its position and its caller's
/// identity from and hands its child, amended.
static METADATA: LazyLock<Path> =
    LazyLock::new(|| "metadata".parse().expect("`metadata` is a path"));

/// `channel_call`'s input on dataflow-rs 4, its fields in
/// `ChannelCallConfig::fields()`' order (I179), each read as 1.12 read it (I183).
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
    /// Target channel to invoke (JSONLogic), so one task can route by message content.
    channel: Expr<OwnedDataValue>,
    /// Payload to pass to the target channel (JSONLogic). Omit to forward the caller's own payload.
    data: Option<Expr<OwnedDataValue>>,
    /// Dotted path where the called channel's response is stored. Defaults to "data".
    #[output(default = "data")]
    output: Output,
    /// Per-call timeout in milliseconds.
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

/// `channel_call` as a function: the call 1.12's handler made, refused in
/// the same order and the same words, with the child run on the
/// generation's engine.
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
    /// The node, resolved once per call: the target's guards and the engine
    /// that runs it come from the same build.
    pub host: Arc<dyn ChannelCallHost>,
    pub max_call_depth: u32,
    pub default_timeout_ms: u64,
}

impl OrionFunction for ChannelCallFunction {
    type Connector = NoConnector;

    const DEFINITION: Definition = Definition {
        name: NAME,
        description: "Invoke another channel's workflow in-process (no HTTP hop).",
        category: "control",
        retry_safety: RetrySafety::DependsOn { input: "channel" },
        fields: &[
            FieldOverlay {
                name: "channel",
                kind: Some(FieldKind::String),
                ..FieldOverlay::DEFAULT
            },
            FieldOverlay {
                name: "timeout_ms",
                kind: Some(FieldKind::Number),
                ..FieldOverlay::DEFAULT
            },
        ],
        deny_unknown: false,
        null_output: NullOutput::Reject,
        stub_target: Some("channel"),
        ..Definition::DEFAULT
    };

    fn from_deps(deps: &crate::HandlerDeps<'_>) -> Self {
        Self {
            host: deps.channel_calls.clone(),
            max_call_depth: deps.engine_config.max_channel_call_depth,
            default_timeout_ms: deps.engine_config.default_channel_call_timeout_ms,
        }
    }
}

impl Function for ChannelCallFunction {
    type Config = ChannelCallConfig;

    fn spec(&self) -> FunctionSpec {
        super::definition::spec::<Self>()
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
        // engine that runs it below come from the same build (inheriting any
        // pinned generation from the parent run under V2-05.3).
        let resolved = self.host.resolve(&target)?;
        let engine = resolved.engine();
        // Where the answer goes, before the guards spend anything on the
        // call: an output that names no path (`null` included, which 1.12
        // wrote to a key no response shows) runs nothing.
        let output = Self::DEFINITION.output_path(ctx, &cfg.output)?;

        let admission = resolved
            .admit(ChildRequest {
                target: &target,
                data: &data,
                metadata: &metadata,
                caller: &caller,
                default_timeout_ms: self.default_timeout_ms,
            })
            .await?;
        let _backpressure_permit = admission.permit;

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
/// error-context path ([`crate::ERROR_CONTEXT`]), read as the
/// engine reads it, when that path is under `metadata`.
fn strip_error_records(metadata: &mut OwnedDataValue) {
    let path = crate::ERROR_CONTEXT.path();
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

/// 1.12's `guard_refusal` on dataflow-rs 4's error: a `validation_logic`
/// rejection stays the caller's `VALIDATION_ERROR`, and every other refusal
/// keeps the status the guard chose, so the caller sees `429`/`503` rather
/// than a generic `500`.
pub fn guard_refused(target: &str, e: crate::errors::OrionError) -> dataflow_rs::Error {
    let (status, _code, detail) = e.response_parts();
    let message = format!("channel_call to '{target}': {detail}");
    if status == axum::http::StatusCode::BAD_REQUEST {
        HandlerError::new(ErrorClass::CallerInput, message).into()
    } else {
        crate::errors::service::channel_refused(status, message)
    }
}

// -- The call position, the target, and the refusals --
//
// The call position, the target's judgement and every refusal 1.12 words,
// once, as `HandlerError`s of the class 1.12's error mapped to:
// `CallerInput` is 1.12's `Validation`, `Backend` its `function_execution`
// and `Timeout` its `Timeout`, so each records 1.12's code and text (I184).

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
/// looked up. Both are loops in the deployed channels, so they answer
/// `508 LOOP_DETECTED`, and a refusal one hop down keeps that kind as it is
/// passed up the chain.
fn check_position(
    max_depth: u32,
    depth: u64,
    chain: &[String],
    target: &str,
) -> dataflow_rs::Result<()> {
    if depth >= u64::from(max_depth) {
        return Err(crate::errors::service::channel_loop(format!(
            "channel_call: max call depth {max_depth} exceeded (chain: {})",
            format_chain(chain, target),
        )));
    }
    if chain.iter().any(|called| called == target) {
        return Err(crate::errors::service::channel_loop(format!(
            "channel_call: cycle detected: {}",
            format_chain(chain, target)
        )));
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

/// The refusal of a target with no workflow to run it.
pub fn not_found(target: &str) -> HandlerError {
    HandlerError::new(
        ErrorClass::Backend,
        format!("channel_call to '{target}': no deployed channel has that name"),
    )
}

/// The refusal of a target whose run failed: its error records summarised,
/// or the error that stopped it.
fn failed(target: &str, why: impl std::fmt::Display) -> HandlerError {
    HandlerError::new(
        ErrorClass::Backend,
        format!("channel_call to '{target}' failed: {why}"),
    )
}

/// The refusal of a target whose run outlived the call's own timeout.
pub fn timed_out(target: &str, ms: u64) -> HandlerError {
    HandlerError::new(
        ErrorClass::Timeout,
        format!("channel_call to '{target}' timed out after {ms}ms"),
    )
}

#[cfg(test)]
mod tests {
    use dataflow_rs::function::testing;
    use dataflow_rs::program::Severity;
    use serde_json::json;

    use super::*;
    use crate::functions::twin;

    /// `engine.max_channel_call_depth`'s default.
    const MAX_DEPTH: u32 = 10;

    /// Declaration order is the field table's (I179); `channel` is
    /// required (I183); `output` defaults to `data`; `data` and
    /// `timeout_ms` written as `null` are unset, as serde read 1.12's
    /// `Option<Template>`.
    #[test]
    fn the_config_compiles_and_checks_its_inputs() {
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
            let expected = format!(
                "channel_call 'channel' must be a channel name or an expression \
                 producing one, not {channel}"
            );
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
        let expected = "channel_call requires 'channel'".to_string();
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

    /// A call through `function::testing` on a node that serves nothing
    /// refuses as a target nothing serves.
    #[tokio::test]
    async fn a_call_through_testing_refuses_a_target_the_node_lacks() {
        let called = testing::call(
            ChannelCallFunction {
                host: Arc::new(kit::NoChannels),
                max_call_depth: MAX_DEPTH,
                default_timeout_ms: 30_000,
            },
            json!({"channel": "orders"}),
        )
        .exprs(kit::test_exprs())
        .name(NAME)
        .await;
        let failure = twin::seen(&called).expect_err("refused");
        let expected: dataflow_rs::Error = not_found("orders").into();
        assert_eq!(failure.message, expected.to_string());
    }
}
