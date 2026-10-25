//! The one step between "the guards admitted this" and "shape a response".
//!
//! `channel::guards::apply_guards` is the single enforcement point for
//! ingress, and everything after it used to be copied per transport: the sync
//! HTTP route, the async trace queue, the Kafka consume loop (since removed)
//! and the in-process `channel_call` handler each built a `Message`, loaded the engine
//! snapshot, ran it, re-derived the "engine returned Ok
//! but the message carries task errors" rule, and mapped a timeout — **four
//! different ways**: an `OrionError::Timeout`, a synthesised engine timeout,
//! a `fail("timeout", …)`, and another engine timeout with different wording. Nothing tied the four
//! together, so the next change to dataflow-rs's error semantics was four
//! edits and a test suite that could not tell if one was missed.
//!
//! They had already drifted. Kafka alone omitted the rollout bucket (correctly
//! — a record has no sticky caller identity), and `channel_call` never applied
//! the `has_errors` rule at all, so a target channel whose workflow failed its
//! tasks reported success to its caller.
//!
//! [`execute_admitted_on_generation`] is that step, once. What stays with each transport is
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
    /// [`crate::engine::utils::rollout_bucket_for_identity`].
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

/// Run `channel`'s workflows over `data` on the admitted [`crate::runtime::RuntimeGeneration`].
///
/// Pins `generation` in [`crate::runtime::generation::PINNED_GENERATION`] for the duration
/// of the execution so nested in-process `channel_call` invocations, guard
/// stores, and model inference resolve against the same generation (`V2-05.2`,
/// `V2-05.3`).
///
/// Owns the message build, the deadline arm and the `has_errors` rule.
/// Everything downstream — persisting a trace, shaping a response, committing
/// an offset, emitting the counter whose label [`RunOutcome::status_label`]
/// derives — is the caller's.
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
    let run = connector_run_context(generation, opts.timeout_ms);
    super::functions::connector_call::ACTIVE_RUN_CONTEXT
        .scope(
            run,
            execute_on(
                &generation.engine,
                channel,
                data,
                metadata,
                opts,
                generation.run_budget_limits,
            ),
        )
        .await
}

/// How far out the deadline of a run with no timeout is put. The run itself
/// has none; the context needs an instant, so it gets one that is never the
/// bound that fires.
const UNBOUNDED_RUN_HORIZON: Duration = Duration::from_secs(24 * 60 * 60);

/// The context every `connector_call` of one run shares: the generation's
/// connector authority as its ceiling, the run's deadline, and one budget,
/// so the run's calls draw down the same attempts and sends.
fn connector_run_context(
    generation: &crate::runtime::RuntimeGeneration,
    timeout_ms: Option<u64>,
) -> crate::extensions::RunContext {
    let horizon = timeout_ms.map_or(UNBOUNDED_RUN_HORIZON, Duration::from_millis);
    let now = Instant::now();
    let deadline = now.checked_add(horizon).unwrap_or(now);
    crate::extensions::context::AdmittedRun::admit(
        uuid::Uuid::now_v7().to_string(),
        generation.id,
        deadline,
        crate::extensions::context::BudgetLimits::default(),
        generation.connector_authority.union(),
    )
    .context()
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
/// workflow reads its input under `data`. The one copy is inherent: the
/// payload stays as the caller sent it while tasks change `data`. Every
/// surface that runs a workflow builds its message here, so an offline run
/// starts where a served one does.
pub fn serving_message(data: &Value, metadata: &Value) -> dataflow_rs::Message {
    use dataflow_rs::datavalue::OwnedDataValue;
    let data = OwnedDataValue::from(data);
    dataflow_rs::Message::from_parts(data.clone(), data, OwnedDataValue::from(metadata))
}

async fn execute_on(
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
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
            RunOutcome::EngineError(dataflow_rs::Error::service("UNKNOWN_ERROR", "x"))
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
        let engine = dataflow_rs::Engine::builder()
            .workflow_json(&serde_json::json!({
                "id": "w", "name": "w", "channel": "c", "priority": 0, "condition": true,
                "loop": {"counter": "i", "max": 3},
                "tasks": [{"id": "m", "name": "m", "function": {"name": "map", "input":
                  {"mappings": [{"path": "temp_data.seen", "logic": {"var": "temp_data.i"}}]}}}]
            }))
            .build()
            .expect("engine builds");
        let data = serde_json::json!({"xs": [1, 2, 3]});
        let metadata = serde_json::json!({});
        let limits = crate::engine::budget::RunBudgetLimits::default();

        let untraced =
            execute_on(&engine, "c", &data, &metadata, ExecOpts::default(), limits).await;
        assert!(untraced.outcome.is_ok());
        assert!(
            untraced.message.audit().is_empty(),
            "an untraced run keeps no audit entries, so a loop holds nothing per sweep"
        );
        assert!(untraced.task_trace.is_none());
        assert!(
            untraced.message.context().temp_data().get("seen").is_some(),
            "the writes themselves still land"
        );

        let traced = execute_on(
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
            limits,
        )
        .await;
        assert!(traced.outcome.is_ok());
        let trace = traced.task_trace.expect("a traced run returns its trace");
        let task_steps: Vec<_> = trace.steps.iter().filter(|s| s.task_id.is_some()).collect();
        assert_eq!(task_steps.len(), 3, "one step per sweep");
        assert!(
            task_steps
                .iter()
                .all(|step| step.changes.iter().any(|c| &*c.path == "temp_data.seen")),
            "each step records its own write"
        );
    }

    #[tokio::test]
    async fn execute_admitted_on_generation_preserves_for_each_and_detail_diagnostics() {
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
            v4_engine,
            std::sync::Arc::new(crate::channel::ChannelSnapshot::empty()),
            crate::engine::FunctionRegistry::builtin().clone(),
            std::sync::Arc::new(crate::plugin::PluginSet::empty()),
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
    async fn traced_execution_runs_on_v4_engine_and_redacts_sensitive_metadata() {
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
            v4_engine,
            std::sync::Arc::new(crate::channel::ChannelSnapshot::empty()),
            crate::engine::FunctionRegistry::builtin().clone(),
            std::sync::Arc::new(crate::plugin::PluginSet::empty()),
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
        assert_eq!(exec.data()["doubled"], serde_json::json!(42));
        let trace = exec.task_trace.expect("a traced run returns its trace");
        assert_eq!(trace.steps.len(), 1);
        let step = &trace.steps[0];
        assert_eq!(&*step.workflow_id, "wf-traced-v4");
        assert_eq!(step.task_id.as_deref(), Some("t-map"));
        assert!(
            step.changes.iter().any(|c| &*c.path == "data.doubled"),
            "the step's changes carry the map write"
        );
        let trace_json = serde_json::to_string(&trace).expect("serializes");
        assert!(!trace_json.contains("top-secret-header-val"));
        assert!(!trace_json.contains("top-secret-cookie-val"));
        assert!(!trace_json.contains("top-secret-oauth-token"));
    }

    #[tokio::test]
    async fn generation_run_budget_halts_excessive_task_dispatches() {
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
        handle.publish_parts(crate::runtime::generation::GenerationParts {
            run_budget_limits: super::super::budget::RunBudgetLimits {
                max_tasks: Some(3),
                ..Default::default()
            },
            ..crate::runtime::generation::GenerationParts::new(
                v4_engine,
                std::sync::Arc::new(crate::channel::ChannelSnapshot::empty()),
                crate::engine::FunctionRegistry::builtin().clone(),
                std::sync::Arc::new(crate::plugin::PluginSet::empty()),
            )
        });
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
                RunOutcome::EngineError(ref e) if *e.kind() == dataflow_rs::ErrorKind::Budget
            ),
            "expected BudgetExceeded when loop exceeds max_tasks=3, got {:?}",
            exec.outcome
        );
    }

    #[tokio::test]
    async fn pinned_generation_isolates_connector_snapshot_across_rotation() {
        let registry = crate::connector::ConnectorRegistry::default();
        let initial_cfg = crate::connector::ConnectorConfig::Http(
            serde_json::from_value(serde_json::json!({
                "url": "https://api-gen1.internal"
            }))
            .expect("valid http config"),
        );
        registry.insert_for_test("shared-api", initial_cfg).await;

        let handle = crate::runtime::generation::test_handle();
        handle.publish_parts(crate::runtime::generation::GenerationParts {
            connectors: Some(registry.snapshot_configs()),
            ..crate::runtime::generation::GenerationParts::new(
                dataflow_rs::Engine::builder()
                    .build()
                    .expect("an empty engine builds"),
                std::sync::Arc::new(crate::channel::ChannelSnapshot::empty()),
                crate::engine::FunctionRegistry::builtin().clone(),
                std::sync::Arc::new(crate::plugin::PluginSet::empty()),
            )
        });
        let pinned_gen = handle.load();

        // Rotate connector in the live registry to a different URL.
        let rotated_cfg = crate::connector::ConnectorConfig::Http(
            serde_json::from_value(serde_json::json!({
                "url": "https://api-gen2.internal"
            }))
            .expect("valid http config"),
        );
        registry.insert_for_test("shared-api", rotated_cfg).await;

        // Outside a pinned generation, the live registry returns the new URL.
        let live = registry.get("shared-api").await.expect("live config");
        let crate::connector::ConnectorConfig::Http(live_http) = live.as_ref() else {
            panic!("expected http connector");
        };
        assert_eq!(live_http.url, "https://api-gen2.internal");

        // Inside `PINNED_GENERATION`, `registry.get` returns the pinned snapshot's URL.
        crate::runtime::generation::PINNED_GENERATION
            .scope(pinned_gen, async {
                let pinned = registry.get("shared-api").await.expect("pinned config");
                let crate::connector::ConnectorConfig::Http(pinned_http) = pinned.as_ref() else {
                    panic!("expected http connector");
                };
                assert_eq!(pinned_http.url, "https://api-gen1.internal");
            })
            .await;
    }
}
