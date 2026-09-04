//! Facts about a definition set, computed once, for every rule to read.
//!
//! A rule never walks `tasks` itself. It reads [`Analysis`] — the flattened
//! steps of every workflow with what each one reads, writes and is
//! conditioned on, every expression compiled by the engine, the channel →
//! workflow binding, the config the user passed with `-c` — and decides.
//! Adding a fact here is how the next rule gets it without a second walk,
//! and a walk over authored steps that lives in one place is a walk that
//! sees task groups (`engine/steps.rs` records what the alternative cost).
//!
//! Two forms of the set are held. The **compiled** form is what the engine
//! will run and is what every semantic rule reads. The **source** form —
//! `use` and `$from` intact — is what the duplication rules read, because
//! after expansion every fragment call site *is* a repeated sequence.

pub mod dataflow;
pub mod keys;
pub mod logic;
pub mod operators;

use std::collections::BTreeMap;

use serde_json::Value;

use crate::config::AppConfig;
use crate::definitions::json::Document;
use crate::definitions::{DefinitionSet, Entity, SharedDefinitions};
use crate::engine::FunctionRegistry;

pub use dataflow::Reads;
pub use logic::{Evaluator, Expr};

/// The context the engine has when it evaluates a workflow's `condition` to
/// decide whether the workflow matches: `data` and `temp_data` are empty —
/// the request body became the *payload*, which is not in the context — and
/// only `metadata` carries anything. Established in the data routes
/// (`routes/data/mod.rs` → `sync.rs`'s `payload_json`) and in `channel_call`.
pub fn selection_context() -> Value {
    serde_json::json!({ "data": {}, "temp_data": {}, "metadata": {} })
}

/// Everything the rules read.
pub struct Analysis<'a> {
    pub source: &'a DefinitionSet,
    pub compiled: &'a DefinitionSet,
    pub shared: &'a SharedDefinitions,
    /// The serving instance's config, when `-c` named it. The rules that
    /// need it declare so and are skipped otherwise.
    pub config: Option<&'a AppConfig>,
    /// Every function a workflow may name, and what each declares — the
    /// built-in registry offline, extended by whatever manifests were given.
    /// The write and expression facts below are read from it, so a rule never
    /// consults a function table itself.
    pub functions: &'a FunctionRegistry,
    pub evaluator: Evaluator,
    /// One entry per workflow of the compiled set, in set order.
    pub workflows: Vec<WorkflowFacts>,
    /// Channel name → the `workflow_id` it is bound to.
    pub channels: BTreeMap<String, String>,
    /// Source documents parsed with the span-carrying front end, keyed by
    /// origin, for documents whose source and compiled forms have the same
    /// coordinates (no `use`, no `$from`). A diagnostic on any other document
    /// carries a path but no line.
    documents: BTreeMap<String, Document>,
}

/// One workflow, flattened.
pub struct WorkflowFacts {
    pub origin: String,
    pub name: String,
    pub workflow_id: Option<String>,
    /// The workflow-level condition; `true` when absent, as the engine reads it.
    pub condition: Expr,
    pub has_loop: bool,
    /// `temp_data.<counter>` when a loop declares a counter.
    pub loop_counter: Option<String>,
    /// Every step in document (pre-)order: a group precedes its members.
    pub steps: Vec<StepFacts>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    Task,
    Group,
}

/// One authored step.
pub struct StepFacts {
    /// `tasks[1].tasks[0]`.
    pub path: String,
    /// The list it sits in: `tasks`, `tasks[1].tasks`.
    pub list: String,
    /// Index into `steps` of the enclosing group, if any.
    pub parent: Option<usize>,
    pub kind: StepKind,
    pub id: String,
    pub node: Value,
    pub condition: Option<Expr>,
    pub terminal: bool,
    /// Runs whenever the list it sits in is reached: no condition (or one the
    /// compiler folded to `true`) on it or on any enclosing group.
    pub certain: bool,
    pub function: Option<String>,
    /// The expressions the engine evaluates in this step's input, by path
    /// relative to `function.input`.
    pub expressions: Vec<(String, Expr)>,
    /// Context paths this step writes (a task), or all its members write (a
    /// group).
    pub writes: Vec<String>,
    /// Whether [`Self::writes`] is the complete list. False when a destination
    /// is authored as an expression, so the step may write somewhere this
    /// analysis cannot name — a rule reasoning about overwrites must then stay
    /// silent, exactly as it does for [`Reads::uncertain`].
    ///
    /// [`Reads::uncertain`]: dataflow::Reads::uncertain
    pub writes_uncertain: bool,
}

impl StepFacts {
    /// Every read this step makes that the walk could name, with whether the
    /// list is complete: the condition's and each input expression's.
    pub fn reads(&self) -> Reads {
        let mut out = Reads::default();
        for expr in self
            .condition
            .iter()
            .chain(self.expressions.iter().map(|(_, e)| e))
        {
            out.paths.extend(expr.reads.paths.iter().cloned());
            out.computed |= expr.reads.computed;
            out.scoped |= expr.reads.scoped;
        }
        out
    }

    pub fn is_unconditional(&self) -> bool {
        self.condition.as_ref().is_none_or(Expr::is_constant_true)
    }
}

impl<'a> Analysis<'a> {
    pub fn new(
        source: &'a DefinitionSet,
        compiled: &'a DefinitionSet,
        shared: &'a SharedDefinitions,
        config: Option<&'a AppConfig>,
        functions: &'a FunctionRegistry,
    ) -> Self {
        let evaluator = Evaluator::new();
        let workflows = compiled
            .iter(Entity::Workflow)
            .map(|def| workflow_facts(&def.origin, &def.doc, &evaluator, functions))
            .collect();
        let channels = compiled
            .iter(Entity::Channel)
            .filter_map(|def| {
                Some((
                    def.doc.get("name")?.as_str()?.to_string(),
                    def.doc.get("workflow_id")?.as_str()?.to_string(),
                ))
            })
            .collect();
        // The set already carries each document's spans — parsed once, when it
        // was loaded. This used to re-read every file from disk and parse it a
        // third time, which was both wasteful and wrong for an artifact or a
        // single in-memory document, neither of which has a file to re-read.
        //
        // The `$from`/`use` filter stays: after expansion a compiled path no
        // longer addresses source coordinates, so locating one would point at
        // the wrong node rather than at none. Fixing *that* needs the passes to
        // record a compiled-path → source-path remap, which is its own change.
        let documents = source
            .definitions
            .iter()
            .filter(|def| crate::definitions::compile::residue(&def.doc, "").is_empty())
            .filter_map(|def| Some((def.origin.clone(), def.spans.clone()?)))
            .collect();
        Self {
            source,
            compiled,
            shared,
            config,
            functions,
            evaluator,
            workflows,
            channels,
            documents,
        }
    }

    /// `(line, column)` of `path` in the source file at `origin`, when the
    /// source has the same coordinates as the compiled form.
    pub fn locate(&self, origin: &str, path: &str) -> Option<(usize, usize)> {
        let doc = self.documents.get(origin)?;
        let span = doc.locate(path)?;
        Some(doc.line_col(span.start))
    }
}

fn workflow_facts(
    origin: &str,
    doc: &Value,
    evaluator: &Evaluator,
    functions: &FunctionRegistry,
) -> WorkflowFacts {
    let condition = evaluator.expr(doc.get("condition").unwrap_or(&Value::Bool(true)));
    let loop_config = doc.get("loop").filter(|l| !l.is_null());
    let loop_counter = loop_config
        .and_then(|l| l.get("counter"))
        .and_then(Value::as_str)
        .map(|c| format!("temp_data.{c}"));
    let mut steps = Vec::new();
    if let Some(tasks) = doc.get("tasks") {
        walk(tasks, "tasks", None, true, evaluator, functions, &mut steps);
    }
    WorkflowFacts {
        origin: origin.to_string(),
        name: doc
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        workflow_id: doc
            .get("workflow_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        condition,
        has_loop: loop_config.is_some(),
        loop_counter,
        steps,
    }
}

/// Pre-order walk of a `tasks` array. The group test is the engine's own
/// (`is_group`: the presence of a `tasks` key), so this and every other walk
/// in the crate agree about what a step is.
fn walk(
    tasks: &Value,
    list: &str,
    parent: Option<usize>,
    parent_certain: bool,
    evaluator: &Evaluator,
    functions: &FunctionRegistry,
    out: &mut Vec<StepFacts>,
) {
    let Some(items) = tasks.as_array() else {
        return;
    };
    for (index, node) in items.iter().enumerate() {
        let path = format!("{list}[{index}]");
        let condition = node.get("condition").map(|c| evaluator.expr(c));
        let certain = parent_certain && condition.as_ref().is_none_or(Expr::is_constant_true);
        let terminal = node
            .get("terminal")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let id = node
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let kind = if crate::engine::is_group(node) {
            StepKind::Group
        } else {
            StepKind::Task
        };
        let function = node
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let expressions = match (&function, node.get("function").and_then(|f| f.get("input"))) {
            (Some(name), Some(input)) => operators::input_expressions(name, input, functions)
                .into_iter()
                .map(|(p, e)| (p, evaluator.expr(e)))
                .collect(),
            _ => Vec::new(),
        };
        let (writes, writes_uncertain) = match kind {
            StepKind::Task => {
                let facts = dataflow::task_write_facts(node, functions);
                (facts.paths, facts.computed)
            }
            // Filled in after the members are walked.
            StepKind::Group => (Vec::new(), false),
        };
        let me = out.len();
        out.push(StepFacts {
            path: path.clone(),
            list: list.to_string(),
            parent,
            kind,
            id,
            node: node.clone(),
            condition,
            terminal,
            certain,
            function,
            expressions,
            writes,
            writes_uncertain,
        });
        if kind == StepKind::Group
            && let Some(members) = node.get("tasks")
        {
            walk(
                members,
                &format!("{path}.tasks"),
                Some(me),
                certain,
                evaluator,
                functions,
                out,
            );
            let member_writes: Vec<String> = out[me + 1..]
                .iter()
                .flat_map(|s| s.writes.iter().cloned())
                .collect();
            out[me].writes = member_writes;
            // A group is as certain as its least certain member — over the
            // same descendants `member_writes` collects from.
            out[me].writes_uncertain = out[me + 1..].iter().any(|s| s.writes_uncertain);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn analysis_of(doc: Value) -> Vec<StepFacts> {
        workflow_facts(
            "wf.json",
            &doc,
            &Evaluator::new(),
            FunctionRegistry::builtin(),
        )
        .steps
    }

    #[test]
    fn steps_are_flattened_in_document_order_with_certainty() {
        let steps = analysis_of(json!({
            "name": "w",
            "tasks": [
                {"id": "a", "name": "A", "function": {"name": "parse_json", "input": {"source": "payload", "target": "req"}}},
                {"id": "g", "condition": {"var": "data.flag"}, "terminal": true, "tasks": [
                    {"id": "b", "name": "B", "function": {"name": "map", "input": {"mappings": [{"path": "data.x", "logic": {"var": "data.req.x"}}]}}},
                    {"id": "c", "name": "C", "condition": true, "function": {"name": "log", "input": {"message": "hi"}}}
                ]},
                {"id": "d", "name": "D", "condition": {"==": [1, 1]}, "function": {"name": "log", "input": {"message": "hi"}}}
            ]
        }));
        let paths: Vec<&str> = steps.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "tasks[0]",
                "tasks[1]",
                "tasks[1].tasks[0]",
                "tasks[1].tasks[1]",
                "tasks[2]"
            ]
        );
        assert!(steps[0].certain);
        assert!(!steps[1].certain, "a conditional group");
        assert!(!steps[2].certain, "inside a conditional group");
        assert!(steps[2].is_unconditional(), "but unconditional itself");
        assert!(
            steps[4].certain,
            "a condition folded to true is no condition"
        );
        assert_eq!(steps[1].kind, StepKind::Group);
        assert!(steps[1].terminal);
        assert_eq!(
            steps[1].writes,
            ["data.x"],
            "a group writes what its members write"
        );
        assert_eq!(steps[2].parent, Some(1));
        assert_eq!(steps[0].writes, ["data.req"]);
        assert_eq!(steps[2].reads().paths, ["data.req.x"]);
        assert_eq!(steps[1].reads().paths, ["data.flag"]);
    }

    #[test]
    fn a_loop_counter_is_named_as_a_temp_data_path() {
        let facts = workflow_facts(
            "wf.json",
            &json!({"name": "w", "loop": {"counter": "i", "max": 3}, "tasks": []}),
            &Evaluator::new(),
            FunctionRegistry::builtin(),
        );
        assert!(facts.has_loop);
        assert_eq!(facts.loop_counter.as_deref(), Some("temp_data.i"));
        assert!(
            facts.condition.is_constant_true(),
            "absent condition is true"
        );
    }

    /// The scoping table must classify the whole vocabulary — a new operator
    /// fails this until someone decides which side it is on.
    #[test]
    fn every_operator_is_classified() {
        for op in crate::engine::operators::operator_names() {
            let scoping = operators::SCOPING.contains(&op.as_str());
            let plain = operators::NON_SCOPING.contains(&op.as_str());
            assert!(
                scoping ^ plain,
                "operator `{op}` must be in exactly one of SCOPING / NON_SCOPING"
            );
        }
        for op in operators::SCOPING.iter().chain(operators::NON_SCOPING) {
            assert!(
                crate::engine::operators::is_operator(op),
                "`{op}` is classified but is not an operator this build registers"
            );
        }
    }

    /// The claim behind `SCOPING`: inside those operators' later arguments a
    /// `var` reads the element, not the context.
    #[test]
    fn scoping_operators_rebind_var_to_the_element() {
        let ev = Evaluator::new();
        let ctx = json!({"data": {"items": [{"payload": 1}, {"payload": 2}]}, "payload": 99});
        assert_eq!(
            ev.evaluate(
                &json!({"map": [{"var": "data.items"}, {"var": "payload"}]}),
                &ctx
            ),
            Some(json!([1, 2]))
        );
        assert_eq!(
            ev.evaluate(
                &json!({"filter": [{"var": "data.items"}, {"==": [{"var": "payload"}, 2]}]}),
                &ctx
            ),
            Some(json!([{"payload": 2}]))
        );
        assert_eq!(
            ev.evaluate(
                &json!({"some": [{"var": "data.items"}, {"==": [{"var": "payload"}, 2]}]}),
                &ctx
            ),
            Some(json!(true))
        );
    }
}
