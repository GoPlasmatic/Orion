//! Per-task engine timing, fed straight into Prometheus.
//!
//! Orion could already time its own handlers — `observed_handler_named` wraps
//! all nine connector handlers and emits
//! `orion_connector_request_duration_seconds{connector,channel}`. What it could
//! not time, at any price, is dataflow-rs's eight *sync built-ins*: `map`,
//! `validate`, `filter`, `parse_json`, `parse_xml`, `publish_json`,
//! `publish_xml` and `log` are dispatched inside a private executor method and
//! never reach the handler registry, so there is no seam to wrap. Their cost
//! was visible only as `workflow_overhead_ms` in the opt-in profile surface —
//! a residual computed by subtraction (`profile.rs`), not a measurement.
//!
//! The engine's `Observer` is the callback that closes it. Unlike
//! an `ExecutionTrace` this is always on and allocates nothing per request:
//! traces are gated on `config.tracing.task_details` and deep-clone the whole
//! message per step, so they can never be a metrics source.
//!
//! The workflow lifecycle, on the same callback, closes the other half. Engine overhead — condition evaluation, group
//! gating, loop bookkeeping, audit writes, arena management — was reachable
//! only as `workflow_overhead_ms` in the profile surface: a *residual*, got by
//! subtracting handler timings from a whole-message total, so it silently
//! absorbed everything else unmeasured and was per-request and opt-in besides.
//! `WorkflowFinished` reports the workflow's own wall clock, and
//! `orion_workflow_duration_seconds` minus the task histogram's sum for the
//! same workflow is that overhead as a measurement.
//!
//! Distinct from `ProfileCollector`, which stays: that is a per-request debug
//! artifact keyed by connector, including a per-message JSONLogic resolution
//! for `channel_call` (F49) that the engine has no concept of.

use dataflow_rs::observe::{Event, Interest, Observer};

/// Emits `orion_task_duration_seconds{workflow,task,function}` per dispatched
/// task, and `orion_workflow_duration_seconds{workflow}` per workflow run.
pub struct MetricsObserver;

impl Observer for MetricsObserver {
    fn interest(&self) -> Interest {
        Interest::WORKFLOWS | Interest::TASKS | Interest::TIMINGS
    }

    fn on(&self, event: &Event<'_>) {
        match event {
            Event::TaskFinished {
                info,
                elapsed: Some(duration),
                ..
            } => {
                crate::metrics::record_task_duration(
                    info.workflow_id,
                    info.task_id,
                    interned_function_name(info.function),
                    duration.as_secs_f64(),
                );
            }
            Event::WorkflowFinished {
                record,
                elapsed: Some(duration),
                ..
            } => {
                crate::metrics::record_workflow_duration(record.id, duration.as_secs_f64());
            }
            _ => {}
        }
    }
}

/// Map a borrowed function name onto the `&'static str` the registry interned
/// it to.
///
/// `metrics` labels want a `&'static str` or an owned `String`; borrowing from
/// the registry's interned set avoids allocating per task, and it doubles as a
/// cardinality bound — a name no registry ever held cannot mint a new label
/// value. One hash lookup per dispatched task, which is noise next to any
/// task body.
fn interned_function_name(name: &str) -> &'static str {
    super::functions::registry::interned(name).unwrap_or("other")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_known_function_interns_to_itself() {
        for name in super::super::FunctionRegistry::builtin().names() {
            assert_eq!(interned_function_name(name), name);
        }
    }

    /// An unrecognised name must not become a label value of its own. Nothing
    /// can reach the engine under an unknown name today — the registry gates
    /// workflow creation — but a metric label is the wrong place to find that
    /// out.
    #[test]
    fn an_unknown_function_collapses_to_one_label() {
        assert_eq!(interned_function_name("enrich"), "other");
        assert_eq!(interned_function_name("__nope__"), "other");
    }

    /// The observer must survive the names the executor actually reports.
    #[test]
    fn the_validation_builtin_is_known() {
        assert_eq!(interned_function_name("validation"), "validation");
    }

    /// Runs `workflows` on an engine with no functions of its own, with
    /// `observer` attached to the run.
    async fn run_observed(workflows: &[serde_json::Value], observer: &dyn Observer) {
        let mut builder = dataflow_rs::Engine::builder();
        for workflow in workflows {
            builder = builder.workflow_json(workflow);
        }
        let engine = builder.build().expect("engine builds");
        let mut message = dataflow_rs::Message::new(dataflow_rs::datavalue::OwnedDataValue::Null);
        engine
            .run_with(
                &mut message,
                &dataflow_rs::RunOptions::default().observer(observer),
            )
            .await;
    }

    fn map_workflow(id: &str, condition: bool) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "condition": condition,
            "tasks": [{"id": "m", "function": {"name": "map", "input":
                {"mappings": [{"path": "data.ok", "logic": true}]}}}]
        })
    }

    /// The whole justification for attaching an observer: the built-ins are
    /// reported.
    ///
    /// `map` never reaches a host call layer, so if a future dataflow-rs
    /// stopped emitting events for that path, `orion_task_duration_seconds`
    /// would go quiet for the functions it exists to cover, with nothing else
    /// failing. This pins the dependency rather than the arithmetic.
    #[tokio::test]
    async fn a_builtin_is_reported_to_the_observer() {
        use std::sync::Mutex;

        #[derive(Default)]
        struct Seen(Mutex<Vec<(String, String)>>);
        impl Observer for Seen {
            fn interest(&self) -> Interest {
                Interest::TASKS
            }
            fn on(&self, event: &Event<'_>) {
                if let Event::TaskFinished { info, .. } = event {
                    self.0
                        .lock()
                        .expect("test")
                        .push((info.task_id.to_string(), info.function.to_string()));
                }
            }
        }

        let seen = Seen::default();
        run_observed(&[map_workflow("w", true)], &seen).await;
        assert_eq!(
            *seen.0.lock().expect("test"),
            vec![("m".to_string(), "map".to_string())],
            "a built-in must still be timed"
        );
    }

    /// The workflow lifecycle event fires, and its duration covers the task
    /// bodies inside it — which is what makes the subtraction that yields
    /// engine overhead meaningful.
    ///
    /// A skipped workflow must *not* report a duration: the overhead figure
    /// is per run, and counting a workflow the condition rejected would
    /// dilute it with runs that never happened.
    #[tokio::test]
    async fn a_workflow_reports_once_and_only_when_it_runs() {
        use std::sync::Mutex;
        use std::time::Duration;

        #[derive(Default)]
        struct Seen {
            workflows: Mutex<Vec<(String, Duration)>>,
            tasks: Mutex<Duration>,
        }
        impl Observer for Seen {
            fn interest(&self) -> Interest {
                Interest::WORKFLOWS | Interest::TASKS | Interest::TIMINGS
            }
            fn on(&self, event: &Event<'_>) {
                match event {
                    Event::TaskFinished {
                        elapsed: Some(d), ..
                    } => *self.tasks.lock().expect("test") += *d,
                    Event::WorkflowFinished {
                        record,
                        elapsed: Some(d),
                        ..
                    } => self
                        .workflows
                        .lock()
                        .expect("test")
                        .push((record.id.to_string(), *d)),
                    _ => {}
                }
            }
        }

        let seen = Seen::default();
        run_observed(
            &[map_workflow("runs", true), map_workflow("skipped", false)],
            &seen,
        )
        .await;

        let workflows = seen.workflows.lock().expect("test");
        assert_eq!(
            workflows
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["runs"],
            "a workflow its condition rejected never starts, so it is never timed"
        );
        assert!(
            workflows[0].1 >= *seen.tasks.lock().expect("test"),
            "the workflow's own clock must cover the task bodies inside it, or \
             subtracting them for the overhead figure would go negative"
        );
    }
}
