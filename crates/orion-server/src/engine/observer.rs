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
//! `ExecutionObserver` (dataflow-rs 3.1) is the callback that closes it. Unlike
//! an `ExecutionTrace` this is always on and allocates nothing per request:
//! traces are gated on `config.tracing.task_details` and deep-clone the whole
//! message per step, so they can never be a metrics source.
//!
//! dataflow-rs 3.7 added the workflow lifecycle to the same callback, which
//! closes the other half. Engine overhead — condition evaluation, group
//! gating, loop bookkeeping, audit writes, arena management — was reachable
//! only as `workflow_overhead_ms` in the profile surface: a *residual*, got by
//! subtracting handler timings from a whole-message total, so it silently
//! absorbed everything else unmeasured and was per-request and opt-in besides.
//! `workflow_finished` reports the workflow's own wall clock, and
//! `orion_workflow_duration_seconds` minus the task histogram's sum for the
//! same workflow is that overhead as a measurement.
//!
//! Distinct from `ProfileCollector`, which stays: that is a per-request debug
//! artifact keyed by connector, including a per-message JSONLogic resolution
//! for `channel_call` (F49) that the engine has no concept of.

use dataflow_rs::{ExecutionObserver, TaskEvent, WorkflowFinished};

/// Emits `orion_task_duration_seconds{workflow,task,function}` per dispatched
/// task, and `orion_workflow_duration_seconds{workflow}` per workflow run.
pub struct MetricsObserver;

impl ExecutionObserver for MetricsObserver {
    fn task_finished(&self, event: &TaskEvent<'_>) {
        // Contract: called synchronously on the executor thread, inside the
        // arena scope on the sync-built-in path. Must not block, must not
        // re-enter the engine, must not panic. `histogram!` is a lock-free
        // recorder write, and `is_enabled()` short-circuits when metrics are
        // off — nothing here can await or unwind.
        crate::metrics::record_task_duration(
            event.workflow_id,
            event.task_id,
            interned_function_name(event.function),
            event.duration.as_secs_f64(),
        );
    }

    fn workflow_finished(&self, event: &WorkflowFinished<'_>) {
        // Same contract as `task_finished`: synchronous, on the executor
        // thread, must not block or unwind. `histogram!` is a lock-free
        // recorder write.
        //
        // `event.sweeps` is deliberately not a label. A looping workflow
        // reports one event for the whole loop, and the sweep count is bounded
        // only by the data — making it a label would let a payload grow the
        // Prometheus label space, which is the one thing these metrics are
        // careful never to allow.
        crate::metrics::record_workflow_duration(event.workflow_id, event.duration.as_secs_f64());
    }
}

/// Map a borrowed function name onto the `&'static str` from Orion's own
/// vocabulary.
///
/// `metrics` labels want a `&'static str` or an owned `String`; borrowing from
/// the known set avoids allocating per task, and it doubles as a cardinality
/// bound — a name Orion does not recognise cannot mint a new label value. The
/// scan is over ~20 entries and runs once per dispatched task, which is noise
/// next to any task body.
fn interned_function_name(name: &str) -> &'static str {
    super::known_functions()
        .find(|known| *known == name)
        .unwrap_or("other")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_known_function_interns_to_itself() {
        for name in super::super::known_functions() {
            assert_eq!(interned_function_name(name), name);
        }
    }

    /// An unrecognised name must not become a label value of its own. Nothing
    /// can reach the engine under an unknown name today — `is_known_function`
    /// gates workflow creation — but a metric label is the wrong place to find
    /// that out.
    #[test]
    fn an_unknown_function_collapses_to_one_label() {
        assert_eq!(interned_function_name("enrich"), "other");
        assert_eq!(interned_function_name("__nope__"), "other");
    }

    /// The observer must survive the names the executor actually reports,
    /// including `validate` standing in for both spellings of the validation
    /// config.
    #[test]
    fn the_validation_alias_the_executor_reports_is_known() {
        assert_eq!(interned_function_name("validate"), "validate");
    }

    /// The whole justification for attaching an observer: the sync built-ins
    /// are reported.
    ///
    /// `map` never reaches the handler registry — it is dispatched inside a
    /// private executor method — so if a future dataflow-rs stopped emitting
    /// events for that path, `orion_task_duration_seconds` would go quiet for
    /// the six functions it exists to cover, with nothing else failing. This
    /// pins the dependency rather than the arithmetic.
    #[tokio::test]
    async fn a_sync_builtin_is_reported_to_the_observer() {
        use std::sync::Mutex;

        #[derive(Default)]
        struct Seen(Mutex<Vec<(String, String)>>);
        impl ExecutionObserver for Seen {
            fn task_finished(&self, e: &TaskEvent<'_>) {
                self.0
                    .lock()
                    .expect("test")
                    .push((e.task_id.to_string(), e.function.to_string()));
            }
        }

        let seen = std::sync::Arc::new(Seen::default());
        let workflow = dataflow_rs::Workflow::from_json(
            r#"{"id":"w","name":"w","priority":0,"condition":true,
                "tasks":[{"id":"m","name":"m","function":{"name":"map","input":
                  {"mappings":[{"path":"data.ok","logic":true}]}}}]}"#,
        )
        .expect("workflow parses");
        let engine = dataflow_rs::Engine::new(vec![workflow], std::collections::HashMap::new())
            .expect("engine builds")
            .with_observer(seen.clone());

        let mut message = dataflow_rs::Message::builder().build();
        engine
            .process_message(&mut message)
            .await
            .expect("run succeeds");

        assert_eq!(
            *seen.0.lock().expect("test"),
            vec![("m".to_string(), "map".to_string())],
            "a built-in dispatched inside the executor must still be timed"
        );
    }

    /// The workflow lifecycle callback fires, and its duration covers the task
    /// bodies inside it — which is what makes the subtraction that yields
    /// engine overhead meaningful.
    ///
    /// A skipped workflow must *not* report: the overhead figure is per run,
    /// and counting a workflow the condition rejected would dilute it with
    /// runs that never happened.
    #[tokio::test]
    async fn a_workflow_reports_once_and_only_when_it_runs() {
        use std::sync::Mutex;
        use std::time::Duration;

        #[derive(Default)]
        struct Seen {
            workflows: Mutex<Vec<(String, Duration)>>,
            tasks: Mutex<Duration>,
        }
        impl ExecutionObserver for Seen {
            fn task_finished(&self, e: &TaskEvent<'_>) {
                *self.tasks.lock().expect("test") += e.duration;
            }
            fn workflow_finished(&self, e: &WorkflowFinished<'_>) {
                self.workflows
                    .lock()
                    .expect("test")
                    .push((e.workflow_id.to_string(), e.duration));
            }
        }

        fn workflow(id: &str, condition: bool) -> dataflow_rs::Workflow {
            dataflow_rs::Workflow::from_json(&format!(
                r#"{{"id":"{id}","name":"{id}","priority":0,"condition":{condition},
                    "tasks":[{{"id":"m","name":"m","function":{{"name":"map","input":
                      {{"mappings":[{{"path":"data.ok","logic":true}}]}}}}}}]}}"#
            ))
            .expect("workflow parses")
        }

        let seen = std::sync::Arc::new(Seen::default());
        let engine = dataflow_rs::Engine::new(
            vec![workflow("runs", true), workflow("skipped", false)],
            std::collections::HashMap::new(),
        )
        .expect("engine builds")
        .with_observer(seen.clone());

        let mut message = dataflow_rs::Message::builder().build();
        engine
            .process_message(&mut message)
            .await
            .expect("run succeeds");

        let workflows = seen.workflows.lock().expect("test");
        assert_eq!(
            workflows
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["runs"],
            "a workflow its condition rejected never starts, so it never finishes"
        );
        assert!(
            workflows[0].1 >= *seen.tasks.lock().expect("test"),
            "the workflow's own clock must cover the task bodies inside it, or \
             subtracting them for the overhead figure would go negative"
        );
    }
}
