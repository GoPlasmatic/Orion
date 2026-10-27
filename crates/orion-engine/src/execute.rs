//! The one step between "the guards admitted this" and "shape a response".
//!
//! `channel::guards::apply_guards` is the single enforcement point for
//! ingress, and what follows it is shared the same way: building a `Message`,
//! loading the engine snapshot, running it, the "engine returned Ok but the
//! message carries task errors" rule, and mapping a timeout. Copied per
//! transport — the sync HTTP route, the async trace queue, the in-process
//! `channel_call` handler — those copies drift: one that skips the
//! `has_errors` rule lets a target channel whose workflow failed its tasks
//! report success to its caller, and a timeout mapped several ways makes every
//! change to dataflow-rs's error semantics several edits that no test can
//! check were all made.
//!
//! `runtime::execute_admitted_on_generation` is that step, once, over [`execute_on`]. What stays with each transport is
//! what genuinely differs: persistence, response shaping, and what a
//! [`RunOutcome`] *means* to it — an HTTP caller gets the errors in its
//! envelope, the async path routes them to the DLQ, a cron occurrence
//! settles as failed.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::runner::TraceCapture;

/// What one admitted execution did.
///
/// The four outcomes are exhaustive over what the engine can report, which is
/// the point: a new dataflow-rs error shape lands here, once, and every
/// transport's `match` fails to compile until it is handled.
#[derive(Debug)]
pub enum RunOutcome {
    /// Every workflow that matched ran, and none reported a task error.
    Ok,
    /// The run completed and the message carries task errors. Carries the joined `code: message` summary, which is what
    /// all three of the transports that look at it were building by hand.
    WorkflowErrors(String),
    /// The deadline expired, with the budget in milliseconds.
    Timeout(u64),
    /// The engine itself failed the call.
    EngineError(dataflow_rs::Error),
}

impl RunOutcome {
    /// The `status` label for `orion_messages_total`.
    ///
    /// One derivation, so the counter means the same thing on every transport.
    /// It did not: the sync route counted [`Self::WorkflowErrors`] as `ok`
    /// while the async path counted it as `error`, so a channel
    /// whose workflow failed every synchronous request reported a 100% success
    /// rate. `error` is the answer the metric's own documentation implies —
    /// "messages processed, by outcome" — and a workflow that failed its tasks
    /// did not achieve its outcome.
    ///
    /// The label is derived here; *emitting* it stays with the transport,
    /// because whether an execution should be counted at all is knowledge only
    /// the transport has: `channel_call` emits nothing because the request
    /// that triggered it was already counted at its own ingress.
    pub fn status_label(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Timeout(_) => "timeout",
            Self::WorkflowErrors(_) | Self::EngineError(_) => "error",
        }
    }

    /// Whether the run produced the result the caller asked for.
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// A cancelled run, as the timeout 1.12 recorded for it.
fn cancelled() -> dataflow_rs::Error {
    dataflow_rs::Error::service("TIMEOUT_ERROR", "Timeout error: cancelled").with_retryable(true)
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
    /// [`crate::utils::rollout_bucket_for_identity`].
    ///
    /// `None` means "admitted by every workflow, rollout or not", and is
    /// deliberate on `channel_call`: an in-process call is already running
    /// inside its caller's bucket.
    pub routing_bucket: Option<u8>,
    /// The per-request profile scope, when one is in flight.
    pub profile: Option<&'a Arc<super::profile::ProfileCollector>>,
}

/// One execution and everything a transport needs from it.
pub struct Execution {
    /// The message as the workflows left it — the source of the response body,
    /// the persisted result, and the error list.
    pub message: dataflow_rs::Message,
    /// The run's trace, when [`ExecOpts::capture`] asked for one.
    pub task_trace: Option<dataflow_rs::observe::Trace>,
    pub outcome: RunOutcome,
    /// Wall time of the engine call alone, for the latency histogram.
    pub duration: Duration,
}

impl Execution {
    /// The message's `data`, as JSON: the response body and the persisted
    /// result.
    pub fn data(&self) -> Value {
        Value::from(self.message.context().data())
    }
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

/// The message a run on dataflow-rs 4 starts from: the caller's `data` as
/// the payload and as the initial `data`, beside `metadata`.
///
/// `data` is seeded because dataflow-rs 4 keeps the payload out of the
/// evaluation context (a path rooted at `payload` is a `PAYLOAD_VAR`), so a
/// workflow reads its input under `data`. Every surface that runs a workflow
/// builds its message here, so an offline run starts where a served one does.
///
/// The one `clone` is inherent, and cannot become a move. The engine holds
/// the payload behind an `Arc` and `data` by value inside the context, which
/// tasks mutate in place, so the two cannot share one value. And the payload
/// must stay as the caller sent it: `channel_call` with no `data` hands the
/// child the caller's *payload* (`ctx.payload()`), and an async trace's
/// persisted result is the whole message, `payload` included — both read
/// after tasks have changed `data`. Dropping either half would change what
/// those see.
pub fn serving_message(data: &Value, metadata: &Value) -> dataflow_rs::Message {
    use dataflow_rs::datavalue::OwnedDataValue;
    let data = OwnedDataValue::from(data);
    dataflow_rs::Message::from_parts(data.clone(), data, OwnedDataValue::from(metadata))
}

/// Run `channel`'s workflows on `engine` over `data`: the message build, the
/// deadline arm, the run budget and the `has_errors` rule. Every surface that
/// runs a workflow comes through here, a workflow provider package's
/// operation included.
pub async fn execute_on(
    engine: &dataflow_rs::Engine,
    channel: &str,
    data: &Value,
    metadata: &Value,
    opts: ExecOpts<'_>,
    budget_limits: super::budget::RunBudgetLimits,
) -> Execution {
    use dataflow_rs::run::{AuditMode, RunOptions, RunStatus};

    let mut v4_msg = serving_message(data, metadata);

    if opts.timeout_ms == Some(0) {
        return Execution {
            message: v4_msg,
            task_trace: None,
            outcome: RunOutcome::Timeout(0),
            duration: Duration::ZERO,
        };
    }

    // No accountant at all when no ceiling is set — the default. It could
    // never refuse anything, nothing reads its counters, and without one the
    // engine skips the per-task and per-expression accounting calls. A
    // `channel_call` child inherits whatever its caller has, so a nested run
    // is unbounded exactly when its root is.
    let accountant = (!budget_limits.is_unbounded())
        .then(|| super::budget::SharedRunAccountant::new(budget_limits));
    let mut run_opts = RunOptions::default()
        .channel(channel)
        .capture_changes(false)
        .audit(AuditMode::Off);
    if let Some(accountant) = &accountant {
        run_opts = accountant.apply(run_opts);
    }
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
    let mut run = if let Some(p) = opts.profile {
        super::profile::ORION_PROFILE
            .scope(p.clone(), run_fut)
            .await
    } else {
        run_fut.await
    };
    let duration = started.elapsed();

    let task_trace = run.take_trace();
    let message = v4_msg;
    let outcome = match run.status() {
        RunStatus::Completed => {
            if !message.errors().is_empty() {
                RunOutcome::WorkflowErrors(error_summary(&message))
            } else {
                RunOutcome::Ok
            }
        }
        RunStatus::CompletedWithErrors => RunOutcome::WorkflowErrors(error_summary(&message)),
        RunStatus::TimedOut => RunOutcome::Timeout(opts.timeout_ms.unwrap_or(0)),
        RunStatus::Cancelled => RunOutcome::EngineError(cancelled()),
        _ => match run.into_result() {
            Ok(_) => {
                if !message.errors().is_empty() {
                    RunOutcome::WorkflowErrors(error_summary(&message))
                } else {
                    RunOutcome::Ok
                }
            }
            Err(e) => RunOutcome::EngineError(e),
        },
    };

    Execution {
        message,
        task_trace,
        outcome,
        duration,
    }
}

fn error_summary(message: &dataflow_rs::Message) -> String {
    message
        .errors()
        .iter()
        .map(|e| format!("{}: {}", e.code, e.message))
        .collect::<Vec<_>>()
        .join("; ")
}
