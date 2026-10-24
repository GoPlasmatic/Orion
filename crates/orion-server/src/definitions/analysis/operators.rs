//! What the analysis knows about individual operators and functions: which
//! operators rebind `var` for their later arguments, and where in a task's
//! input the engine evaluates JSONLogic.
//!
//! Operator semantics come from dataflow-rs's live catalogue. Logic-bearing
//! fields come from Orion's function registry and the built-ins below.

use serde_json::Value;

use crate::engine::FunctionRegistry;

/// The live operator metadata, including aliases and Orion's registrations.
/// Kept once for the analysis; the engine supplies scope and effect semantics.
fn catalogue() -> &'static std::collections::BTreeMap<String, dataflow_rs::expr::Operator> {
    static OPERATORS: std::sync::OnceLock<
        std::collections::BTreeMap<String, dataflow_rs::expr::Operator>,
    > = std::sync::OnceLock::new();
    OPERATORS.get_or_init(|| {
        crate::engine::operators::orion_exprs(&crate::engine::ResolvedSecrets::empty(), 0)
            .expect("Orion's operator registrations are valid")
            .operators()
            .iter()
            .cloned()
            .map(|op| (op.name.to_string(), op))
            .collect()
    })
}

/// Whether later arguments can read a rebound element rather than the context.
/// Unknown operators are conservative, so an analysis cannot prove a false read.
pub fn is_scoping(op: &str) -> bool {
    catalogue().get(op).is_none_or(|op| op.scoping)
}

/// Secrets depend on the engine's store, even though their reads are deterministic.
/// They must never be evaluated by an offline rule with an empty store.
pub fn is_nondeterministic(op: &str) -> bool {
    catalogue().get(op).is_none_or(|op| {
        !op.deterministic || op.family == dataflow_rs::expr::OperatorFamily::Secret
    })
}

/// The JSONLogic expressions the engine evaluates in a task's `input`, as
/// `(path relative to the input, expression)`. Covers the built-ins' logic
/// fields, every registry field whose `template_at` says dataflow-rs evaluates
/// it, and every field marked `resolvable` (a `{"var": …}` fold is a read too).
///
/// `template_at` is why an `http_call` path or header value counts. Before
/// dataflow-rs 3.9 those were static strings, so a `{"var": "payload.x"}` in
/// one was inert text and reporting it as a read would have been wrong; now it
/// is evaluated, and a rule that could not see it was under-reporting.
pub fn input_expressions<'a>(
    function: &str,
    input: &'a Value,
    functions: &FunctionRegistry,
) -> Vec<(String, &'a Value)> {
    let mut out = Vec::new();
    let Some(map) = input.as_object() else {
        return out;
    };
    let each = |key: &str, member: &str, out: &mut Vec<(String, &'a Value)>| {
        if let Some(items) = map.get(key).and_then(Value::as_array) {
            for (i, item) in items.iter().enumerate() {
                if let Some(expr) = item.get(member) {
                    out.push((format!("{key}[{i}].{member}"), expr));
                }
            }
        }
    };
    let one = |key: &str, out: &mut Vec<(String, &'a Value)>| {
        if let Some(expr) = map.get(key) {
            out.push((key.to_string(), expr));
        }
    };
    match function {
        "map" => each("mappings", "logic", &mut out),
        "filter" => one("condition", &mut out),
        "validation" => each("rules", "logic", &mut out),
        "log" => {
            one("message", &mut out);
            if let Some(fields) = map.get("fields").and_then(Value::as_object) {
                for (name, expr) in fields {
                    out.push((format!("fields.{name}"), expr));
                }
            }
        }
        // `channel_call`'s `channel` and `data` are not listed here: they are
        // ordinary `template_at` fields now, found by the sweep below along
        // with every other one, and their pre-1.0 `*_logic` spellings resolve
        // through the same registry aliases.
        _ => {}
    }
    for (field, value) in map {
        if out.iter().any(|(p, _)| p == field) {
            continue;
        }
        let template_at = functions.template_paths(function, field);
        // `""` — the field's own value is the expression. `"*"` — the field is
        // a map whose *values* are, so the map itself is not one.
        if template_at.contains(&"") {
            out.push((field.clone(), value));
        } else if template_at.contains(&"*") {
            if let Some(members) = value.as_object() {
                for (name, expr) in members {
                    out.push((format!("{field}.{name}"), expr));
                }
            }
        } else if functions.is_resolvable_field(function, field) {
            out.push((field.clone(), value));
        }
    }
    out
}
