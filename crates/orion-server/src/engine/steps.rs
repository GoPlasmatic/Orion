//! Walking a workflow's `tasks` array, which since dataflow-rs 3.6 holds
//! **steps** rather than tasks.
//!
//! An element carrying a `tasks` key is a *task group* — `{id, condition,
//! terminal, tasks}` — stating one condition for a contiguous run of tasks
//! instead of repeating it on each. Groups nest. dataflow-rs flattens the tree
//! at parse time and records each group's span on the task that opens it, so
//! the executor still walks a flat `&[Task]` and nothing downstream of parsing
//! has to know.
//!
//! Orion's own analysis does not go through that parse. Every check that asks
//! "what does this workflow reference" reads the **authored JSON**: the
//! connector walk, the `channel_call` walk, the task-shape validator, the
//! unresolvable-JSONLogic advisory, the offline call-log correlation. Each of
//! those iterated the array and looked for `function`, so on a grouped
//! workflow each of them would have skipped the group — and every task inside
//! it — without saying so. A connector referenced only from inside a guard
//! clause would have passed closure checking; a task inside one would have
//! gone unvalidated.
//!
//! So there is one flattener here and every walk uses it. The alternative —
//! each walk learning about groups on its own — is how they come to disagree
//! about what a workflow contains, and the disagreement would be silent in the
//! direction that matters: fewer things checked, still reported green.
//!
//! # Since dataflow-rs 3.7 this is an adapter, not an implementation
//!
//! The traversal, the group test and the depth cap now come from
//! [`walk_authored_steps`], which the engine ships precisely so a host stops
//! mirroring them. What is left here is the *shape* Orion's callers want —
//! leaves and groups pre-split into vectors — over the engine's lazy iterator.
//!
//! Mirroring cost Orion one real bug, and it is the reason this module no
//! longer holds a depth constant of its own. Orion counted depth from 1 while
//! the parser counts enclosing groups from 0, so the mirror refused the eighth
//! nesting level that dataflow-rs accepts: a legal workflow was rejected at
//! create with an "engine refuses to build this" message that was not true.
//! Reading [`MAX_GROUP_DEPTH`] means the limit tracks the parser by
//! construction.

use dataflow_rs::{MAX_GROUP_DEPTH, StepKind, walk_authored_steps};
use serde_json::Value;

pub use dataflow_rs::is_group;

/// dataflow-rs refuses a step tree nested deeper than this.
///
/// Re-exported rather than redefined so the number an author is told about is
/// the number the parser enforces.
pub const MAX_STEP_DEPTH: usize = MAX_GROUP_DEPTH;

/// What a `tasks` array holds, flattened.
#[derive(Debug, Default)]
pub struct Steps<'a> {
    /// Every leaf task in document order, with the JSON path that addresses
    /// it — `tasks[0]`, `tasks[1].tasks[0]`. The path is what makes a
    /// field error point at the thing the author wrote.
    pub tasks: Vec<(String, &'a Value)>,
    /// Every group, likewise. Groups are validated in their own right: they
    /// carry an id that shares the task namespace, and a condition.
    pub groups: Vec<(String, &'a Value)>,
    /// Groups nested at or past [`MAX_STEP_DEPTH`]. The walk reports them and
    /// stops descending, so a pathological document is bounded and the author
    /// still gets a node to point at.
    ///
    /// A group listed here is *not* also in [`Self::groups`]: it is the one
    /// finding that matters about it, and the whole workflow is refused over
    /// it either way.
    pub too_deep: Vec<String>,
}

/// Flatten a `tasks` array into its leaf tasks and its groups.
pub fn walk_steps(tasks: &Value) -> Steps<'_> {
    let mut out = Steps::default();
    for step in walk_authored_steps(tasks) {
        match step.kind {
            StepKind::Leaf => out.tasks.push((step.path, step.node)),
            StepKind::Group => out.groups.push((step.path, step.node)),
            StepKind::TooDeep => out.too_deep.push(step.path),
        }
    }
    out
}

/// Just the leaf tasks, for the walks that do not need paths.
///
/// The engine's walker formats a path for every node, so this no longer saves
/// the allocation it once did — it saves the caller a `filter`/`map` and keeps
/// "which tasks are in this workflow" a single spelling. The traversal is the
/// same one [`walk_steps`] uses, which is the property that matters: the
/// premise of this module is that every walk sees the same tasks.
pub fn leaf_tasks(tasks: &Value) -> Vec<&Value> {
    walk_authored_steps(tasks)
        .filter(|step| step.kind == StepKind::Leaf)
        .map(|step| step.node)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn task(id: &str) -> Value {
        json!({"id": id, "name": id, "function": {"name": "map", "input": {"mappings": []}}})
    }

    /// The flat case must be untouched: every workflow written before 3.6 is
    /// one, and this walk replaced the loops that handled them.
    #[test]
    fn a_flat_array_yields_its_tasks_in_order() {
        let tasks = json!([task("a"), task("b")]);
        let steps = walk_steps(&tasks);
        assert!(steps.groups.is_empty());
        assert_eq!(
            steps
                .tasks
                .iter()
                .map(|(p, _)| p.as_str())
                .collect::<Vec<_>>(),
            ["tasks[0]", "tasks[1]"]
        );
    }

    /// The case that motivated this: a task inside a guard clause is a task,
    /// and every check that walks the array must see it.
    #[test]
    fn a_group_yields_its_members_with_addressable_paths() {
        let tasks = json!([
            task("first"),
            {"id": "guard", "condition": true, "terminal": true, "tasks": [
                task("inner_a"),
                task("inner_b")
            ]},
            task("last")
        ]);
        let steps = walk_steps(&tasks);

        assert_eq!(
            steps
                .tasks
                .iter()
                .map(|(p, _)| p.as_str())
                .collect::<Vec<_>>(),
            [
                "tasks[0]",
                "tasks[1].tasks[0]",
                "tasks[1].tasks[1]",
                "tasks[2]"
            ],
            "document order, with the path that addresses each task"
        );
        assert_eq!(steps.groups.len(), 1);
        assert_eq!(steps.groups[0].0, "tasks[1]");
    }

    #[test]
    fn groups_nest() {
        let tasks = json!([
            {"id": "outer", "tasks": [
                {"id": "inner", "tasks": [task("deep")]}
            ]}
        ]);
        let steps = walk_steps(&tasks);
        assert_eq!(steps.tasks.len(), 1);
        assert_eq!(steps.tasks[0].0, "tasks[0].tasks[0].tasks[0]");
        assert_eq!(steps.groups.len(), 2, "both levels are groups");
        assert!(steps.too_deep.is_empty());
    }

    /// Deeper than the engine accepts: reported, and the walk stops rather
    /// than recursing without bound on a pathological document.
    #[test]
    fn nesting_past_the_engine_limit_is_reported() {
        let mut inner = json!([task("leaf")]);
        for i in 0..MAX_STEP_DEPTH + 2 {
            inner = json!([{"id": format!("g{i}"), "tasks": inner}]);
        }
        let steps = walk_steps(&inner);
        assert!(
            !steps.too_deep.is_empty(),
            "a tree past the limit must be reported, not silently truncated"
        );
    }

    /// The bug the mirror had: the engine accepts [`MAX_STEP_DEPTH`] nesting
    /// levels, and Orion must accept exactly those too. Counting depth from
    /// the wrong base made this workflow a 400 describing an engine refusal
    /// that would never have happened.
    #[test]
    fn the_deepest_nesting_the_parser_accepts_is_not_reported() {
        let mut inner = json!([task("leaf")]);
        for i in 0..MAX_STEP_DEPTH {
            inner = json!([{"id": format!("g{i}"), "tasks": inner}]);
        }
        assert!(
            walk_steps(&inner).too_deep.is_empty(),
            "{MAX_STEP_DEPTH} levels of nesting is what the parser accepts"
        );
        assert!(
            serde_json::from_value::<dataflow_rs::Workflow>(json!({
                "id": "w", "name": "w", "condition": true, "tasks": inner,
            }))
            .is_ok(),
            "and the parser agrees, which is the whole claim"
        );
    }

    /// A malformed task with neither `function` nor `tasks` is a broken task,
    /// not an empty group — reporting it as a group would send the author to
    /// the wrong place.
    #[test]
    fn a_task_missing_its_function_is_still_a_task() {
        let tasks = json!([{"id": "broken", "name": "broken"}]);
        let steps = walk_steps(&tasks);
        assert_eq!(steps.tasks.len(), 1);
        assert!(steps.groups.is_empty());
    }

    /// [`leaf_tasks`] and [`walk_steps`] must not drift apart. Upstream pins
    /// its walker against the engine's own flattening; this pins Orion's two
    /// spellings of it against each other.
    #[test]
    fn leaf_tasks_sees_exactly_what_walk_steps_sees() {
        let mut deep = json!([task("leaf")]);
        for i in 0..MAX_STEP_DEPTH + 2 {
            deep = json!([{"id": format!("g{i}"), "tasks": deep}]);
        }
        let cases = [
            json!([task("a"), task("b")]),
            json!([task("first"), {"id": "guard", "tasks": [task("x"), task("y")]}, task("last")]),
            json!([{"id": "outer", "tasks": [{"id": "inner", "tasks": [task("deep")]}]}]),
            json!([{"id": "broken", "name": "broken"}]),
            json!([{"id": "empty", "tasks": []}]),
            json!({"not": "an array"}),
            deep,
        ];
        for tasks in cases {
            let expected: Vec<&Value> = walk_steps(&tasks)
                .tasks
                .into_iter()
                .map(|(_, t)| t)
                .collect();
            assert_eq!(leaf_tasks(&tasks), expected, "disagreement on {tasks}");
        }
    }
}
