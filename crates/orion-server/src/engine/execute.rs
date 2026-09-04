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
    EngineError(dataflow_rs::DataflowError),
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
        match self {
            Self::Ok => "ok",
            Self::WorkflowErrors(_) | Self::EngineError(_) => "error",
            Self::Timeout(_) => "timeout",
        }
    }

    /// Whether the run produced the result the caller asked for.
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// Everything about one execution that is not the channel or the payload.
#[derive(Default)]
pub struct ExecOpts<'a> {
    /// Deadline for the whole call. `None` runs untimed.
    pub timeout_ms: Option<u64>,
    /// Per-task trace capture, when the channel opted in via
    /// `config.tracing.task_details`.
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
    pub message: dataflow_rs::Message,
    /// The per-task trace, when [`ExecOpts::capture`] asked for one.
    pub task_trace: Option<dataflow_rs::ExecutionTrace>,
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
    engine: &Arc<dataflow_rs::Engine>,
    channel: &str,
    data: &Value,
    metadata: &Value,
    opts: ExecOpts<'_>,
) -> Execution {
    let mut builder = dataflow_rs::Message::builder()
        .payload_json(data)
        .metadata_json(metadata);
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

/// The `code: message; code: message` summary three transports were each
/// building from `message.errors()`.
fn error_summary(message: &dataflow_rs::Message) -> String {
    message
        .errors()
        .iter()
        .map(|e| format!("{}: {}", e.code, e.message))
        .collect::<Vec<_>>()
        .join("; ")
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
            RunOutcome::EngineError(dataflow_rs::DataflowError::Unknown("x".into())).status_label(),
            "error"
        );
    }
}
