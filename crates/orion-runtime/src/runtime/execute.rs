//! The step between "the guards admitted this" and "shape a response", on a
//! generation: [`execute_admitted_on_generation`] pins the generation and the
//! run's connector context, then runs [`crate::engine::execute::execute_on`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::generation::RuntimeGeneration;
use crate::engine::{ExecOpts, Execution};

/// Run `channel`'s workflows over `data` on the admitted [`RuntimeGeneration`].
///
/// Pins `generation` ([`super::generation::pin`]) for the duration
/// of the execution so nested in-process `channel_call` invocations, guard
/// stores, and model inference resolve against the same generation (`V2-05.2`,
/// `V2-05.3`).
///
/// Owns the message build, the deadline arm and the `has_errors` rule.
/// Everything downstream — persisting a trace, shaping a response, committing
/// an offset, emitting the counter whose label [`crate::engine::RunOutcome::status_label`]
/// derives — is the caller's.
pub async fn execute_admitted_on_generation(
    generation: &Arc<RuntimeGeneration>,
    channel: &str,
    data: &Value,
    metadata: &Value,
    opts: ExecOpts<'_>,
) -> Execution {
    super::generation::pin(
        Arc::clone(generation),
        execute_admitted_on_generation_inner(generation, channel, data, metadata, opts),
    )
    .await
}

async fn execute_admitted_on_generation_inner(
    generation: &Arc<RuntimeGeneration>,
    channel: &str,
    data: &Value,
    metadata: &Value,
    opts: ExecOpts<'_>,
) -> Execution {
    let token = opts.cancel.map_or_else(
        dataflow_rs::run::CancelToken::new,
        dataflow_rs::run::CancelToken::child,
    );
    let _cancel_on_drop = CancelOnDrop(token.clone());
    let accountant = (!generation.run_budget_limits.is_unbounded()).then(|| {
        Arc::new(crate::engine::SharedRunAccountant::new(
            generation.run_budget_limits,
        ))
    });
    let run = connector_run_context(generation, opts.timeout_ms)
        .with_execution(token.clone(), accountant.clone())
        .context();
    let opts = ExecOpts {
        cancel: Some(&token),
        accountant: accountant
            .as_deref()
            .map(|a| a as &dyn dataflow_rs::run::RunAccountant),
        ..opts
    };
    super::functions::ACTIVE_RUN_CONTEXT
        .scope(
            run,
            crate::engine::execute::execute_on(
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

struct CancelOnDrop(dataflow_rs::run::CancelToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// How far out the deadline of a run with no timeout is put. The run itself
/// has none; the context needs an instant, so it gets one that is never the
/// bound that fires.
const UNBOUNDED_RUN_HORIZON: Duration = Duration::from_secs(24 * 60 * 60);

/// The context every `connector_call` of one run shares: the generation's
/// connector authority as its ceiling, the run's deadline, and one budget,
/// so the run's calls draw down the same attempts and sends.
fn connector_run_context(
    generation: &RuntimeGeneration,
    timeout_ms: Option<u64>,
) -> crate::extensions::AdmittedRun {
    let horizon = timeout_ms.map_or(UNBOUNDED_RUN_HORIZON, Duration::from_millis);
    let now = Instant::now();
    let deadline = now.checked_add(horizon).unwrap_or(now);
    crate::extensions::context::AdmittedRun::admit(
        uuid::Uuid::now_v7().to_string(),
        generation.id,
        deadline,
        generation.connector_budget_limits,
        generation.connector_authority.union(),
    )
}

#[cfg(test)]
#[expect(clippy::panic)]
mod tests {
    use super::*;
    use crate::engine::TraceCapture;

    use crate::engine::execute::*;

    #[tokio::test]
    async fn admitted_engine_and_connector_context_share_cancellation_and_limits() {
        struct WaitsForCancellation {
            ready: Arc<tokio::sync::Notify>,
            context: Arc<std::sync::Mutex<Option<crate::extensions::RunContext>>>,
        }
        impl dataflow_rs::Function for WaitsForCancellation {
            type Config = ();
            async fn call(
                &self,
                ctx: &mut dataflow_rs::Ctx<'_>,
                _: &(),
            ) -> dataflow_rs::Result<dataflow_rs::Outcome> {
                assert!(ctx.cancel().is_some());
                *self.context.lock().expect("execution fixture succeeds") =
                    Some(super::super::functions::ACTIVE_RUN_CONTEXT.with(Clone::clone));
                self.ready.notify_one();
                std::future::pending().await
            }
        }
        let ready = Arc::new(tokio::sync::Notify::new());
        let context = Arc::new(std::sync::Mutex::new(None));
        let engine = dataflow_rs::Engine::builder()
            .register("wait", WaitsForCancellation { ready: ready.clone(), context: context.clone() })
            .workflow_json(&serde_json::json!({"id":"wait","channel":"c","tasks":[{"id":"wait","function":{"name":"wait"}}]}))
            .build().expect("execution fixture succeeds");
        let handle = crate::runtime::generation::test_handle();
        handle.publish_parts(crate::runtime::generation::GenerationParts {
            connector_budget_limits: crate::config::ConnectorBudgetLimits {
                max_attempts: 7,
                ..Default::default()
            },
            run_budget_limits: crate::engine::RunBudgetLimits {
                max_tasks: Some(10),
                ..Default::default()
            },
            ..crate::runtime::generation::GenerationParts::new(
                engine,
                Arc::new(crate::channel::ChannelSnapshot::empty()),
                crate::engine::FunctionRegistry::builtin().clone(),
                Arc::new(crate::plugin::PluginSet::empty()),
            )
        });
        let generation = handle.load();
        let mut task = tokio::spawn(async move {
            execute_admitted_on_generation(
                &generation,
                "c",
                &serde_json::json!({}),
                &serde_json::json!({}),
                ExecOpts::default(),
            )
            .await
        });
        ready.notified().await;
        let run = context
            .lock()
            .expect("execution fixture succeeds")
            .clone()
            .expect("execution fixture succeeds");
        assert_eq!(run.budget().limits().max_attempts, 7);
        assert_eq!(
            run.accountant()
                .expect("execution fixture succeeds")
                .snapshot()
                .tasks_dispatched,
            1
        );
        run.cancellation().cancel();
        let execution = tokio::time::timeout(Duration::from_secs(2), &mut task).await;
        if execution.is_err() {
            task.abort();
        }
        assert!(
            matches!(
                execution
                    .expect("execution fixture succeeds")
                    .expect("execution fixture succeeds")
                    .outcome,
                RunOutcome::EngineError(_)
            ),
            "cancelling the connector context must stop the running engine"
        );
    }

    #[tokio::test]
    async fn nested_engine_executions_cannot_reset_the_root_accountant() {
        let engine = dataflow_rs::Engine::builder()
            .workflow_json(&serde_json::json!({"id":"one","channel":"c","tasks":[
                {"id":"one","function":{"name":"map","input":{"mappings":[{"path":"data.done","logic":true}]}}}
            ]})).build().expect("execution fixture succeeds");
        let accountant = crate::engine::SharedRunAccountant::new(crate::engine::RunBudgetLimits {
            max_tasks: Some(1),
            ..Default::default()
        });
        let data = serde_json::json!({});
        let first = execute_on(
            &engine,
            "c",
            &data,
            &data,
            ExecOpts {
                accountant: Some(&accountant),
                ..Default::default()
            },
            Default::default(),
        )
        .await;
        assert!(first.outcome.is_ok());
        let second = execute_on(
            &engine,
            "c",
            &data,
            &data,
            ExecOpts {
                accountant: Some(&accountant),
                ..Default::default()
            },
            Default::default(),
        )
        .await;
        assert!(
            matches!(second.outcome, RunOutcome::EngineError(ref e) if *e.kind() == dataflow_rs::ErrorKind::Budget)
        );
        assert!(second.message.context().data().get("done").is_none());
    }

    #[test]
    fn the_status_label_is_one_derivation() {
        assert_eq!(RunOutcome::Ok.status_label(), "ok");
        assert_eq!(RunOutcome::Timeout(50).status_label(), "timeout");
        // Workflow errors are an `error` on every transport, the sync route
        // included: one derivation, so no transport can label them `ok`.
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

    /// A run carries an accountant only when a ceiling is set: with none, it
    /// could refuse nothing and is not attached.
    #[tokio::test]
    async fn an_accountant_is_attached_only_when_a_limit_is_set() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct SeesAccountant(Arc<AtomicBool>);
        impl dataflow_rs::Function for SeesAccountant {
            type Config = ();
            async fn call(
                &self,
                ctx: &mut dataflow_rs::Ctx<'_>,
                _: &(),
            ) -> dataflow_rs::Result<dataflow_rs::Outcome> {
                self.0.store(ctx.accountant().is_some(), Ordering::SeqCst);
                Ok(dataflow_rs::Outcome::Ok)
            }
        }

        let seen = Arc::new(AtomicBool::new(false));
        let engine = dataflow_rs::Engine::builder()
            .register("probe", SeesAccountant(seen.clone()))
            .workflow_json(&serde_json::json!({
                "id": "w", "name": "w", "channel": "c", "priority": 0, "condition": true,
                "tasks": [{"id": "p", "name": "p", "function": {"name": "probe"}}]
            }))
            .build()
            .expect("engine builds");
        let (data, metadata) = (serde_json::json!({}), serde_json::json!({}));

        let unbounded = crate::engine::budget::RunBudgetLimits::default();
        assert!(unbounded.is_unbounded());
        let run = execute_on(
            &engine,
            "c",
            &data,
            &metadata,
            ExecOpts::default(),
            unbounded,
        )
        .await;
        assert!(run.outcome.is_ok());
        assert!(!seen.load(Ordering::SeqCst), "no ceiling, no accountant");

        let bounded = crate::engine::budget::RunBudgetLimits {
            max_tasks: Some(10),
            ..Default::default()
        };
        assert!(!bounded.is_unbounded());
        let run = execute_on(&engine, "c", &data, &metadata, ExecOpts::default(), bounded).await;
        assert!(run.outcome.is_ok());
        assert!(
            seen.load(Ordering::SeqCst),
            "a ceiling attaches the accountant"
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
            run_budget_limits: crate::engine::budget::RunBudgetLimits {
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

        // Inside a pinned generation, `registry.get` returns the pinned snapshot's URL.
        crate::runtime::generation::pin(pinned_gen, async {
            let pinned = registry.get("shared-api").await.expect("pinned config");
            let crate::connector::ConnectorConfig::Http(pinned_http) = pinned.as_ref() else {
                panic!("expected http connector");
            };
            assert_eq!(pinned_http.url, "https://api-gen1.internal");
        })
        .await;
    }
}
