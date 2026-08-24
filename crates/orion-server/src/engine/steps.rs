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

use serde_json::Value;

/// dataflow-rs refuses a step tree nested deeper than this. Mirrored so an
/// author gets a field-pathed error from Orion rather than an engine failure
/// at load, which for Orion means a quarantined channel.
pub const MAX_STEP_DEPTH: usize = 8;

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
    /// Set when the tree nests deeper than [`MAX_STEP_DEPTH`]; the walk stops
    /// descending at that point rather than recursing without bound.
    pub too_deep: Vec<String>,
}

/// Whether a step element is a group rather than a task.
///
/// dataflow-rs's rule exactly: an element carrying a `tasks` key is a group.
/// Not "has no `function`" — a malformed task with neither should be reported
/// as a broken task, which is what the author meant it to be, rather than as
/// an empty group.
pub fn is_group(step: &Value) -> bool {
    step.get("tasks").is_some()
}

/// Flatten a `tasks` array into its leaf tasks and its groups.
pub fn walk_steps(tasks: &Value) -> Steps<'_> {
    let mut out = Steps::default();
    descend(tasks, "tasks", 1, &mut out);
    out
}

fn descend<'a>(tasks: &'a Value, path: &str, depth: usize, out: &mut Steps<'a>) {
    let Some(arr) = tasks.as_array() else {
        return;
    };
    for (i, step) in arr.iter().enumerate() {
        let step_path = format!("{path}[{i}]");
        if !is_group(step) {
            out.tasks.push((step_path, step));
            continue;
        }
        out.groups.push((step_path.clone(), step));
        if depth >= MAX_STEP_DEPTH {
            out.too_deep.push(step_path);
            continue;
        }
        if let Some(inner) = step.get("tasks") {
            descend(inner, &format!("{step_path}.tasks"), depth + 1, out);
        }
    }
}

/// Just the leaf tasks, for the walks that do not need paths.
///
/// Its own recursion rather than [`walk_steps`] with the paths dropped: the
/// connector-rename guard calls this once per active workflow, and building a
/// `tasks[1].tasks[0]` string per task only to discard it made that scan
/// allocate proportionally to the whole estate. Same traversal, same depth
/// cap — only the addresses are not computed.
pub fn leaf_tasks(tasks: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    push_leaves(tasks, 1, &mut out);
    out
}

fn push_leaves<'a>(tasks: &'a Value, depth: usize, out: &mut Vec<&'a Value>) {
    let Some(arr) = tasks.as_array() else {
        return;
    };
    for step in arr {
        if !is_group(step) {
            out.push(step);
            continue;
        }
        if depth < MAX_STEP_DEPTH
            && let Some(inner) = step.get("tasks")
        {
            push_leaves(inner, depth + 1, out);
        }
    }
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

    /// [`leaf_tasks`] skips the path formatting, which makes it a second
    /// traversal — so it is pinned to the first one here. The premise of this
    /// module is that every walk sees the same tasks; two that disagree would
    /// be the drift it exists to prevent.
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
