//! `engine.ops_budget` reaches the engine and bites where the configuration
//! reference says it does.
//!
//! The setting is the ceiling `engine::operators::orion_exprs` builds every
//! engine's expressions with, at every serving builder site; a test drives it
//! through an engine built on those expressions directly. Two things are
//! pinned: a ceiling an evaluation crosses fails the *task* (a handler
//! evaluation has an error channel), and `0` is "no ceiling" rather than "a
//! ceiling of zero" — the same mapping runs to completion.

use dataflow_rs::datavalue::OwnedDataValue;
use serde_json::json;

/// A mapping that visits every item of `data.items`, so its cost grows with
/// the input and a small ceiling is crossed for certain.
fn sweep_workflow() -> serde_json::Value {
    json!({
        "id": "sweep",
        "tasks": [{
            "id": "double",
            "function": { "name": "map", "input": { "mappings": [{
                "path": "data.doubled",
                "logic": { "map": [{ "var": "data.items" }, { "*": [{ "var": "" }, 2] }] }
            }] } }
        }]
    })
}

/// The same sweep as a *custom* function's template: `crypto.data` is a
/// `template_at` field, so the expression is resolved by the function, which
/// has an error channel for the refusal.
fn crypto_sweep_workflow() -> serde_json::Value {
    json!({
        "id": "sweep",
        "tasks": [{
            "id": "digest",
            "function": { "name": "crypto", "input": {
                "op": "hash",
                "data": { "map": [{ "var": "data.items" }, { "*": [{ "var": "" }, 2] }] },
                "output": "data.digest"
            } }
        }]
    })
}

/// Run `workflow` under `ops_budget` over 500 items, on Orion's expressions
/// with `crypto` registered, and return the message it leaves.
async fn run_workflow(workflow: serde_json::Value, ops_budget: u64) -> dataflow_rs::Message {
    let exprs = crate::operators::orion_exprs(&crate::ResolvedSecrets::empty(), ops_budget)
        .expect("Orion's expressions build");
    let mut registry = dataflow_rs::Registry::new(exprs);
    registry.register("crypto", crate::functions::crypto::CryptoFunction);
    let engine = dataflow_rs::Engine::builder()
        .registry(registry)
        .workflow_json(&workflow)
        .build()
        .expect("the sweep builds under any budget — compiling charges nothing");
    let items: Vec<u32> = (0..500).collect();
    let data = OwnedDataValue::from(&json!({ "items": items }));
    let mut message = dataflow_rs::Message::from_parts(data.clone(), data, OwnedDataValue::Null);
    engine.run(&mut message).await;
    message
}

async fn run(ops_budget: u64) -> dataflow_rs::Message {
    run_workflow(sweep_workflow(), ops_budget).await
}

fn data(message: &dataflow_rs::Message) -> serde_json::Value {
    serde_json::Value::from(message.context().data())
}

/// A built-in `map` mapping that crosses the ceiling fails its task and
/// leaves its target untouched.
#[tokio::test]
async fn a_mapping_that_crosses_the_ceiling_fails_its_task() {
    let message = run(50).await;
    let record = message
        .errors()
        .first()
        .expect("500 items under a ceiling of 50 operations must fail the task");
    assert_eq!(record.code, "BUDGET_EXCEEDED", "{record:?}");
    assert!(
        data(&message).get("doubled").is_none(),
        "a refused evaluation writes nothing: {}",
        data(&message)
    );
}

/// A custom function's template field surfaces the refusal as the code
/// `BUDGET_EXCEEDED` on the message, which is what a trace and a channel
/// error body carry, and which the retry loop classifies as not retryable.
#[tokio::test]
async fn a_template_field_that_crosses_the_ceiling_fails_with_budget_exceeded() {
    let message = run_workflow(crypto_sweep_workflow(), 50).await;
    let rendered = format!("{:?}", message.errors());
    assert!(
        rendered.contains("BUDGET_EXCEEDED"),
        "the refusal carries the upstream code so a caller can tell it from a logic \
         error: {rendered}"
    );
    assert!(
        data(&message).get("digest").is_none(),
        "a refused evaluation writes nothing: {}",
        data(&message)
    );

    // And the same template under no ceiling digests the sweep.
    let message = run_workflow(crypto_sweep_workflow(), 0).await;
    assert!(message.errors().is_empty(), "{:?}", message.errors());
    assert!(data(&message)["digest"].is_string(), "{}", data(&message));
}

#[tokio::test]
async fn zero_is_no_ceiling() {
    let message = run(0).await;
    assert!(message.errors().is_empty(), "{:?}", message.errors());
    let data = data(&message);
    let doubled = data["doubled"].as_array().expect("the sweep ran");
    assert_eq!(doubled.len(), 500);
    assert_eq!(doubled[499], json!(998));
}

/// A generous ceiling is invisible: the same sweep completes under it, which
/// is the property that makes the setting safe to turn on with headroom.
#[tokio::test]
async fn a_ceiling_the_evaluation_stays_under_changes_nothing() {
    let message = run(100_000).await;
    assert!(message.errors().is_empty(), "{:?}", message.errors());
    assert_eq!(
        data(&message)["doubled"].as_array().map(Vec::len),
        Some(500)
    );
}
