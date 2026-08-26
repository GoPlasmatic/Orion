//! The authoring layer: source form in, canonical form out.
//!
//! An author writes conveniences a definition set understands — `$from`
//! splices a shared value, `use` expands a task fragment (#285). The admin
//! API, the engine, traces and the UI understand none of them, and are not
//! meant to: a runtime that had to know about authoring sugar would have to
//! keep knowing about every future piece of it.
//!
//! So the two forms are separated by a compiler, and this module is its
//! pipeline. Each simplification is one [`Pass`]; [`compile`] runs them in a
//! declared order; `orion-server compile` writes the result out as something
//! the admin API accepts.
//!
//! ## Why a pass declares its residue
//!
//! A [`Pass`] must say not only how to rewrite a document but *where its own
//! syntax still appears* in one ([`Pass::residue`]). That one extra method is
//! what makes the layer safe to grow, because it is read three times:
//!
//! 1. the pipeline test asserts residue is empty after [`compile`], which is
//!    the definition of "canonical" and the property the whole runtime relies
//!    on;
//! 2. `compile` reports which passes actually fired, so an author can see what
//!    the command did to their document;
//! 3. the admin API turns leftover residue into the error that **names** it.
//!    Before this existed, an uncompiled `$from` reached the function-input
//!    validator as literal JSON and was refused for missing the fields the
//!    reference would have supplied — an error describing the symptom and
//!    hiding the cause (#295). Every pass added from here gets that error for
//!    free.
//!
//! ## Adding a pass
//!
//! Implement [`Pass`], add it to [`passes`] in the position its inputs
//! require, and give it a **stable id**: ids appear in findings and in the
//! documented table, and a pipeline grandfathers by id — the same contract
//! [`super::check`] gives its checks.
//!
//! Two invariants are asserted for every registered pass, so a new one cannot
//! quietly break the layer:
//!
//! - **idempotent** — `compile(compile(x)) == compile(x)`;
//! - **canonical output** — `residue()` is empty once `compile()` has run,
//!   including on the failure paths, because a pass that leaves its own syntax
//!   behind after refusing it would have the API report a reference the
//!   compiler already rejected.
//!
//! And one rule that is not machine-checked: **residue must mirror the
//! rewrite exactly**. A pass that detects more than it expands refuses
//! documents the compiler would have accepted; one that detects less lets
//! source form reach the runtime.

use serde_json::Value;

use super::finding::Finding;
use super::shared::SharedDefinitions;

/// What a pass may resolve against.
///
/// A struct rather than loose arguments so a later pass can be given more —
/// the connector registry, say — without changing [`Pass`] and every
/// implementation of it.
pub struct Cx<'a> {
    pub shared: &'a SharedDefinitions,
    /// How to name this document in a finding: a file path, or
    /// `workflows[3]` for an artifact entry.
    pub origin: &'a str,
}

/// One occurrence of a pass's source form in a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Residue {
    /// The id of the pass that owns this syntax.
    pub pass: &'static str,
    /// What it is, for a message: "a shared-value reference".
    pub noun: &'static str,
    /// The key that marks it — `$from`, `use`.
    pub key: &'static str,
    /// What it names: `constants.db`, or a fragment name.
    pub target: String,
    /// Authored coordinate, rooted at whatever [`residue`] was given:
    /// `tasks[1].function.input`.
    pub path: String,
}

impl Residue {
    /// The reference as it appears in the document: `{"$from": "constants.db"}`.
    ///
    /// Rendered rather than sliced out of the source so the message shows the
    /// reference alone, without whichever siblings happened to sit beside it.
    pub fn syntax(&self) -> String {
        format!(
            "{{{}: {}}}",
            Value::from(self.key),
            Value::from(&*self.target)
        )
    }

    /// The phrasing the single-file commands have always used.
    pub fn describe(&self) -> String {
        match self.key {
            "use" => format!("a reference to fragment '{}'", self.target),
            _ => format!("a reference to '{}'", self.target),
        }
    }
}

/// One authoring simplification: a rewrite from the source form an author
/// writes to the canonical form the admin API and the engine accept.
pub trait Pass: Send + Sync {
    /// Stable id, never renamed — findings, the compile report and the
    /// documented table all key on it.
    fn id(&self) -> &'static str;

    /// What this pass's source form is, for an error message.
    fn noun(&self) -> &'static str;

    /// Where this pass's syntax appears in `doc`, rooted at `root`.
    ///
    /// `root` is the coordinate `doc` sits at: `""` when it is a whole
    /// authored entity, `"tasks"` when the caller holds a task array on its
    /// own — which is the shape the admin API's validators have.
    fn residue(&self, doc: &Value, root: &str) -> Vec<Residue>;

    /// Rewrite `doc` in place, reporting what would not resolve.
    fn apply(&self, doc: &mut Value, cx: &Cx<'_>, findings: &mut Vec<Finding>);
}

/// The pipeline, in order.
///
/// Fragments before values: a fragment's tasks may themselves carry `$from`,
/// and splicing afterwards means a fragment is written exactly the way a
/// workflow is.
pub fn passes() -> &'static [&'static dyn Pass] {
    static FRAGMENTS: Fragments = Fragments;
    static VALUES: Values = Values;
    static PASSES: &[&dyn Pass] = &[&FRAGMENTS, &VALUES];
    PASSES
}

/// Run every pass over one authored document, returning the ids of those that
/// had anything to do.
pub fn compile(doc: &mut Value, cx: &Cx<'_>, findings: &mut Vec<Finding>) -> Vec<&'static str> {
    let mut applied = Vec::new();
    for pass in passes() {
        // Asked before the rewrite, because after it there is nothing left to
        // see — that is the point of the rewrite.
        let fired = !pass.residue(doc, "").is_empty();
        pass.apply(doc, cx, findings);
        if fired {
            applied.push(pass.id());
        }
    }
    applied
}

/// Every pass's residue in `doc`, in pipeline order.
///
/// Empty means the document is canonical: nothing in it is waiting to be
/// compiled, so it is safe to store, hash and run.
pub fn residue(doc: &Value, root: &str) -> Vec<Residue> {
    passes()
        .iter()
        .flat_map(|pass| pass.residue(doc, root))
        .collect()
}

// ============================================================
// shared.fragments — `{"id": "_x", "use": "f", "with": {..}}`
// ============================================================

struct Fragments;

impl Fragments {
    /// Named once and read by both the trait methods and the residue
    /// constructor below, so the id a finding carries and the id the pipeline
    /// reports cannot drift apart.
    const ID: &'static str = "shared.fragments";
    const NOUN: &'static str = "a task-fragment reference";
}

impl Pass for Fragments {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn noun(&self) -> &'static str {
        Self::NOUN
    }

    /// Walks the authored step tree, and only that.
    ///
    /// `use` names a fragment where the expander reads it — an element of a
    /// `tasks` array — and nowhere else, so a payload field that happens to be
    /// called `use` is left alone. The descent into a group mirrors
    /// `expand_tasks`: `is_group` is the engine's own test, so a step this
    /// walk declines to enter is exactly one the expander calls a task.
    fn residue(&self, doc: &Value, root: &str) -> Vec<Residue> {
        let mut out = Vec::new();
        // `root == "tasks"` says the caller already stepped through the key
        // and is holding the array itself.
        if root == "tasks" {
            steps(doc, root, &mut out);
        } else if let Some(tasks) = doc.get("tasks") {
            let at = if root.is_empty() {
                "tasks".to_string()
            } else {
                format!("{root}.tasks")
            };
            steps(tasks, &at, &mut out);
        }
        out
    }

    fn apply(&self, doc: &mut Value, cx: &Cx<'_>, findings: &mut Vec<Finding>) {
        if let Some(tasks) = doc.get_mut("tasks").and_then(Value::as_array_mut) {
            let expanded = cx.shared.expand_tasks(tasks, cx.origin, findings);
            *tasks = expanded;
        }
    }
}

fn steps(tasks: &Value, path: &str, out: &mut Vec<Residue>) {
    let Some(items) = tasks.as_array() else {
        return;
    };
    for (i, item) in items.iter().enumerate() {
        let at = format!("{path}[{i}]");
        if let Some(name) = item.get("use").and_then(Value::as_str) {
            out.push(Residue {
                pass: Fragments::ID,
                noun: Fragments::NOUN,
                key: "use",
                target: name.to_string(),
                path: at,
            });
            // The expander replaces this element wholesale; `with` is
            // arguments, not a document with steps of its own.
            continue;
        }
        if crate::engine::is_group(item)
            && let Some(inner) = item.get("tasks")
        {
            steps(inner, &format!("{at}.tasks"), out);
        }
    }
}

// ============================================================
// shared.values — `{"$from": "ns.key", ..siblings}`
// ============================================================

struct Values;

impl Values {
    const ID: &'static str = "shared.values";
    const NOUN: &'static str = "a shared-value reference";
}

impl Pass for Values {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn noun(&self) -> &'static str {
        Self::NOUN
    }

    /// Every depth, matching the splicer: a `$from` is as legal inside a
    /// `map` mapping's `logic` as it is in a task input.
    ///
    /// A non-string `$from` is skipped, because the splicer skips it too — the
    /// two have to refuse and rewrite the same set of documents.
    fn residue(&self, doc: &Value, root: &str) -> Vec<Residue> {
        let mut out = Vec::new();
        spliceable(doc, root, &mut out);
        out
    }

    fn apply(&self, doc: &mut Value, cx: &Cx<'_>, findings: &mut Vec<Finding>) {
        cx.shared.splice(doc, cx.origin, findings);
    }
}

fn spliceable(value: &Value, path: &str, out: &mut Vec<Residue>) {
    match value {
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                spliceable(item, &format!("{path}[{i}]"), out);
            }
        }
        Value::Object(map) => {
            if let Some(target) = map.get("$from").and_then(Value::as_str) {
                out.push(Residue {
                    pass: Values::ID,
                    noun: Values::NOUN,
                    key: "$from",
                    target: target.to_string(),
                    path: path.to_string(),
                });
            }
            for (key, v) in map {
                let at = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                spliceable(v, &at, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn catalog() -> SharedDefinitions {
        let mut shared = SharedDefinitions::default();
        let mut findings = Vec::new();
        shared.merge(
            &json!({
                "constants": { "db": { "connector": "mongo", "database": "app" } },
                "errors": { "NOT_FOUND": { "status": 404, "body": "nope" } },
                "fragments": { "guard": {
                    "params": { "msg": { "default": "denied" } },
                    "tasks": [ { "id": "deny", "name": "Deny", "function": { "name": "map",
                        "input": { "mappings": [
                            { "path": "data.msg", "logic": { "$param": "msg" } } ] } } } ] } }
            }),
            "catalog.json",
            &mut findings,
        );
        assert!(findings.is_empty(), "{findings:?}");
        shared
    }

    fn sugared() -> Value {
        json!({
            "workflow_id": "w", "name": "w",
            "tasks": [
                { "id": "_g", "use": "guard", "with": { "msg": "no" } },
                { "id": "read", "name": "Read", "function": { "name": "mongo_read",
                    "input": { "$from": "constants.db", "collection": "users" } } },
                { "id": "group", "condition": true, "tasks": [
                    { "id": "_g2", "use": "guard" },
                    { "id": "err", "name": "Err", "function": { "name": "map",
                        "input": { "mappings": [
                            { "path": "data.out", "logic": { "$from": "errors.NOT_FOUND" } } ] } } } ] }
            ]
        })
    }

    #[test]
    fn residue_names_every_reference_with_its_coordinate() {
        let found = residue(&sugared(), "");
        let seen: Vec<(&str, String)> = found
            .iter()
            .map(|r| (r.key, r.path.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            seen,
            vec![
                ("use", "tasks[0]".to_string()),
                ("use", "tasks[2].tasks[0]".to_string()),
                ("$from", "tasks[1].function.input".to_string()),
                (
                    "$from",
                    "tasks[2].tasks[1].function.input.mappings[0].logic".to_string()
                ),
            ],
            "fragments are reported before values, each at the coordinate the author typed"
        );
    }

    #[test]
    fn a_task_array_held_on_its_own_roots_at_tasks() {
        let doc = sugared();
        let found = residue(&doc["tasks"], "tasks");
        assert_eq!(
            found.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            vec![
                "tasks[0]",
                "tasks[2].tasks[0]",
                "tasks[1].function.input",
                "tasks[2].tasks[1].function.input.mappings[0].logic",
            ],
            "the admin API holds `tasks` alone and must get the same coordinates"
        );
    }

    #[test]
    fn use_outside_a_step_is_an_ordinary_field() {
        // A payload field named `use`, and a `tasks` array that is a function
        // input rather than a step list. The expander touches neither, so
        // neither may be reported — a pass that detects more than it expands
        // would refuse documents the compiler accepts.
        let doc = json!({
            "name": "w",
            "tasks": [ { "id": "t", "name": "T", "function": { "name": "http_call",
                "input": { "body": { "use": "cache", "tasks": [ { "use": "nested" } ] } } } } ]
        });
        assert_eq!(residue(&doc, ""), vec![]);
    }

    #[test]
    fn a_non_string_from_is_not_a_reference() {
        // The splicer requires a string, so this walk must too.
        let doc = json!({ "tasks": [ { "function": { "input": { "$from": 5 } } } ] });
        assert_eq!(residue(&doc, ""), vec![]);
    }

    #[test]
    fn compiling_reports_the_passes_that_fired_and_leaves_nothing_behind() {
        let shared = catalog();
        let mut doc = sugared();
        let mut findings = Vec::new();
        let applied = compile(
            &mut doc,
            &Cx {
                shared: &shared,
                origin: "wf.json",
            },
            &mut findings,
        );
        assert!(findings.is_empty(), "{findings:?}");
        assert_eq!(applied, vec!["shared.fragments", "shared.values"]);
        assert_eq!(
            residue(&doc, ""),
            vec![],
            "a compiled document is canonical — this is what the runtime relies on"
        );
        // The splice merged, and the fragment's ids were namespaced.
        assert_eq!(doc["tasks"][1]["function"]["input"]["connector"], "mongo");
        assert_eq!(doc["tasks"][1]["function"]["input"]["collection"], "users");
        assert_eq!(doc["tasks"][0]["id"], "_g.deny");
    }

    #[test]
    fn compiling_is_idempotent() {
        let shared = catalog();
        let cx = Cx {
            shared: &shared,
            origin: "wf.json",
        };
        let mut once = sugared();
        let mut findings = Vec::new();
        compile(&mut once, &cx, &mut findings);
        let mut twice = once.clone();
        let applied = compile(&mut twice, &cx, &mut findings);
        assert_eq!(once, twice);
        assert!(
            applied.is_empty(),
            "a canonical document must fire no pass at all"
        );
    }

    #[test]
    fn nothing_survives_a_reference_that_does_not_resolve() {
        // The failure paths matter as much as the happy one: a pass that
        // refused a reference and left its syntax in place would have the
        // admin API report something the compiler had already rejected.
        let shared = SharedDefinitions::default();
        let mut doc = sugared();
        let mut findings = Vec::new();
        compile(
            &mut doc,
            &Cx {
                shared: &shared,
                origin: "wf.json",
            },
            &mut findings,
        );
        assert!(findings.iter().any(|f| f.is_error()));
        assert_eq!(residue(&doc, ""), vec![]);
    }

    #[test]
    fn every_pass_has_a_distinct_stable_id() {
        let ids: Vec<&str> = passes().iter().map(|p| p.id()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "pass ids must be unique: {ids:?}");
        assert!(passes().iter().all(|p| !p.noun().is_empty()));
    }
}
