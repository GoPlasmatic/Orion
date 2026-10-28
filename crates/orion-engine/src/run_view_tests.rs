//! Orion's expression registry preserves v4 fan-out isolation and ordered folds.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::function::{Ctx, Function, Outcome};
use dataflow_rs::observe::TraceOptions;
use dataflow_rs::run::{AuditMode, CancelToken, RunOptions, RunStatus};
use serde_json::{Value, json};

struct Probe {
    fail: bool,
    cancel: Option<CancelToken>,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl Function for Probe {
    type Config = ();

    async fn call(&self, ctx: &mut Ctx<'_>, _: &()) -> dataflow_rs::Result<Outcome> {
        let index = ctx.info().element_index.expect("fan-out element");
        let value = "data.value".parse().expect("path");
        assert_eq!(ctx.get(&value).as_deref(), Some(&OwnedDataValue::from(7)));
        assert_eq!(
            ctx.get(&"temp_data.item".parse().expect("path")).as_deref(),
            Some(&OwnedDataValue::from(index))
        );
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        ctx.set(&value, OwnedDataValue::from(index))?;
        // Exercise overlapping calls, including reads after another call writes.
        for _ in 0..(4 - index) {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            ctx.get(&value).as_deref(),
            Some(&OwnedDataValue::from(index))
        );
        self.active.fetch_sub(1, Ordering::SeqCst);
        if index == 1 {
            if let Some(cancel) = &self.cancel {
                cancel.cancel();
            }
            if self.fail {
                return Err(dataflow_rs::Error::service(
                    "PROBE_FAILED",
                    "element failed",
                ));
            }
        }
        Ok(Outcome::Ok)
    }
}

async fn exercise(concurrency: usize, trace: bool, fail: bool, cancel_run: bool) {
    let cancel = CancelToken::new();
    let peak = Arc::new(AtomicUsize::new(0));
    let exprs = crate::operators::orion_exprs(&crate::ResolvedSecrets::empty(), 0)
        .expect("Orion expressions");
    let mut registry = dataflow_rs::Registry::new(exprs);
    registry.register(
        "probe",
        Probe {
            fail,
            cancel: cancel_run.then(|| cancel.clone()),
            active: Arc::new(AtomicUsize::new(0)),
            peak: peak.clone(),
        },
    );
    let engine = dataflow_rs::Engine::builder()
        .registry(registry)
        .workflow_json(&json!({"id":"fan", "tasks":[{
            "id":"probe", "for_each":{
                "over":[0,1,2,3], "as":"item", "max_concurrency":concurrency,
                "collect":"data.value", "into":"data.results"
            }, "function":{"name":"probe"}
        }]}))
        .build()
        .expect("fan-out builds");
    let data = OwnedDataValue::from(&json!({"value":7, "unrelated":["x".repeat(65536)]}));
    let mut message =
        dataflow_rs::Message::from_parts(OwnedDataValue::Null, data, OwnedDataValue::Null);
    let mut opts = RunOptions::default().audit(AuditMode::Off).cancel(&cancel);
    if trace {
        opts = opts.trace(TraceOptions::default());
    }
    let run = engine.run_with(&mut message, &opts).await;
    assert_eq!(run.trace().is_some(), trace);
    if let Some(trace) = run.trace() {
        trace.validate().expect("well-formed trace");
        let replay = trace.contexts().last().expect("trace context");
        assert_eq!(replay.data(), message.context().data());
        assert_eq!(replay.temp_data(), message.context().temp_data());
    }
    let data = Value::from(message.context().data());
    assert_eq!(data["unrelated"][0].as_str().expect("kept").len(), 65536);
    assert!(
        message.context().temp_data().get("item").is_none(),
        "binding cannot escape"
    );
    if cancel_run {
        assert!(matches!(run.status(), RunStatus::Cancelled));
        assert_eq!(data["value"], 7);
        assert!(
            data.get("results").is_none(),
            "cancelled fan-out folds nothing"
        );
    } else if fail {
        assert!(message.errors().iter().any(|e| e.code == "PROBE_FAILED"));
        assert_eq!(data["results"], json!([0, null, null, null]));
        assert_eq!(data["value"], 0, "failed and later elements cannot write");
    } else {
        assert!(message.errors().is_empty(), "{:?}", message.errors());
        assert_eq!(data["results"], json!([0, 1, 2, 3]));
        assert_eq!(data["value"], 3, "fold follows element order");
        assert_eq!(peak.load(Ordering::SeqCst), concurrency);
    }
}

#[tokio::test]
async fn fan_out_keeps_overlays_isolated_and_folds_in_element_order() {
    for concurrency in [1, 4] {
        for trace in [false, true] {
            exercise(concurrency, trace, false, false).await;
        }
    }
}

#[tokio::test]
async fn fan_out_failure_keeps_only_the_successful_prefix() {
    for concurrency in [1, 4] {
        for trace in [false, true] {
            exercise(concurrency, trace, true, false).await;
        }
    }
}

#[tokio::test]
async fn fan_out_cancellation_discards_all_element_writes() {
    for concurrency in [1, 4] {
        for trace in [false, true] {
            exercise(concurrency, trace, false, true).await;
        }
    }
}
