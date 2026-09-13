//! `engine.ops_budget` reaches the engine and bites where the configuration
//! reference says it does.
//!
//! The setting is threaded through `engine::operators::with_ops_budget` at
//! every serving builder site and at `build_single`, which is the one an
//! integration test can drive directly. Two things are pinned: a ceiling a
//! `map` mapping crosses fails the *task* with `BUDGET_EXCEEDED` (a handler
//! evaluation has an error channel), and `0` is "no ceiling" rather than "a
//! ceiling of zero" — the same mapping runs to completion.

use std::collections::HashMap;

use dataflow_rs::datavalue::OwnedDataValue;
use serde_json::json;

/// A mapping that visits every item of `data.items`, so its cost grows with
/// the input and a small ceiling is crossed for certain.
fn sweep_workflow() -> dataflow_rs::Workflow {
    dataflow_rs::Workflow::from_json(
        &json!({
            "id": "sweep", "name": "sweep", "priority": 0, "condition": true,
            "tasks": [{
                "id": "double", "name": "double",
                "function": { "name": "map", "input": { "mappings": [{
                    "path": "data.doubled",
                    "logic": { "map": [{ "var": "data.items" }, { "*": [{ "var": "" }, 2] }] }
                }] } }
            }]
        })
        .to_string(),
    )
    .expect("a valid workflow")
}

/// The same sweep as a *custom* function's template: `crypto.data` is a
/// `template_at` field, so the expression is resolved by the handler through
/// `Template::resolve`, which has an error channel the built-in `map` does
/// not use.
fn crypto_sweep_workflow() -> dataflow_rs::Workflow {
    dataflow_rs::Workflow::from_json(
        &json!({
            "id": "sweep", "name": "sweep", "priority": 0, "condition": true,
            "tasks": [{
                "id": "digest", "name": "digest",
                "function": { "name": "crypto", "input": {
                    "op": "hash",
                    "data": { "map": [{ "var": "data.items" }, { "*": [{ "var": "" }, 2] }] },
                    "output": "data.digest"
                } }
            }]
        })
        .to_string(),
    )
    .expect("a valid workflow")
}

/// Run the sweep under `ops_budget`. The refusal is surfaced the way the
/// engine surfaces a failed task: as the `Err` of `process_message` for a
/// task without `continue_on_error`, with the message's own error list as
/// the other place a caller looks. Both are returned so the assertions can
/// read whichever carries it.
async fn run(ops_budget: u64) -> (Result<(), String>, dataflow_rs::Message) {
    run_workflow(sweep_workflow(), HashMap::new(), ops_budget).await
}

async fn run_workflow(
    workflow: dataflow_rs::Workflow,
    handlers: HashMap<String, dataflow_rs::BoxedFunctionHandler>,
    ops_budget: u64,
) -> (Result<(), String>, dataflow_rs::Message) {
    let engine = orion::engine::build_single(
        workflow,
        handlers,
        &orion::engine::ResolvedSecrets::empty(),
        ops_budget,
    )
    .expect("the sweep builds under any budget — compiling charges nothing");
    let mut message = dataflow_rs::Message::from_value(&json!({}));
    let items: Vec<u32> = (0..500).collect();
    dataflow_rs::engine::utils::set_nested_value(
        &mut message.context,
        "data",
        OwnedDataValue::from(&json!({ "items": items })),
    );
    let outcome = engine
        .process_message(&mut message)
        .await
        .map(|_| ())
        .map_err(|e| format!("{e:?}"));
    (outcome, message)
}

/// The built-in `map` collects its mappings' failures into one `500` and
/// keeps the reason for the log, so what a caller sees is a failed task and
/// an untouched target — not the upstream code. That is the engine's
/// behaviour for every mapping error, and the configuration reference says
/// so rather than promising a code this surface does not carry.
#[tokio::test]
async fn a_mapping_that_crosses_the_ceiling_fails_its_task() {
    let (outcome, message) = run(50).await;
    let rendered = format!("{outcome:?} {:?}", message.errors());
    assert!(
        rendered.contains("Task double failed with status 500"),
        "500 items under a ceiling of 50 operations must fail the task: {rendered}"
    );
    let data: serde_json::Value = message.data().into();
    assert!(
        data.get("doubled").is_none(),
        "a refused evaluation writes nothing: {data}"
    );
}

/// A custom function's template field resolves through `Template::resolve`,
/// which surfaces the refusal as `DataflowError::BudgetExceeded` — code
/// `BUDGET_EXCEEDED` on the message, which is what a trace and a channel
/// error body carry, and which the retry loop classifies as not retryable.
#[tokio::test]
async fn a_template_field_that_crosses_the_ceiling_fails_with_budget_exceeded() {
    let mut handlers: HashMap<String, dataflow_rs::BoxedFunctionHandler> = HashMap::new();
    handlers.insert(
        "crypto".to_string(),
        Box::new(orion::engine::functions::crypto::CryptoHandler),
    );
    let (outcome, message) = run_workflow(crypto_sweep_workflow(), handlers, 50).await;
    let rendered = format!("{outcome:?} {:?}", message.errors());
    assert!(
        rendered.contains("BUDGET_EXCEEDED"),
        "the refusal carries the upstream code so a caller can tell it from a logic \
         error: {rendered}"
    );
    let data: serde_json::Value = message.data().into();
    assert!(
        data.get("digest").is_none(),
        "a refused evaluation writes nothing: {data}"
    );

    // And the same template under no ceiling digests the sweep.
    let mut handlers: HashMap<String, dataflow_rs::BoxedFunctionHandler> = HashMap::new();
    handlers.insert(
        "crypto".to_string(),
        Box::new(orion::engine::functions::crypto::CryptoHandler),
    );
    let (outcome, message) = run_workflow(crypto_sweep_workflow(), handlers, 0).await;
    assert_eq!(outcome, Ok(()), "{:?}", message.errors());
    let data: serde_json::Value = message.data().into();
    assert!(data["digest"].is_string(), "{data}");
}

#[tokio::test]
async fn zero_is_no_ceiling() {
    let (outcome, message) = run(0).await;
    assert_eq!(outcome, Ok(()));
    assert!(message.errors().is_empty(), "{:?}", message.errors());
    let data: serde_json::Value = message.data().into();
    let doubled = data["doubled"].as_array().expect("the sweep ran");
    assert_eq!(doubled.len(), 500);
    assert_eq!(doubled[499], json!(998));
}

/// A generous ceiling is invisible: the same sweep completes under it, which
/// is the property that makes the setting safe to turn on with headroom.
#[tokio::test]
async fn a_ceiling_the_evaluation_stays_under_changes_nothing() {
    let (outcome, message) = run(100_000).await;
    assert_eq!(outcome, Ok(()));
    let data: serde_json::Value = message.data().into();
    assert_eq!(data["doubled"].as_array().map(Vec::len), Some(500));
}
