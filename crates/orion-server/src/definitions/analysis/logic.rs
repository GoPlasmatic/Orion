//! Expressions as the engine sees them: compiled once, with what the
//! compiler could prove about them.
//!
//! Every judgement here is the engine's own — `Logic::is_constant` is the
//! datalogic compiler reporting that it folded the whole rule to a literal,
//! and [`Evaluator::evaluate`] runs the same evaluator the serving engine
//! runs, with Orion's operators registered. A rule that uses these is not
//! predicting what the engine will do; it is asking it.

use std::collections::BTreeSet;

use dataflow_rs::datalogic_rs;
use serde_json::{Value, json};

use super::dataflow::{self, Reads};
use super::operators;

/// One authored expression and what is known about it.
#[derive(Debug, Clone)]
pub struct Expr {
    pub value: Value,
    /// `Some` when the compiler folded the whole expression to this literal.
    pub constant: Option<Value>,
    /// `false` when the engine refused it — `lint` reports that; rules treat
    /// the expression as unknown.
    pub compiles: bool,
    pub reads: Reads,
    /// Every operator name appearing as a single-key object anywhere in it.
    /// Over-inclusive on purpose (a data key that happens to be an operator
    /// name is counted), which can only make a rule more silent.
    pub operators: BTreeSet<String>,
}

impl Expr {
    /// The two values the engine treats as "do not run": exactly `false` and
    /// `null`. Nothing else is claimed — `0` and `""` are falsy too, but the
    /// rules that use this want a bar no truthiness rule can move.
    pub fn is_constant_falsy(&self) -> bool {
        matches!(self.constant, Some(Value::Bool(false)) | Some(Value::Null))
    }

    pub fn is_constant_true(&self) -> bool {
        matches!(self.constant, Some(Value::Bool(true)))
    }

    /// Whether the expression's result depends on anything but the context.
    pub fn nondeterministic(&self) -> bool {
        self.operators
            .iter()
            .any(|op| operators::NONDETERMINISTIC.contains(&op.as_str()))
    }
}

/// A datalogic engine with Orion's operators, for offline judgement.
pub struct Evaluator {
    engine: datalogic_rs::Engine,
}

impl Default for Evaluator {
    fn default() -> Self {
        Self::new()
    }
}

impl Evaluator {
    pub fn new() -> Self {
        Self {
            engine: crate::engine::operators::add_to_datalogic(datalogic_rs::Engine::builder())
                .build(),
        }
    }

    pub fn expr(&self, value: &Value) -> Expr {
        let compiled = self.engine.compile(value).ok();
        let constant = compiled
            .as_ref()
            .filter(|logic| logic.is_constant())
            .and_then(|_| self.evaluate(value, &json!({})));
        let mut ops = BTreeSet::new();
        collect_operators(value, &mut ops);
        Expr {
            value: value.clone(),
            constant,
            compiles: compiled.is_some(),
            reads: dataflow::reads(value),
            operators: ops,
        }
    }

    /// Evaluate `value` against `context` exactly as the serving engine
    /// would. `None` when the engine refuses or errors — never a guess.
    pub fn evaluate(&self, value: &Value, context: &Value) -> Option<Value> {
        self.engine.eval_into::<Value, _, _>(value, context).ok()
    }
}

fn collect_operators(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Array(items) => items.iter().for_each(|v| collect_operators(v, out)),
        Value::Object(map) => {
            if map.len() == 1 {
                let key = map.keys().next().expect("one member");
                if crate::engine::operators::is_operator(key) {
                    out.insert(key.clone());
                }
            }
            map.values().for_each(|v| collect_operators(v, out));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_are_the_compilers_verdict() {
        let ev = Evaluator::new();
        assert!(ev.expr(&json!(false)).is_constant_falsy());
        assert!(ev.expr(&json!(true)).is_constant_true());
        assert!(ev.expr(&json!({"==": [1, 2]})).is_constant_falsy());
        assert!(
            ev.expr(&json!({"and": [true, {"==": [1, 1]}]}))
                .is_constant_true()
        );
        assert!(!ev.expr(&json!({"var": "data.x"})).is_constant_falsy());
        assert_eq!(ev.expr(&json!({"var": "data.x"})).constant, None);
    }

    #[test]
    fn evaluation_runs_the_real_engine_with_orion_operators() {
        let ev = Evaluator::new();
        assert_eq!(
            ev.evaluate(
                &json!({"==": [{"var": "data.type"}, "order"]}),
                &json!({"data": {}})
            ),
            Some(json!(false))
        );
        assert_eq!(
            ev.evaluate(&json!({"!": {"var": "data.x"}}), &json!({"data": {}})),
            Some(json!(true))
        );
        assert_eq!(
            ev.evaluate(&json!({"hex_encode": ["a"]}), &json!({})),
            Some(json!("61")),
            "Orion's own operators are registered"
        );
    }

    #[test]
    fn nondeterminism_is_seen_at_any_depth() {
        let ev = Evaluator::new();
        assert!(
            ev.expr(&json!({">": [{"var": "data.t"}, {"now": []}]}))
                .nondeterministic()
        );
        assert!(
            !ev.expr(&json!({">": [{"var": "data.t"}, 1]}))
                .nondeterministic()
        );
    }
}
