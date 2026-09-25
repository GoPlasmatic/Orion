//! Exact rewrites a clippy rule can prove, and the edit that applies one to
//! a source file (#337). Pure: text and values in, text out, no I/O.
//!
//! A rule reads the **compiled** set; the edit lands in the **source** file.
//! Step ids bridge the two: they are the one coordinate a step keeps between
//! the forms, and a step an expansion produced (a fragment's `outer.inner`,
//! an `$each`'s `infer3`) has no source step with that id — so it is
//! reported, not fixed, with no extra code.
//!
//! Every edit is proven before it is kept. The fold is applied twice — to
//! the source tree and to the compiled document — and the real compiler run
//! over the edited source must produce exactly the folded compiled form. If
//! anything between the two (a `$from` merge, a fragment) made the source
//! edit mean something else, nothing is written.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use super::json::{Document, Member, Node, Span, Spanned};

/// An exact, meaning-preserving rewrite a rule can prove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fix {
    /// Fold the consecutive steps `members` — in order, one list, each with
    /// the same condition — into one task group carrying that condition.
    FoldRun {
        members: Vec<String>,
        group_id: String,
    },
}

impl std::fmt::Display for Fix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fix::FoldRun { members, group_id } => write!(
                f,
                "folded {} into group `{group_id}`",
                members
                    .iter()
                    .map(|m| format!("`{m}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// Why a fix was not applied. The finding stands; nothing was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The steps are not written, consecutively, in this file's own step
    /// lists — a fragment or another expansion produced them.
    NotAuthoredHere { members: Vec<String> },
    /// A member's condition is not written on the step itself.
    ConditionNotOnStep { step: String },
    /// The group id is already a step id in the workflow.
    IdCollision { id: String },
    /// Folding would nest groups past the engine's limit.
    TooDeep,
    /// The file could not be read by the order-preserving parser.
    NoSpans,
    /// The edited source does not compile to the folded compiled form.
    Verification,
    /// The edited file could not be formatted.
    Format(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::NotAuthoredHere { members } => write!(
                f,
                "steps {} are not written in this file — they come from a `use` fragment or \
                 another expansion, so the edit belongs there and would change every caller",
                members
                    .iter()
                    .map(|m| format!("`{m}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Refusal::ConditionNotOnStep { step } => {
                write!(f, "`{step}`'s condition is not written on the step itself")
            }
            Refusal::IdCollision { id } => write!(
                f,
                "the group would be named `{id}`, which is already a step id in this workflow"
            ),
            Refusal::TooDeep => write!(
                f,
                "folding would nest groups deeper than the engine's limit of {}",
                crate::engine::MAX_STEP_DEPTH
            ),
            Refusal::NoSpans => write!(
                f,
                "the file cannot be read by the order-preserving parser (a duplicate key?) — \
                 `orion-server fmt` reports why"
            ),
            Refusal::Verification => write!(
                f,
                "the edit did not compile to the folded workflow — a clippy bug; nothing was \
                 written, please report it with the file"
            ),
            Refusal::Format(reason) => {
                write!(f, "the edited file could not be formatted: {reason}")
            }
        }
    }
}

/// What applying a file's fixes produced.
#[derive(Debug, Default)]
pub struct FileOutcome {
    /// The new file text — formatted — when at least one fix was applied.
    pub text: Option<String>,
    /// The compiled document with every applied fix folded in.
    pub compiled: Option<Value>,
    pub applied: Vec<Fix>,
    pub refused: Vec<(Fix, Refusal)>,
}

/// Apply `fixes` to one source file, in order, each proven before it is
/// kept. `recompile` is the real authoring pipeline for this file; a fix
/// whose edited source does not compile to its folded compiled form is
/// refused and the file is left as it was before it.
pub fn apply(
    source: Option<&Document>,
    compiled: &Value,
    fixes: &[Fix],
    recompile: &dyn Fn(&Value) -> Option<Value>,
) -> FileOutcome {
    let mut outcome = FileOutcome::default();
    let Some(source) = source else {
        outcome.refused = fixes
            .iter()
            .map(|fix| (fix.clone(), Refusal::NoSpans))
            .collect();
        return outcome;
    };
    let mut src = source.root.node.clone();
    let mut folded = compiled.clone();
    for fix in fixes {
        let mut next_src = src.clone();
        let mut next_folded = folded.clone();
        let result =
            fold_value(&mut next_folded, fix).and_then(|()| fold_source(&mut next_src, fix));
        let result = result.and_then(|()| {
            if recompile(&next_src.to_value()).as_ref() == Some(&next_folded) {
                Ok(())
            } else {
                Err(Refusal::Verification)
            }
        });
        match result {
            Ok(()) => {
                src = next_src;
                folded = next_folded;
                outcome.applied.push(fix.clone());
            }
            Err(refusal) => outcome.refused.push((fix.clone(), refusal)),
        }
    }
    if outcome.applied.is_empty() {
        return outcome;
    }
    let printed = super::fmt::format_document(&Document {
        root: Spanned {
            node: src,
            span: Span { start: 0, end: 0 },
        },
        source: String::new(),
    });
    // The formatter's own guard: it re-parses what it printed and refuses
    // anything that differs from its input as a value.
    match super::fmt::format_str(&printed, "clippy --fix") {
        Ok(super::fmt::Outcome::Unchanged) => outcome.text = Some(printed),
        Ok(super::fmt::Outcome::Changed(text)) => outcome.text = Some(text),
        Err(e) => {
            let refused: Vec<(Fix, Refusal)> = std::mem::take(&mut outcome.applied)
                .into_iter()
                .map(|fix| (fix, Refusal::Format(e.to_string())))
                .collect();
            outcome.refused.extend(refused);
            return outcome;
        }
    }
    outcome.compiled = Some(folded);
    outcome
}

// ------------------------------------------------------------
// The fold, on the compiled document
// ------------------------------------------------------------

/// Fold the run in a compiled workflow. The checks that need the whole
/// workflow — the new id is free, the depth fits — are made here, on the
/// form the engine will parse.
pub fn fold_value(doc: &mut Value, fix: &Fix) -> Result<(), Refusal> {
    let Fix::FoldRun { members, group_id } = fix;
    // One id namespace across a loop's `setup` and the body, as the engine
    // checks it.
    let collides = STEP_LISTS
        .iter()
        .any(|at| step_ids(doc.pointer(at).unwrap_or(&Value::Null)).contains(group_id.as_str()));
    if collides {
        return Err(Refusal::IdCollision {
            id: group_id.clone(),
        });
    }
    let not_here = || Refusal::NotAuthoredHere {
        members: members.clone(),
    };
    // The list the run sits in, found without holding a borrow, then taken.
    let at = STEP_LISTS
        .iter()
        .find(|at| {
            doc.pointer(at)
                .is_some_and(|list| locate_value(&mut list.clone(), members).is_some())
        })
        .ok_or_else(not_here)?;
    let (list, start) = doc
        .pointer_mut(at)
        .and_then(|tasks| locate_value(tasks, members))
        .ok_or_else(not_here)?;
    let run: Vec<Value> = list.drain(start..start + members.len()).collect();
    let condition = run[0].get("condition").cloned().unwrap_or(Value::Null);
    let steps = run
        .into_iter()
        .map(|mut step| {
            if let Some(obj) = step.as_object_mut() {
                obj.remove("condition");
            }
            step
        })
        .collect();
    let mut group = Map::new();
    group.insert("id".to_string(), Value::String(group_id.clone()));
    group.insert("condition".to_string(), condition);
    group.insert("tasks".to_string(), Value::Array(steps));
    list.insert(start, Value::Object(group));
    if !crate::engine::walk_steps(doc.get("tasks").unwrap_or(&Value::Null), doc.get("loop"))
        .too_deep
        .is_empty()
    {
        return Err(Refusal::TooDeep);
    }
    Ok(())
}

/// The top-level step lists of a workflow, as JSON pointers: a loop's
/// `setup`, which runs first, and the body. A run is folded in whichever it
/// sits in.
const STEP_LISTS: [&str; 2] = ["/loop/setup", "/tasks"];

/// Every step id in a compiled step list, groups included.
fn step_ids(tasks: &Value) -> BTreeSet<&str> {
    let mut out = BTreeSet::new();
    let mut stack = vec![tasks];
    while let Some(list) = stack.pop() {
        for step in list.as_array().into_iter().flatten() {
            if let Some(id) = step.get("id").and_then(Value::as_str) {
                out.insert(id);
            }
            if let Some(inner) = step.get("tasks") {
                stack.push(inner);
            }
        }
    }
    out
}

/// The step list holding `members` consecutively, and where the run starts.
fn locate_value<'a>(
    tasks: &'a mut Value,
    members: &[String],
) -> Option<(&'a mut Vec<Value>, usize)> {
    let found = {
        let list = tasks.as_array()?;
        find_run(
            list.iter().map(|s| s.get("id").and_then(Value::as_str)),
            members,
        )
    };
    if let Some(start) = found {
        return Some((tasks.as_array_mut()?, start));
    }
    for step in tasks.as_array_mut()? {
        if let Some(inner) = step.get_mut("tasks")
            && let Some(hit) = locate_value(inner, members)
        {
            return Some(hit);
        }
    }
    None
}

/// Where `members` sit consecutively among `ids`, if they do.
fn find_run<'a>(ids: impl Iterator<Item = Option<&'a str>>, members: &[String]) -> Option<usize> {
    let ids: Vec<Option<&str>> = ids.collect();
    if members.is_empty() || ids.len() < members.len() {
        return None;
    }
    (0..=ids.len() - members.len()).find(|&start| {
        members
            .iter()
            .enumerate()
            .all(|(k, m)| ids[start + k] == Some(m.as_str()))
    })
}

// ------------------------------------------------------------
// The fold, on the source tree
// ------------------------------------------------------------

/// Fold the run in the source tree. A step list is searched only where the
/// author wrote steps — the workflow's `tasks`, its loop's `setup`, and each
/// group's — never
/// inside a `use` step, and a run holding a `use` or an `$each` is not one.
pub fn fold_source(root: &mut Node, fix: &Fix) -> Result<(), Refusal> {
    let Fix::FoldRun { members, group_id } = fix;
    let not_here = || Refusal::NotAuthoredHere {
        members: members.clone(),
    };
    let in_setup = object_get_mut(root, "loop")
        .and_then(|l| object_get_mut(l, "setup"))
        .and_then(|setup| locate_node(setup, members))
        .is_some();
    let tasks = if in_setup {
        object_get_mut(root, "loop").and_then(|l| object_get_mut(l, "setup"))
    } else {
        object_get_mut(root, "tasks")
    }
    .ok_or_else(not_here)?;
    let (list, start) = locate_node(tasks, members).ok_or_else(not_here)?;
    for step in &list[start..start + members.len()] {
        if step.node.get("condition").is_none() {
            let id = step
                .node
                .get("id")
                .and_then(|n| n.node.as_str())
                .unwrap_or("?")
                .to_string();
            return Err(Refusal::ConditionNotOnStep { step: id });
        }
    }
    let mut run: Vec<Spanned<Node>> = list.drain(start..start + members.len()).collect();
    // The condition as the first member wrote it — a `$from` stays a
    // reference.
    let condition = take_member(&mut run[0].node, "condition").unwrap_or(Spanned {
        node: Node::Null,
        span: placeholder(),
    });
    for step in &mut run[1..] {
        take_member(&mut step.node, "condition");
    }
    let group = Node::Object(vec![
        member("id", Node::String(group_id.clone())),
        Member {
            key: Spanned {
                node: "condition".to_string(),
                span: placeholder(),
            },
            value: condition,
        },
        member("tasks", Node::Array(run)),
    ]);
    list.insert(
        start,
        Spanned {
            node: group,
            span: placeholder(),
        },
    );
    Ok(())
}

fn locate_node<'a>(
    tasks: &'a mut Node,
    members: &[String],
) -> Option<(&'a mut Vec<Spanned<Node>>, usize)> {
    let found = {
        let Node::Array(list) = &*tasks else {
            return None;
        };
        let sugar = |step: &Node| {
            step.get("use").is_some() || step.get("$use").is_some() || step.get("$each").is_some()
        };
        find_run(
            list.iter().map(|s| {
                if sugar(&s.node) {
                    None
                } else {
                    s.node.get("id").and_then(|n| n.node.as_str())
                }
            }),
            members,
        )
    };
    let Node::Array(list) = tasks else {
        return None;
    };
    if let Some(start) = found {
        return Some((list, start));
    }
    for step in list {
        if step.node.get("use").is_some() {
            continue;
        }
        if let Some(inner) = object_get_mut(&mut step.node, "tasks")
            && let Some(hit) = locate_node(inner, members)
        {
            return Some(hit);
        }
    }
    None
}

fn object_get_mut<'a>(node: &'a mut Node, key: &str) -> Option<&'a mut Node> {
    let Node::Object(members) = node else {
        return None;
    };
    members
        .iter_mut()
        .find(|m| m.key.node == key)
        .map(|m| &mut m.value.node)
}

fn take_member(node: &mut Node, key: &str) -> Option<Spanned<Node>> {
    let Node::Object(members) = node else {
        return None;
    };
    let index = members.iter().position(|m| m.key.node == key)?;
    Some(members.remove(index).value)
}

/// New nodes have no source text; the printer never reads a span.
fn placeholder() -> Span {
    Span { start: 0, end: 0 }
}

fn member(key: &str, value: Node) -> Member {
    Member {
        key: Spanned {
            node: key.to_string(),
            span: placeholder(),
        },
        value: Spanned {
            node: value,
            span: placeholder(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fold(ids: &[&str], group: &str) -> Fix {
        Fix::FoldRun {
            members: ids.iter().map(|s| s.to_string()).collect(),
            group_id: group.to_string(),
        }
    }

    fn step(id: &str, extra: Value) -> Value {
        let mut step = json!({"id": id, "name": id, "condition": {"var": "data.go"},
            "function": {"name": "map", "input": {"mappings": []}}});
        for (k, v) in extra.as_object().expect("object") {
            step[k] = v.clone();
        }
        step
    }

    fn doc(text: &str) -> Document {
        Document::parse(text).expect("json")
    }

    /// The compiler stand-in for a source with no sugar: identity.
    fn same(v: &Value) -> Option<Value> {
        Some(v.clone())
    }

    #[test]
    fn a_run_folds_into_a_group_that_carries_the_condition_once() {
        let wf = json!({"tasks": [step("a", json!({})), step("b", json!({"terminal": true})),
                                  step("c", json!({"condition": {"var": "data.other"}}))]});
        let source = doc(&wf.to_string());
        let out = apply(Some(&source), &wf, &[fold(&["a", "b"], "when_a")], &same);
        assert!(out.refused.is_empty(), "{:?}", out.refused);
        let text = out.text.expect("written");
        let edited: Value = serde_json::from_str(&text).expect("json");
        let group = &edited["tasks"][0];
        assert_eq!(group["id"], "when_a");
        assert_eq!(group["condition"], json!({"var": "data.go"}));
        assert!(
            group.get("terminal").is_none(),
            "the group gets no terminal"
        );
        assert!(group["tasks"][0].get("condition").is_none());
        assert_eq!(
            group["tasks"][1]["terminal"], true,
            "a member keeps its own"
        );
        assert_eq!(edited["tasks"][1]["id"], "c");
        assert_eq!(out.compiled.expect("folded"), edited);
    }

    #[test]
    fn a_run_inside_a_group_is_found() {
        let wf = json!({"tasks": [{"id": "g", "condition": true,
            "tasks": [step("a", json!({})), step("b", json!({}))]}]});
        let out = apply(
            Some(&doc(&wf.to_string())),
            &wf,
            &[fold(&["a", "b"], "when_a")],
            &same,
        );
        let edited: Value = serde_json::from_str(&out.text.expect("written")).expect("json");
        assert_eq!(edited["tasks"][0]["tasks"][0]["id"], "when_a");
    }

    #[test]
    fn what_is_not_written_here_is_refused() {
        // Compiled: a fragment's steps, `pay.a` and `pay.b`; source: one `use`.
        let compiled = json!({"tasks": [step("pay.a", json!({})), step("pay.b", json!({}))]});
        let source = doc(r#"{"tasks": [{"id": "pay", "use": "charge"}]}"#);
        let out = apply(
            Some(&source),
            &compiled,
            &[fold(&["pay.a", "pay.b"], "when_pay.a")],
            &same,
        );
        assert!(out.text.is_none());
        assert!(matches!(out.refused[0].1, Refusal::NotAuthoredHere { .. }));
    }

    #[test]
    fn a_colliding_group_id_is_refused_not_renamed() {
        let wf = json!({"tasks": [step("a", json!({})), step("b", json!({})),
                                  step("when_a", json!({"condition": true}))]});
        let out = apply(
            Some(&doc(&wf.to_string())),
            &wf,
            &[fold(&["a", "b"], "when_a")],
            &same,
        );
        assert_eq!(
            out.refused[0].1,
            Refusal::IdCollision {
                id: "when_a".to_string()
            }
        );
    }

    /// A loop's `setup` shares the body's id namespace, so a group id taken
    /// there is taken.
    #[test]
    fn a_group_id_taken_in_loop_setup_is_a_collision() {
        let wf = json!({
            "loop": {"max": 2, "setup": [step("when_a", json!({}))]},
            "tasks": [step("a", json!({})), step("b", json!({}))]
        });
        let out = apply(
            Some(&doc(&wf.to_string())),
            &wf,
            &[fold(&["a", "b"], "when_a")],
            &same,
        );
        assert_eq!(
            out.refused[0].1,
            Refusal::IdCollision {
                id: "when_a".to_string()
            }
        );
    }

    #[test]
    fn a_condition_arriving_through_a_splice_is_refused() {
        let compiled = json!({"tasks": [step("a", json!({})), step("b", json!({}))]});
        let source = doc(r#"{"tasks": [
                {"$from": "constants.guarded", "id": "a", "name": "a", "function": {"name": "map", "input": {"mappings": []}}},
                {"id": "b", "name": "b", "condition": {"var": "data.go"}, "function": {"name": "map", "input": {"mappings": []}}}]}"#);
        let out = apply(
            Some(&source),
            &compiled,
            &[fold(&["a", "b"], "when_a")],
            &same,
        );
        assert_eq!(
            out.refused[0].1,
            Refusal::ConditionNotOnStep {
                step: "a".to_string()
            }
        );
    }

    #[test]
    fn an_edit_that_compiles_to_something_else_is_not_written() {
        let wf = json!({"tasks": [step("a", json!({})), step("b", json!({}))]});
        let out = apply(
            Some(&doc(&wf.to_string())),
            &wf,
            &[fold(&["a", "b"], "when_a")],
            &|_| Some(json!({"tasks": []})),
        );
        assert!(out.text.is_none());
        assert_eq!(out.refused[0].1, Refusal::Verification);
    }

    #[test]
    fn folding_past_the_depth_limit_is_refused() {
        let mut inner = json!([step("a", json!({})), step("b", json!({}))]);
        for depth in 0..crate::engine::MAX_STEP_DEPTH {
            inner = json!([{"id": format!("g{depth}"), "condition": true, "tasks": inner}]);
        }
        let wf = json!({"tasks": inner});
        assert!(
            crate::engine::walk_steps(&wf["tasks"], None)
                .too_deep
                .is_empty()
        );
        let out = apply(
            Some(&doc(&wf.to_string())),
            &wf,
            &[fold(&["a", "b"], "when_a")],
            &same,
        );
        assert_eq!(out.refused[0].1, Refusal::TooDeep);
    }
}
