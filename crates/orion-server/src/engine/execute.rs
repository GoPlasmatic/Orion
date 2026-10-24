//! The one step between "the guards admitted this" and "shape a response".
//!
//! `channel::guards::apply_guards` is the single enforcement point for
//! ingress, and everything after it used to be copied per transport: the sync
//! HTTP route, the async trace queue, the Kafka consume loop and the
//! in-process `channel_call` handler each built a `Message`, loaded the engine
//! snapshot, called [`run_for_channel`], re-derived the "engine returned Ok
//! but the message carries task errors" rule, and mapped a timeout — **four
//! different ways**: an `OrionError::Timeout`, a synthesised
//! `DataflowError::Timeout`, a `fail("timeout", …)`, and another
//! `DataflowError::Timeout` with different wording. Nothing tied the four
//! together, so the next change to dataflow-rs's error semantics was four
//! edits and a test suite that could not tell if one was missed.
//!
//! They had already drifted. Kafka alone omitted the rollout bucket (correctly
//! — a record has no sticky caller identity), and `channel_call` never applied
//! the `has_errors` rule at all, so a target channel whose workflow failed its
//! tasks reported success to its caller.
//!
//! [`execute_admitted`] is that step, once. What stays with each transport is
//! what genuinely differs: persistence, response shaping, and what a
//! [`RunOutcome`] *means* to it — an HTTP caller gets the errors in its
//! envelope, the async path routes them to the DLQ, Kafka rewinds or
//! dead-letters.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::runner::{TraceCapture, run_for_channel};

/// What one admitted execution did.
///
/// The four outcomes are exhaustive over what the engine can report, which is
/// the point: a new dataflow-rs error shape lands here, once, and every
/// transport's `match` fails to compile until it is handled.
#[derive(Debug)]
pub enum RunOutcome {
    /// Every workflow that matched ran, and none reported a task error.
    Ok,
    /// The engine returned `Ok(())` and the message carries task errors — the
    /// v3 contract. Carries the joined `code: message` summary, which is what
    /// all three of the transports that look at it were building by hand.
    WorkflowErrors(String),
    /// The deadline expired, with the budget in milliseconds.
    Timeout(u64),
    /// The engine itself failed the call.
    EngineError(dataflow_rs_v3::DataflowError),
}

impl RunOutcome {
    /// The `status` label for `orion_messages_total`.
    ///
    /// One derivation, so the counter means the same thing on every transport.
    /// It did not: the sync route counted [`Self::WorkflowErrors`] as `ok`
    /// while the Kafka and async paths counted it as `error`, so a channel
    /// whose workflow failed every synchronous request reported a 100% success
    /// rate. `error` is the answer the metric's own documentation implies —
    /// "messages processed, by outcome" — and a workflow that failed its tasks
    /// did not achieve its outcome.
    ///
    /// The label is derived here; *emitting* it stays with the transport,
    /// because whether an execution should be counted at all is knowledge only
    /// the transport has. Kafka suppresses the counter on an in-place retry
    /// (K10 — one poison record otherwise inflated the error rate once a
    /// minute forever), and `channel_call` emits nothing because the request
    /// that triggered it was already counted at its own ingress.
    pub fn status_label(&self) -> &'static str {
        NormalizedRunOutcome::from(self).status_label()
    }

    /// Whether the run produced the result the caller asked for.
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

impl From<&RunOutcome> for NormalizedRunOutcome {
    fn from(outcome: &RunOutcome) -> Self {
        match outcome {
            RunOutcome::Ok => Self::Success,
            RunOutcome::WorkflowErrors(summary) => Self::TaskFailure {
                summary: summary.clone(),
                continued_after_error: false,
            },
            RunOutcome::Timeout(ms) => Self::DeadlineExceeded(*ms),
            RunOutcome::EngineError(err) => Self::InternalFailure(err.to_string()),
        }
    }
}

impl From<NormalizedRunOutcome> for RunOutcome {
    fn from(outcome: NormalizedRunOutcome) -> Self {
        match outcome {
            NormalizedRunOutcome::Success | NormalizedRunOutcome::NoMatch => Self::Ok,
            NormalizedRunOutcome::TaskFailure { summary, .. }
            | NormalizedRunOutcome::Rejected(summary) => Self::WorkflowErrors(summary),
            NormalizedRunOutcome::DeadlineExceeded(ms) => Self::Timeout(ms),
            NormalizedRunOutcome::Cancelled => {
                Self::EngineError(dataflow_rs_v3::DataflowError::Timeout("cancelled".into()))
            }
            NormalizedRunOutcome::Unavailable(msg) | NormalizedRunOutcome::InternalFailure(msg) => {
                Self::EngineError(dataflow_rs_v3::DataflowError::Unknown(msg))
            }
        }
    }
}

/// Everything about one execution that is not the channel or the payload.
#[derive(Default)]
pub struct ExecOpts<'a> {
    /// Deadline for the whole call. `None` runs untimed.
    pub timeout_ms: Option<u64>,
    /// Per-task trace capture, when the channel opted in via
    /// `config.tracing.task_details`. Also decides the message's
    /// `capture_changes`: only a traced run pays for the per-write value copies.
    pub capture: Option<TraceCapture>,
    /// The rollout bucket to stamp on the message, from
    /// [`crate::engine::utils::rollout_bucket_for_identity`].
    ///
    /// `None` means "admitted by every workflow, rollout or not", and is
    /// deliberate on two transports: a Kafka record has no sticky caller
    /// identity and no forwarded address, so a random bucket per record would
    /// split one topic's traffic across canary versions non-deterministically;
    /// an in-process `channel_call` is already running inside its caller's
    /// bucket.
    pub routing_bucket: Option<u8>,
    /// The per-request profile scope, when one is in flight.
    pub profile: Option<&'a Arc<super::profile::ProfileCollector>>,
}

/// One execution and everything a transport needs from it.
pub struct Execution {
    /// The message as the workflows left it — the source of the response body,
    /// the persisted result, and the error list.
    pub message: dataflow_rs_v3::Message,
    /// The per-task trace, when [`ExecOpts::capture`] asked for one.
    pub task_trace: Option<dataflow_rs_v3::ExecutionTrace>,
    pub outcome: RunOutcome,
    /// Wall time of the engine call alone, for the latency histogram.
    pub duration: Duration,
}

/// Run `channel`'s workflows over `data`, after the guards have admitted it.
///
/// `engine` is the one belonging to the generation that admitted the request
/// (`RuntimeGeneration::engine`) — passed in rather than loaded here, so a
/// transport cannot admit against one generation and execute on the next. It
/// used to take the engine *handle* and load it at this line, which is exactly
/// where that mismatch entered.
///
/// Owns the message build, the deadline arm and the `has_errors` rule.
/// Everything downstream — persisting a trace, shaping a response, committing
/// an offset, emitting the counter whose label [`RunOutcome::status_label`]
/// derives — is the caller's.
pub async fn execute_admitted(
    engine: &Arc<dataflow_rs_v3::Engine>,
    channel: &str,
    data: &Value,
    metadata: &Value,
    opts: ExecOpts<'_>,
) -> Execution {
    // Per-write capture and the audit trail are kept only for a traced run.
    // With capture on, every write deep-copies its old and new value into the
    // audit trail, and those copies stay on the message until the run returns
    // — so a looping workflow holds sweeps × writes × value size (#350; about
    // 65 bytes per number written). The entries themselves cost one per task
    // per sweep, and a `for_each` used to copy them into every element (#373).
    //
    // Nothing on this path reads `Message::audit_trail`. The trace's per-step
    // diff does: it is read from each task's own entry, so a traced run keeps
    // them all (`AuditMode::Full`), and `TraceOptions { changes: true }` only
    // reports what was captured, it never turns capture on. An untraced run
    // keeps none (`AuditMode::Off`); status classification, the error records,
    // `metadata.progress` and halting do not depend on the trail.
    let traced = opts.capture.is_some();
    let mut builder = dataflow_rs_v3::Message::builder()
        .payload_json(data)
        .metadata_json(metadata)
        .capture_changes(traced)
        .audit_mode(if traced {
            dataflow_rs_v3::AuditMode::Full
        } else {
            dataflow_rs_v3::AuditMode::Off
        });
    if let Some(bucket) = opts.routing_bucket {
        builder = builder.routing_bucket(bucket);
    }
    let mut message = builder.build();

    let started = Instant::now();
    let call = run_for_channel(
        engine,
        channel,
        &mut message,
        opts.timeout_ms,
        opts.profile,
        opts.capture,
    )
    .await;
    let duration = started.elapsed();

    let (outcome, task_trace) = match call {
        Err(ms) => (RunOutcome::Timeout(ms), None),
        Ok((Err(e), trace)) => (RunOutcome::EngineError(e), trace),
        Ok((Ok(()), trace)) => {
            // The v3 contract: `process_message_for_channel` answers `Ok(())`
            // even when individual workflows failed — the failures are pushed
            // into `message.errors()`. Derived here so a transport cannot
            // forget to look, which is what `channel_call` was doing.
            if message.has_errors() {
                (RunOutcome::WorkflowErrors(error_summary(&message)), trace)
            } else {
                (RunOutcome::Ok, trace)
            }
        }
    };

    Execution {
        message,
        task_trace,
        outcome,
        duration,
    }
}

/// Run `channel`'s workflows over `data` on the admitted [`crate::runtime::RuntimeGeneration`].
///
/// Pins `generation` in [`crate::runtime::PINNED_GENERATION`] for the duration
/// of the execution so nested in-process `channel_call` invocations, guard
/// stores, and model inference resolve against the same generation (`V2-05.2`,
/// `V2-05.3`). When `opts.capture` is `None` and `generation.engine4` is
/// present, executes on the Dataflow v4 [`dataflow_rs::Engine`]; when per-task
/// `ExecutionTrace` capture is requested (or on synthetic test generations
/// without `engine4`), delegates to [`execute_admitted`].
pub async fn execute_admitted_on_generation(
    generation: &Arc<crate::runtime::RuntimeGeneration>,
    channel: &str,
    data: &Value,
    metadata: &Value,
    opts: ExecOpts<'_>,
) -> Execution {
    crate::runtime::generation::PINNED_GENERATION
        .scope(
            Arc::clone(generation),
            execute_admitted_on_generation_inner(generation, channel, data, metadata, opts),
        )
        .await
}

async fn execute_admitted_on_generation_inner(
    generation: &Arc<crate::runtime::RuntimeGeneration>,
    channel: &str,
    data: &Value,
    metadata: &Value,
    opts: ExecOpts<'_>,
) -> Execution {
    if let Some(ref engine4) = generation.engine4 {
        return execute_admitted_on_v4(
            engine4,
            channel,
            data,
            metadata,
            opts,
            generation.run_budget_limits,
        )
        .await;
    }
    execute_admitted(&generation.engine, channel, data, metadata, opts).await
}

fn v4_trace_options(max_snapshot_bytes: usize) -> dataflow_rs::observe::TraceOptions {
    let redact = ["metadata.headers", "metadata.cookies", "metadata.oauth"]
        .into_iter()
        .filter_map(|p| p.parse().ok())
        .collect();
    let mut opts = dataflow_rs::observe::TraceOptions::default()
        .timings(true)
        .changes(true)
        .redact(redact);
    if max_snapshot_bytes > 0 {
        opts = opts.max_bytes(max_snapshot_bytes);
    }
    opts
}

fn v4_trace_to_v3(
    v4_trace: &dataflow_rs::observe::Trace,
    v4_msg: &dataflow_rs::Message,
    initial_payload: &Value,
    capture: TraceCapture,
) -> dataflow_rs_v3::ExecutionTrace {
    let mut v3_trace = dataflow_rs_v3::ExecutionTrace::with_options(dataflow_rs_v3::TraceOptions {
        changes: true,
        snapshot_audit_trail: dataflow_rs_v3::AuditTrailScope::Own,
        max_snapshot_bytes: capture.max_snapshot_bytes,
        ..Default::default()
    });

    for (idx, (step, step_ctx)) in v4_trace
        .steps
        .iter()
        .zip(v4_trace.contexts())
        .enumerate()
    {
        match (&step.task_id, step.result) {
            (None, dataflow_rs::observe::StepResult::Skipped) => {
                v3_trace.add_step(
                    dataflow_rs_v3::ExecutionStep::workflow_skipped(&step.workflow_id)
                        .with_loop_counter(step.loop_counter),
                );
            }
            (None, _) => {
                // Internal engine workflow-level write (e.g. loop sweep entry);
                // `v4_trace.contexts()` already applied its changes for later task steps.
            }
            (Some(task_id), dataflow_rs::observe::StepResult::Skipped) => {
                v3_trace.add_step(
                    dataflow_rs_v3::ExecutionStep::task_skipped(&step.workflow_id, task_id)
                        .with_loop_counter(step.loop_counter)
                        .with_element_index(step.element_index),
                );
            }
            (Some(task_id), _) => {
                let mut step_msg = dataflow_rs_v3::Message::builder()
                    .id(v4_msg.id())
                    .payload_json(initial_payload)
                    .capture_changes(false)
                    .audit_mode(dataflow_rs_v3::AuditMode::Off)
                    .build();
                step_msg.context = step_ctx.as_value().clone();
                for rec in &step.errors {
                    let mut eb =
                        dataflow_rs_v3::ErrorInfo::builder(rec.code.as_str(), rec.message.as_str());
                    if let Some(timestamp) = rec.timestamp.as_deref() {
                        eb = eb.timestamp(timestamp);
                    }
                    if let Some(path) = rec.path.as_deref() {
                        eb = eb.path(path);
                    }
                    if let Some(workflow_id) = rec.workflow_id.as_deref() {
                        eb = eb.workflow_id(workflow_id);
                    }
                    if let Some(t_id) = rec.task_id.as_deref() {
                        eb = eb.task_id(t_id);
                    }
                    if let Some(detail) = rec.detail.as_deref() {
                        eb = eb.detail(detail);
                    }
                    if let Some(element_index) = rec.element_index {
                        eb = eb.element_index(element_index);
                    }
                    step_msg.add_error(eb.build());
                }

                let v3_changes: Vec<dataflow_rs_v3::engine::message::Change> = step
                    .changes
                    .iter()
                    .filter(|c| {
                        &*c.path != "metadata.progress"
                            && !c.path.starts_with("metadata._orion_errors")
                    })
                    .map(|c| dataflow_rs_v3::engine::message::Change {
                        path: Arc::clone(&c.path),
                        old_value: c.old_value.clone(),
                        new_value: c.new_value.clone(),
                        removed: c.removed,
                    })
                    .collect();

                let mut v3_step =
                    dataflow_rs_v3::ExecutionStep::executed(&step.workflow_id, task_id, &step_msg)
                        .with_changes(v3_changes)
                        .with_loop_counter(step.loop_counter)
                        .with_element_index(step.element_index);

                if v4_trace.truncated && step.changes.is_empty() {
                    v3_step.message = None;
                }
                if let Some(elapsed) = step.elapsed {
                    v3_step = v3_step.with_timing(
                        chrono::Utc::now(),
                        u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
                    );
                }
                if let Some(map_ctxs) = v4_trace.mapping_contexts(idx) {
                    v3_step = v3_step.with_mapping_contexts(
                        map_ctxs
                            .into_iter()
                            .map(|c| Value::from(c.as_value()))
                            .collect(),
                    );
                }
                v3_trace.add_step(v3_step);
            }
        }
    }

    v3_trace
}

async fn execute_admitted_on_v4(
    engine: &dataflow_rs::Engine,
    channel: &str,
    data: &Value,
    metadata: &Value,
    opts: ExecOpts<'_>,
    budget_limits: super::budget::RunBudgetLimits,
) -> Execution {
    use dataflow_rs::datavalue::OwnedDataValue;
    use dataflow_rs::run::{AuditMode, RunOptions, RunStatus};

    let mut v4_msg = dataflow_rs::Message::from_parts(
        OwnedDataValue::from(data),
        OwnedDataValue::from(data),
        OwnedDataValue::from(metadata),
    );

    if opts.timeout_ms == Some(0) {
        return Execution {
            message: v4_message_to_v3(&v4_msg, data, metadata),
            task_trace: None,
            outcome: RunOutcome::Timeout(0),
            duration: Duration::ZERO,
        };
    }

    let accountant = super::budget::SharedRunAccountant::new(budget_limits);
    let mut run_opts = accountant.apply(
        RunOptions::default()
            .channel(channel)
            .capture_changes(false)
            .audit(AuditMode::Off),
    );
    if let Some(capture) = opts.capture {
        run_opts = run_opts.trace(v4_trace_options(capture.max_snapshot_bytes));
    }
    if let Some(bucket) = opts.routing_bucket {
        run_opts = run_opts.routing_bucket(bucket);
    }
    if let Some(ms) = opts.timeout_ms
        && let Some(deadline) = tokio::time::Instant::now()
            .into_std()
            .checked_add(Duration::from_millis(ms))
    {
        run_opts = run_opts.deadline(deadline);
    }

    let started = Instant::now();
    let run_fut = engine.run_with(&mut v4_msg, &run_opts);
    let run = if let Some(p) = opts.profile {
        super::profile::ORION_PROFILE
            .scope(p.clone(), run_fut)
            .await
    } else {
        run_fut.await
    };
    let duration = started.elapsed();

    let task_trace = opts
        .capture
        .and_then(|capture| run.trace().map(|t| v4_trace_to_v3(t, &v4_msg, data, capture)));
    let message = v4_message_to_v3(&v4_msg, data, metadata);
    let outcome = match run.status() {
        RunStatus::Completed => {
            if message.has_errors() {
                RunOutcome::WorkflowErrors(error_summary(&message))
            } else {
                RunOutcome::Ok
            }
        }
        RunStatus::CompletedWithErrors => RunOutcome::WorkflowErrors(error_summary(&message)),
        RunStatus::TimedOut => RunOutcome::Timeout(opts.timeout_ms.unwrap_or(0)),
        RunStatus::Cancelled => {
            RunOutcome::EngineError(dataflow_rs_v3::DataflowError::Timeout("cancelled".into()))
        }
        _ => match run.into_result() {
            Ok(_) => {
                if message.has_errors() {
                    RunOutcome::WorkflowErrors(error_summary(&message))
                } else {
                    RunOutcome::Ok
                }
            }
            Err(e) => RunOutcome::EngineError(dataflow_rs_v3::DataflowError::from(
                super::HandlerError::from(e),
            )),
        },
    };

    Execution {
        message,
        task_trace,
        outcome,
        duration,
    }
}

fn v4_message_to_v3(
    v4_msg: &dataflow_rs::Message,
    initial_payload: &Value,
    initial_metadata: &Value,
) -> dataflow_rs_v3::Message {
    let data_val = Value::from(v4_msg.context().data());
    let temp_val = Value::from(v4_msg.context().temp_data());
    let meta_val = Value::from(v4_msg.context().metadata());
    let meta_ref = if meta_val.is_null() {
        initial_metadata
    } else {
        &meta_val
    };

    let mut v3_msg = dataflow_rs_v3::Message::builder()
        .id(v4_msg.id())
        .payload_json(initial_payload)
        .metadata_json(meta_ref)
        .capture_changes(false)
        .audit_mode(dataflow_rs_v3::AuditMode::Off)
        .build();
    if !data_val.is_null() {
        dataflow_rs_v3::engine::utils::set_nested_value(
            &mut v3_msg.context,
            "data",
            dataflow_rs_v3::datavalue::OwnedDataValue::from(&data_val),
        );
    }
    if !temp_val.is_null() {
        dataflow_rs_v3::engine::utils::set_nested_value(
            &mut v3_msg.context,
            "temp_data",
            dataflow_rs_v3::datavalue::OwnedDataValue::from(&temp_val),
        );
    }
    // Copy every error diagnostic field shared between dataflow-rs v4
    // `ErrorRecord` and v3 `ErrorInfo` (`code`, `message`, `path`,
    // `workflow_id`, `task_id`, `timestamp`, `detail`, `element_index`).
    // Note: `rec.cause` (`Arc<dyn Error>`) and `rec.site` (`ErrorSite`) are
    // v4-only in-memory diagnostics with no v3 `ErrorInfo` counterpart.
    for rec in v4_msg.errors() {
        let mut eb = dataflow_rs_v3::ErrorInfo::builder(rec.code.as_str(), rec.message.as_str());
        if let Some(timestamp) = rec.timestamp.as_deref() {
            eb = eb.timestamp(timestamp);
        }
        if let Some(path) = rec.path.as_deref() {
            eb = eb.path(path);
        }
        if let Some(workflow_id) = rec.workflow_id.as_deref() {
            eb = eb.workflow_id(workflow_id);
        }
        if let Some(task_id) = rec.task_id.as_deref() {
            eb = eb.task_id(task_id);
        }
        if let Some(detail) = rec.detail.as_deref() {
            eb = eb.detail(detail);
        }
        if let Some(element_index) = rec.element_index {
            eb = eb.element_index(element_index);
        }
        v3_msg.add_error(eb.build());
    }
    v3_msg
}

/// The `code: message; code: message` summary three transports were each
/// building from `message.errors()`.
fn error_summary(message: &dataflow_rs_v3::Message) -> String {
    message
        .errors()
        .iter()
        .map(|e| format!("{}: {}", e.code, e.message))
        .collect::<Vec<_>>()
        .join("; ")
}

// ============================================================
// Dataflow v4 Serving & Offline Execution Adapter (V2-02.2..V2-02.5)
// ============================================================

/// Normalized v2 execution outcome (`V2-02.4`).
///
/// Exhaustive over the 8 canonical v2 outcome classes so v1/v2 HTTP envelopes,
/// async queues, source runners, and nested `channel_call` invocations map
/// from one shared classification without ever reporting v2 `"success"` when
/// task errors occurred (even under `continue_on_error`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizedRunOutcome {
    /// At least one workflow matched and completed with zero task errors.
    Success,
    /// Zero workflows matched the channel/condition/rollout bucket, with zero errors.
    NoMatch,
    /// Input or validation rule rejected the request before or during execution.
    Rejected(String),
    /// One or more tasks failed (`Stopped` or `CompletedWithErrors` under `continue_on_error`).
    TaskFailure {
        summary: String,
        continued_after_error: bool,
    },
    /// Execution exceeded its deadline budget (in milliseconds).
    DeadlineExceeded(u64),
    /// Execution was cancelled via its [`dataflow_rs::run::CancelToken`].
    Cancelled,
    /// A downstream dependency or resource pool was transiently unavailable.
    Unavailable(String),
    /// Unrecoverable expression, budget, or internal engine failure.
    InternalFailure(String),
}

impl NormalizedRunOutcome {
    /// Prometheus `status` label for `orion_messages_total`.
    ///
    /// Note that `TaskFailure` with `continued_after_error: true` still records
    /// `"error"` so `continue_on_error` never hides task failures in metrics.
    pub fn status_label(&self) -> &'static str {
        match self {
            Self::Success | Self::NoMatch => "ok",
            Self::DeadlineExceeded(_) => "timeout",
            Self::Cancelled => "cancelled",
            Self::Rejected(_)
            | Self::TaskFailure { .. }
            | Self::Unavailable(_)
            | Self::InternalFailure(_) => "error",
        }
    }

    /// Whether the run succeeded under v2 strict semantics (never true when task errors exist).
    pub fn is_v2_success(&self) -> bool {
        matches!(self, Self::Success)
    }

    /// Map to the legacy v1 [`RunOutcome`] for v1 transport compatibility (`V2-02.4`).
    pub fn to_v1_outcome(&self) -> RunOutcome {
        RunOutcome::from(self.clone())
    }

    /// Render the canonical v2 response envelope (`V2-02.4`).
    pub fn to_v2_envelope(&self, data: &Value) -> Value {
        match self {
            Self::Success => serde_json::json!({
                "status": "success",
                "outcome": "success",
                "data": data,
            }),
            Self::NoMatch => serde_json::json!({
                "status": "no_match",
                "outcome": "no_match",
                "data": data,
            }),
            Self::Rejected(reason) => serde_json::json!({
                "status": "error",
                "outcome": "rejected",
                "error": { "code": "RUN_REJECTED", "message": reason },
            }),
            Self::TaskFailure {
                summary,
                continued_after_error,
            } => serde_json::json!({
                "status": "error",
                "outcome": "task_failure",
                "continued_after_error": continued_after_error,
                "error": { "code": "TASK_FAILURE", "message": summary },
                "data": data,
            }),
            Self::DeadlineExceeded(ms) => serde_json::json!({
                "status": "error",
                "outcome": "deadline_exceeded",
                "error": { "code": "DEADLINE_EXCEEDED", "timeout_ms": ms },
            }),
            Self::Cancelled => serde_json::json!({
                "status": "error",
                "outcome": "cancelled",
                "error": { "code": "RUN_CANCELLED", "message": "execution cancelled" },
            }),
            Self::Unavailable(reason) => serde_json::json!({
                "status": "error",
                "outcome": "unavailable",
                "error": { "code": "DEPENDENCY_UNAVAILABLE", "message": reason },
            }),
            Self::InternalFailure(reason) => serde_json::json!({
                "status": "error",
                "outcome": "internal_failure",
                "error": { "code": "INTERNAL_FAILURE", "message": reason },
            }),
        }
    }
}

/// Execution options for the Dataflow v4 serving/offline adapter (`V2-02.2`, `V2-02.3`).
#[derive(Default)]
pub struct ExecOptsV4<'a> {
    pub timeout_ms: Option<u64>,
    pub capture: Option<TraceCapture>,
    pub routing_bucket: Option<u8>,
    pub profile: Option<&'a Arc<super::profile::ProfileCollector>>,
    pub accountant: Option<&'a super::budget::SharedRunAccountant>,
    pub cancel: Option<&'a dataflow_rs::run::CancelToken>,
    pub run_context: Option<crate::extensions::RunContext>,
}

/// Result of one Dataflow v4 execution (`V2-02.2`).
pub struct ExecutionV4 {
    pub message: dataflow_rs::Message,
    pub outcome: NormalizedRunOutcome,
    pub matched_workflows: usize,
    pub duration: Duration,
}

/// Execute `channel`'s workflows on the Dataflow v4 [`dataflow_rs::Engine`] (`V2-02.2`, `V2-02.3`, `V2-02.4`).
pub async fn execute_admitted_v4(
    engine: &dataflow_rs::Engine,
    channel: &str,
    data: &Value,
    metadata: &Value,
    opts: ExecOptsV4<'_>,
) -> ExecutionV4 {
    use dataflow_rs::datavalue::OwnedDataValue;
    use dataflow_rs::run::{AuditMode, RunOptions};

    let traced = opts.capture.is_some();
    let payload_dv = OwnedDataValue::from(data);
    let data_dv = OwnedDataValue::from(data);
    let meta_dv = OwnedDataValue::from(metadata);
    let mut message = dataflow_rs::Message::from_parts(payload_dv, data_dv, meta_dv);

    let mut run_opts = RunOptions::default()
        .channel(channel)
        .capture_changes(traced)
        .audit(if traced {
            AuditMode::Full
        } else {
            AuditMode::Off
        });

    if let Some(bucket) = opts.routing_bucket {
        run_opts = run_opts.routing_bucket(bucket);
    }
    if let Some(ms) = opts.timeout_ms
        && let Some(deadline) = tokio::time::Instant::now()
            .into_std()
            .checked_add(Duration::from_millis(ms))
    {
        run_opts = run_opts.deadline(deadline);
    }
    if let Some(cancel) = opts.cancel {
        if cancel.is_cancelled() {
            return ExecutionV4 {
                message,
                outcome: NormalizedRunOutcome::Cancelled,
                matched_workflows: 0,
                duration: Duration::ZERO,
            };
        }
        run_opts = run_opts.cancel(cancel);
    }
    if opts.timeout_ms == Some(0) {
        return ExecutionV4 {
            message,
            outcome: NormalizedRunOutcome::DeadlineExceeded(0),
            matched_workflows: 0,
            duration: Duration::ZERO,
        };
    }
    if let Some(acct) = opts.accountant {
        run_opts = run_opts.accountant(acct);
    }

    let started = Instant::now();
    let run_fut = engine.run_with(&mut message, &run_opts);
    let run = match (opts.profile, opts.run_context) {
        (Some(p), Some(rc)) => {
            super::profile::ORION_PROFILE
                .scope(
                    p.clone(),
                    super::functions::connector_call::ACTIVE_RUN_CONTEXT.scope(rc, run_fut),
                )
                .await
        }
        (Some(p), None) => {
            super::profile::ORION_PROFILE
                .scope(p.clone(), run_fut)
                .await
        }
        (None, Some(rc)) => {
            super::functions::connector_call::ACTIVE_RUN_CONTEXT
                .scope(rc, run_fut)
                .await
        }
        (None, None) => run_fut.await,
    };
    let duration = started.elapsed();

    let matched_workflows = run.matched();
    let outcome = classify_v4_run(&run, &message, opts.timeout_ms);

    ExecutionV4 {
        message,
        outcome,
        matched_workflows,
        duration,
    }
}

fn classify_v4_run(
    run: &dataflow_rs::Run,
    message: &dataflow_rs::Message,
    timeout_ms: Option<u64>,
) -> NormalizedRunOutcome {
    use dataflow_rs::ErrorKind;
    use dataflow_rs::run::RunStatus;

    let err_summary = || {
        message
            .errors()
            .iter()
            .map(|e| format!("{}: {}", e.code, e.message))
            .collect::<Vec<_>>()
            .join("; ")
    };

    match run.status() {
        RunStatus::Completed => {
            if !message.errors().is_empty() {
                NormalizedRunOutcome::TaskFailure {
                    summary: err_summary(),
                    continued_after_error: true,
                }
            } else if run.matched() == 0 {
                NormalizedRunOutcome::NoMatch
            } else {
                NormalizedRunOutcome::Success
            }
        }
        RunStatus::CompletedWithErrors => NormalizedRunOutcome::TaskFailure {
            summary: err_summary(),
            continued_after_error: true,
        },
        RunStatus::TimedOut => NormalizedRunOutcome::DeadlineExceeded(timeout_ms.unwrap_or(0)),
        RunStatus::Cancelled => NormalizedRunOutcome::Cancelled,
        RunStatus::Stopped(err) => match err.kind() {
            ErrorKind::TimedOut => NormalizedRunOutcome::DeadlineExceeded(timeout_ms.unwrap_or(0)),
            ErrorKind::Cancelled => NormalizedRunOutcome::Cancelled,
            ErrorKind::Service { code } if code.as_ref() == "VALIDATION_ERROR" => {
                NormalizedRunOutcome::Rejected(err.message().to_string())
            }
            ErrorKind::Service { .. } if err.is_retryable() => {
                NormalizedRunOutcome::Unavailable(format!("{}: {}", err.code(), err.message()))
            }
            ErrorKind::Service { .. } => {
                let summary = if !message.errors().is_empty() {
                    err_summary()
                } else {
                    format!("{}: {}", err.code(), err.message())
                };
                NormalizedRunOutcome::TaskFailure {
                    summary,
                    continued_after_error: false,
                }
            }
            _ => {
                NormalizedRunOutcome::InternalFailure(format!("{}: {}", err.code(), err.message()))
            }
        },
        _ => NormalizedRunOutcome::InternalFailure("unknown run status".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_label_is_one_derivation() {
        assert_eq!(RunOutcome::Ok.status_label(), "ok");
        assert_eq!(RunOutcome::Timeout(50).status_label(), "timeout");
        // The unification: workflow errors are an `error` on every transport,
        // where the sync route used to call them `ok`.
        assert_eq!(
            RunOutcome::WorkflowErrors("boom".into()).status_label(),
            "error"
        );
        assert_eq!(
            RunOutcome::EngineError(dataflow_rs_v3::DataflowError::Unknown("x".into()))
                .status_label(),
            "error"
        );
    }

    /// #350, #373: a looping workflow run without a trace keeps no audit
    /// entries and no value copies, so its memory does not grow with the
    /// sweeps. A traced run still keeps both: its per-step diff is built from
    /// them.
    #[tokio::test]
    async fn only_a_traced_run_captures_per_write_changes() {
        let workflow = dataflow_rs_v3::Workflow::from_json(
            r#"{"id":"w","name":"w","channel":"c","priority":0,"condition":true,
                "loop":{"counter":"i","max":3},
                "tasks":[{"id":"m","name":"m","function":{"name":"map","input":
                  {"mappings":[{"path":"temp_data.seen","logic":{"var":"temp_data.i"}}]}}}]}"#,
        )
        .expect("workflow parses");
        let engine = Arc::new(
            dataflow_rs_v3::Engine::new(vec![workflow], std::collections::HashMap::new())
                .expect("engine builds"),
        );
        let data = serde_json::json!({"xs": [1, 2, 3]});
        let metadata = serde_json::json!({});

        let untraced = execute_admitted(&engine, "c", &data, &metadata, ExecOpts::default()).await;
        assert!(untraced.outcome.is_ok());
        assert!(
            untraced.message.audit_trail().is_empty(),
            "an untraced run keeps no audit entries, so a loop holds nothing per sweep"
        );
        assert!(
            untraced.message.temp_data().get("seen").is_some(),
            "the writes themselves still land"
        );

        let traced = execute_admitted(
            &engine,
            "c",
            &data,
            &metadata,
            ExecOpts {
                capture: Some(TraceCapture {
                    max_snapshot_bytes: 0,
                }),
                ..Default::default()
            },
        )
        .await;
        assert!(traced.outcome.is_ok());
        assert!(
            traced
                .message
                .audit_trail()
                .iter()
                .all(|entry| entry.changes.len() == 1),
            "a traced run captures each write"
        );
        let trace = traced.task_trace.expect("a traced run returns its trace");
        assert!(
            trace
                .steps
                .iter()
                .all(|step| step.changes.as_ref().is_some_and(|c| c.len() == 1)),
            "the trace's per-step diff is still populated"
        );
    }

    #[tokio::test]
    async fn v2_02_2_3_4_serving_adapter_normalizes_outcomes_and_never_reports_success_with_task_errors()
     {
        let engine = dataflow_rs::Engine::builder()
            .config(
                dataflow_rs::run::EngineConfig::default()
                    .error_context(crate::engine::ERROR_CONTEXT.clone())
                    .with_condition_error_policy(dataflow_rs::run::ConditionErrorPolicy::Stop),
            )
            .workflow_json(&serde_json::json!({
                "id": "wf-ok",
                "channel": "orders",
                "condition": { "==": [{ "var": "data.mode" }, "ok"] },
                "tasks": [
                    {
                        "id": "t1",
                        "function": {
                            "name": "map",
                            "input": { "mappings": [{ "path": "data.done", "logic": true }] }
                        }
                    }
                ]
            }))
            .workflow_json(&serde_json::json!({
                "id": "wf-continue-err",
                "channel": "orders",
                "condition": { "==": [{ "var": "data.mode" }, "continue_err"] },
                "tasks": [
                    {
                        "id": "t-fail",
                        "on_failure": "continue",
                        "function": {
                            "name": "validation",
                            "input": {
                                "rules": [{ "logic": false, "message": "bad field" }]
                            }
                        }
                    },
                    {
                        "id": "t-after",
                        "function": {
                            "name": "map",
                            "input": { "mappings": [{ "path": "data.after", "logic": true }] }
                        }
                    }
                ]
            }))
            .build()
            .expect("v4 engine builds");

        // 1. Success outcome
        let ok_exec = execute_admitted_v4(
            &engine,
            "orders",
            &serde_json::json!({ "mode": "ok" }),
            &serde_json::json!({}),
            ExecOptsV4::default(),
        )
        .await;
        assert_eq!(ok_exec.outcome, NormalizedRunOutcome::Success);
        assert!(ok_exec.outcome.is_v2_success());
        assert_eq!(ok_exec.outcome.status_label(), "ok");

        // 2. NoMatch outcome (v1 compat maps to Ok, v2 distinguishes NoMatch)
        let nomatch_exec = execute_admitted_v4(
            &engine,
            "orders",
            &serde_json::json!({ "mode": "unmatched" }),
            &serde_json::json!({}),
            ExecOptsV4::default(),
        )
        .await;
        assert_eq!(nomatch_exec.outcome, NormalizedRunOutcome::NoMatch);
        assert!(!nomatch_exec.outcome.is_v2_success());
        assert!(nomatch_exec.outcome.to_v1_outcome().is_ok());

        // 3. TaskFailure with continue_on_error: v2 never reports success and retains failure metric
        let cont_exec = execute_admitted_v4(
            &engine,
            "orders",
            &serde_json::json!({ "mode": "continue_err" }),
            &serde_json::json!({}),
            ExecOptsV4::default(),
        )
        .await;
        assert!(matches!(
            cont_exec.outcome,
            NormalizedRunOutcome::TaskFailure {
                continued_after_error: true,
                ..
            }
        ));
        assert!(!cont_exec.outcome.is_v2_success());
        assert_eq!(cont_exec.outcome.status_label(), "error");
        let v2_env = cont_exec
            .outcome
            .to_v2_envelope(&Value::from(cont_exec.message.context().data()));
        assert_eq!(v2_env["status"], "error");
        assert_eq!(v2_env["outcome"], "task_failure");
        assert_eq!(v2_env["continued_after_error"], true);

        // 4. Cancellation attribution
        let token = dataflow_rs::run::CancelToken::new();
        token.cancel();
        let cancel_exec = execute_admitted_v4(
            &engine,
            "orders",
            &serde_json::json!({ "mode": "ok" }),
            &serde_json::json!({}),
            ExecOptsV4 {
                cancel: Some(&token),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(cancel_exec.outcome, NormalizedRunOutcome::Cancelled);
        assert_eq!(cancel_exec.outcome.status_label(), "cancelled");
    }

    #[test]
    fn v4_message_to_v3_preserves_message_id_and_all_error_diagnostic_fields() {
        let v4_msg: dataflow_rs::Message = serde_json::from_value(serde_json::json!({
            "id": "msg-preserve-id-01",
            "payload": { "hello": "world" },
            "context": {
                "data": { "hello": "world" },
                "metadata": { "channel": "orders" },
                "temp_data": {}
            },
            "audit_trail": [],
            "errors": [{
                "code": "CONNECTOR_ERROR",
                "message": "upstream failed",
                "path": "data.orders.0",
                "workflow_id": "wf-orders",
                "task_id": "task-call-api",
                "timestamp": "2026-01-01T00:00:00Z",
                "detail": "HTTP 502 from internal-gateway: upstream connection reset",
                "element_index": 3
            }]
        }))
        .expect("deserialize v4 message");
        let expected_id = v4_msg.id().to_string();

        let v3_msg = v4_message_to_v3(
            &v4_msg,
            &serde_json::json!({ "hello": "world" }),
            &serde_json::json!({ "channel": "orders" }),
        );

        assert_eq!(v3_msg.id(), expected_id, "v4 message ID must be preserved");
        assert_eq!(v3_msg.errors().len(), 1);
        let err = &v3_msg.errors()[0];
        assert_eq!(err.code, "CONNECTOR_ERROR");
        assert_eq!(err.message, "upstream failed");
        assert_eq!(err.path.as_deref(), Some("data.orders.0"));
        assert_eq!(err.workflow_id.as_deref(), Some("wf-orders"));
        assert_eq!(err.task_id.as_deref(), Some("task-call-api"));
        assert_eq!(err.timestamp.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(
            err.detail.as_deref(),
            Some("HTTP 502 from internal-gateway: upstream connection reset")
        );
        assert_eq!(err.element_index, Some(3));
    }

    #[tokio::test]
    async fn execute_admitted_on_generation_preserves_for_each_and_detail_diagnostics() {
        let v3_engine = std::sync::Arc::new(
            dataflow_rs_v3::Engine::new(vec![], std::collections::HashMap::new())
                .expect("empty v3 engine"),
        );
        let mut registry = dataflow_rs::Registry::new(crate::engine::kit::test_exprs());
        registry.register(
            crate::engine::functions::crypto::NAME,
            crate::engine::functions::crypto::CryptoFunction,
        );
        let v4_engine = dataflow_rs::Engine::builder()
            .registry(registry)
            .config(
                dataflow_rs::run::EngineConfig::default()
                    .error_context(crate::engine::ERROR_CONTEXT.clone()),
            )
            .workflow_json(&serde_json::json!({
                "id": "wf-fanout",
                "channel": "orders",
                "tasks": [
                    {
                        "id": "t-validate-item",
                        "for_each": { "over": { "var": "data.items" }, "as": "item" },
                        "function": {
                            "name": "crypto",
                            "input": {
                                "op": "hmac_verify",
                                "key": "secret",
                                "data": "msg",
                                "signature": { "var": "temp_data.item" },
                                "output": "data.ok"
                            }
                        }
                    }
                ]
            }))
            .build()
            .expect("v4 engine builds");

        let handle = crate::runtime::generation::test_handle();
        handle.publish(
            v3_engine,
            Some(v4_engine),
            std::sync::Arc::new(crate::channel::ChannelSnapshot::empty()),
            crate::engine::FunctionRegistry::builtin().clone(),
            std::sync::Arc::new(crate::plugin::PluginSet::empty()),
            std::sync::Arc::new(crate::model::ModelSet::empty()),
        );
        let generation = handle.load();

        let exec = execute_admitted_on_generation(
            &generation,
            "orders",
            &serde_json::json!({ "items": ["00".repeat(32), "!!not-an-encoding!!"] }),
            &serde_json::json!({ "trace_id": "t-1" }),
            ExecOpts::default(),
        )
        .await;

        assert!(!exec.outcome.is_ok());
        assert!(!exec.message.id().is_empty());
        assert!(!exec.message.errors().is_empty());
        let err = &exec.message.errors()[0];
        assert_eq!(err.workflow_id.as_deref(), Some("wf-fanout"));
        assert_eq!(err.task_id.as_deref(), Some("t-validate-item"));
        assert_eq!(err.element_index, Some(1));
        assert!(err.timestamp.as_deref().is_some_and(|ts| !ts.is_empty()));
    }

    #[tokio::test]
    async fn td02_traced_execution_runs_on_v4_engine_and_redacts_sensitive_metadata() {
        // Empty v3 engine: proves traced execution runs on `engine4`, not `engine`.
        let v3_engine = std::sync::Arc::new(
            dataflow_rs_v3::Engine::new(vec![], std::collections::HashMap::new())
                .expect("empty v3 engine"),
        );
        let v4_engine = dataflow_rs::Engine::builder()
            .config(
                dataflow_rs::run::EngineConfig::default()
                    .error_context(crate::engine::ERROR_CONTEXT.clone()),
            )
            .workflow_json(&serde_json::json!({
                "id": "wf-traced-v4",
                "channel": "orders",
                "tasks": [
                    {
                        "id": "t-map",
                        "function": {
                            "name": "map",
                            "input": {
                                "mappings": [
                                    { "path": "data.doubled", "logic": { "*": [{ "var": "data.x" }, 2] } }
                                ]
                            }
                        }
                    }
                ]
            }))
            .build()
            .expect("v4 engine builds");

        let handle = crate::runtime::generation::test_handle();
        handle.publish(
            v3_engine,
            Some(v4_engine),
            std::sync::Arc::new(crate::channel::ChannelSnapshot::empty()),
            crate::engine::FunctionRegistry::builtin().clone(),
            std::sync::Arc::new(crate::plugin::PluginSet::empty()),
            std::sync::Arc::new(crate::model::ModelSet::empty()),
        );
        let generation = handle.load();

        let exec = execute_admitted_on_generation(
            &generation,
            "orders",
            &serde_json::json!({ "x": 21 }),
            &serde_json::json!({
                "channel": "orders",
                "headers": { "x-secret-header": "top-secret-header-val" },
                "cookies": { "session": "top-secret-cookie-val" },
                "oauth": { "access_token": "top-secret-oauth-token" }
            }),
            ExecOpts {
                capture: Some(TraceCapture {
                    max_snapshot_bytes: 0,
                }),
                ..Default::default()
            },
        )
        .await;

        assert!(exec.outcome.is_ok());
        assert_eq!(
            serde_json::Value::from(exec.message.data())["doubled"],
            serde_json::json!(42)
        );
        let trace = exec.task_trace.expect("traced v4 run must return ExecutionTrace");
        assert_eq!(trace.steps.len(), 1);
        let step = &trace.steps[0];
        assert_eq!(step.workflow_id, "wf-traced-v4");
        assert_eq!(step.task_id.as_deref(), Some("t-map"));
        assert!(step.message.is_some());
        assert!(
            step.changes.as_ref().is_some_and(|c| c.len() == 1),
            "per-step changes should exclude metadata.progress and contain the map write"
        );
        let trace_json = serde_json::to_string(&trace).expect("serializes");
        assert!(!trace_json.contains("top-secret-header-val"));
        assert!(!trace_json.contains("top-secret-cookie-val"));
        assert!(!trace_json.contains("top-secret-oauth-token"));
    }

    #[tokio::test]
    async fn td03_generation_run_budget_halts_excessive_task_dispatches() {
        let v3_engine = std::sync::Arc::new(
            dataflow_rs_v3::Engine::new(vec![], std::collections::HashMap::new())
                .expect("empty v3 engine"),
        );
        let v4_engine = dataflow_rs::Engine::builder()
            .config(
                dataflow_rs::run::EngineConfig::default()
                    .error_context(crate::engine::ERROR_CONTEXT.clone()),
            )
            .workflow_json(&serde_json::json!({
                "id": "wf-loop",
                "channel": "orders",
                "loop": { "counter": "i", "max": 10 },
                "tasks": [
                    {
                        "id": "t-step",
                        "function": {
                            "name": "map",
                            "input": {
                                "mappings": [
                                    { "path": "data.step", "logic": { "var": "temp_data.i" } }
                                ]
                            }
                        }
                    }
                ]
            }))
            .build()
            .expect("v4 engine builds");

        let handle = crate::runtime::generation::test_handle();
        handle.publish_with_estate(
            v3_engine,
            Some(v4_engine),
            std::sync::Arc::new(crate::channel::ChannelSnapshot::empty()),
            crate::engine::FunctionRegistry::builtin().clone(),
            std::sync::Arc::new(crate::plugin::PluginSet::empty()),
            std::sync::Arc::new(crate::model::ModelSet::empty()),
            None,
            super::super::budget::RunBudgetLimits {
                max_tasks: Some(3),
                ..Default::default()
            },
        );
        let generation = handle.load();

        let exec = execute_admitted_on_generation(
            &generation,
            "orders",
            &serde_json::json!({}),
            &serde_json::json!({}),
            ExecOpts::default(),
        )
        .await;

        assert!(
            matches!(
                exec.outcome,
                RunOutcome::EngineError(dataflow_rs_v3::DataflowError::BudgetExceeded(_))
            ),
            "expected BudgetExceeded when loop exceeds max_tasks=3, got {:?}",
            exec.outcome
        );
    }

    #[tokio::test]
    async fn td07_pinned_generation_isolates_connector_snapshot_across_rotation() {
        let registry = crate::connector::ConnectorRegistry::default();
        let initial_cfg = crate::connector::ConnectorConfig::Cache(
            serde_json::from_value(serde_json::json!({
                "backend": "redis",
                "url": "redis://cache-gen1.internal:6379"
            }))
            .expect("valid cache config"),
        );
        registry.insert_for_test("shared-cache", initial_cfg).await;

        let handle = crate::runtime::generation::test_handle();
        handle.publish_with_estate(
            handle.load().engine.clone(),
            None,
            std::sync::Arc::new(crate::channel::ChannelSnapshot::empty()),
            crate::engine::FunctionRegistry::builtin().clone(),
            std::sync::Arc::new(crate::plugin::PluginSet::empty()),
            std::sync::Arc::new(crate::model::ModelSet::empty()),
            Some(registry.snapshot_configs()),
            Default::default(),
        );
        let pinned_gen = handle.load();

        // Rotate connector in the live registry to a different URL.
        let rotated_cfg = crate::connector::ConnectorConfig::Cache(
            serde_json::from_value(serde_json::json!({
                "backend": "redis",
                "url": "redis://cache-gen2.internal:6379"
            }))
            .expect("valid cache config"),
        );
        registry.insert_for_test("shared-cache", rotated_cfg).await;

        // Outside a pinned generation, the live registry returns the new URL.
        let live = registry.get("shared-cache").await.expect("live config");
        let crate::connector::ConnectorConfig::Cache(live_cache) = live.as_ref() else {
            panic!("expected cache connector");
        };
        assert_eq!(
            live_cache.url.as_deref(),
            Some("redis://cache-gen2.internal:6379")
        );

        // Inside `PINNED_GENERATION`, `registry.get` returns the pinned snapshot's URL.
        crate::runtime::generation::PINNED_GENERATION
            .scope(pinned_gen, async {
                let pinned = registry.get("shared-cache").await.expect("pinned config");
                let crate::connector::ConnectorConfig::Cache(pinned_cache) = pinned.as_ref() else {
                    panic!("expected cache connector");
                };
                assert_eq!(
                    pinned_cache.url.as_deref(),
                    Some("redis://cache-gen1.internal:6379")
                );
            })
            .await;
    }
}
