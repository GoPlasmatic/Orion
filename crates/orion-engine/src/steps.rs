//! Walking a workflow's steps: its `tasks` array, which holds **steps**
//! rather than tasks, and its loop's `setup` list, which holds steps too and
//! runs first.
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
//! # The group test and the depth cap are the parser's
//!
//! dataflow-rs 4 parses the authored tree but publishes no walker over raw
//! JSON, and Orion's callers need the raw nodes: a malformed task the parser
//! refuses is still a node a validator must point at. So the traversal lives
//! here, and the two facts it depends on match the parser: a step is a group
//! when it carries a `tasks` key (whatever that key holds), and groups nest at
//! most [`MAX_GROUP_DEPTH`] deep, counted from 0 at the top level. Orion once
//! counted depth from 1 and refused the deepest nesting the parser accepts, so
//! the number comes from the engine, not from here.

use dataflow_rs::def::MAX_GROUP_DEPTH;
use serde_json::Value;

/// dataflow-rs refuses a step tree nested deeper than this.
///
/// Re-exported rather than redefined so the number an author is told about is
/// the number the parser enforces.
pub const MAX_STEP_DEPTH: usize = MAX_GROUP_DEPTH;

/// Whether an authored step is a task group: it carries a `tasks` key. The
/// key's presence decides, not its type, so `{"tasks": "oops"}` is a
/// malformed group rather than a task.
#[inline]
pub fn is_group(step: &Value) -> bool {
    step.get("tasks").is_some()
}

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

/// Every step of a workflow, in the order the engine runs them: its loop's
/// `setup` list, at `loop.setup[0]`…, then `tasks`.
///
/// Both halves, always. A loop's setup steps are tasks the engine resolves,
/// validates and runs exactly as it does the body's, and a walk handed `tasks`
/// alone passed an unknown function, a missing connector or a duplicate id
/// placed there without a word (#351). Taking the loop in the signature is
/// what makes that omission something a caller has to write out rather than
/// fall into.
fn walk_into<'a>(tasks: &'a Value, loop_config: Option<&'a Value>, out: &mut Steps<'a>) {
    if let Some(setup) = loop_config.and_then(|l| l.get("setup")) {
        walk_list(setup, "loop.setup", 0, out);
    }
    walk_list(tasks, "tasks", 0, out);
}

/// One step list, pre-order: a group before its members. A list that is not
/// an array yields nothing; whether it must be one is the caller's rule.
fn walk_list<'a>(steps: &'a Value, prefix: &str, depth: usize, out: &mut Steps<'a>) {
    let Some(items) = steps.as_array() else {
        return;
    };
    for (idx, node) in items.iter().enumerate() {
        let path = format!("{prefix}[{idx}]");
        if !is_group(node) {
            out.tasks.push((path, node));
        } else if depth >= MAX_GROUP_DEPTH {
            out.too_deep.push(path);
        } else {
            let members = node.get("tasks").expect("is_group checked the key");
            let member_prefix = format!("{path}.tasks");
            out.groups.push((path, node));
            walk_list(members, &member_prefix, depth + 1, out);
        }
    }
}

/// Flatten a workflow's steps into its leaf tasks and its groups, setup
/// first.
pub fn walk_steps<'a>(tasks: &'a Value, loop_config: Option<&'a Value>) -> Steps<'a> {
    let mut out = Steps::default();
    walk_into(tasks, loop_config, &mut out);
    out
}

/// Just the leaf tasks, setup first, for the walks that do not need paths.
///
/// The same traversal [`walk_steps`] runs, which is the property that matters:
/// the premise of this module is that every walk sees the same tasks.
pub fn leaf_tasks<'a>(tasks: &'a Value, loop_config: Option<&'a Value>) -> Vec<&'a Value> {
    walk_steps(tasks, loop_config)
        .tasks
        .into_iter()
        .map(|(_, task)| task)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn task(id: &str) -> Value {
        json!({"id": id, "name": id, "function": {"name": "map", "input": {"mappings": []}}})
    }

    /// #351: a loop's `setup` steps are steps the engine runs, first, and
    /// every walk must see them, at the coordinate the author typed.
    #[test]
    fn a_loops_setup_steps_come_first_at_their_own_paths() {
        let tasks = json!([task("body")]);
        let loop_config = json!({"max": 2, "setup": [
            task("prime"),
            {"id": "g", "tasks": [task("inner")]}
        ]});
        let steps = walk_steps(&tasks, Some(&loop_config));
        assert_eq!(
            steps
                .tasks
                .iter()
                .map(|(p, _)| p.as_str())
                .collect::<Vec<_>>(),
            ["loop.setup[0]", "loop.setup[1].tasks[0]", "tasks[0]"]
        );
        assert_eq!(steps.groups.len(), 1);
        assert_eq!(leaf_tasks(&tasks, Some(&loop_config)).len(), 3);
        // A loop without `setup` adds nothing.
        assert_eq!(leaf_tasks(&tasks, Some(&json!({"max": 2}))).len(), 1);
    }

    /// The flat case must be untouched: every workflow written before 3.6 is
    /// one, and this walk replaced the loops that handled them.
    #[test]
    fn a_flat_array_yields_its_tasks_in_order() {
        let tasks = json!([task("a"), task("b")]);
        let steps = walk_steps(&tasks, None);
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
        let steps = walk_steps(&tasks, None);

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
        let steps = walk_steps(&tasks, None);
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
        let steps = walk_steps(&inner, None);
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
            walk_steps(&inner, None).too_deep.is_empty(),
            "{MAX_STEP_DEPTH} levels of nesting is what the parser accepts"
        );
        let parsed = dataflow_rs::def::parse(
            &json!({"id": "w", "name": "w", "condition": true, "tasks": inner}),
            crate::parse_options(),
        );
        assert!(
            parsed.def.is_some(),
            "and the parser agrees, which is the whole claim: {:?}",
            parsed.issues
        );
    }

    /// A malformed task with neither `function` nor `tasks` is a broken task,
    /// not an empty group — reporting it as a group would send the author to
    /// the wrong place.
    #[test]
    fn a_task_missing_its_function_is_still_a_task() {
        let tasks = json!([{"id": "broken", "name": "broken"}]);
        let steps = walk_steps(&tasks, None);
        assert_eq!(steps.tasks.len(), 1);
        assert!(steps.groups.is_empty());
    }

    /// One past the deepest nesting the parser accepts: the parser refuses it,
    /// and the walk reports it, so the two agree on where the limit is.
    #[test]
    fn the_parser_refuses_what_the_walk_reports_too_deep() {
        let mut inner = json!([task("leaf")]);
        for i in 0..=MAX_STEP_DEPTH {
            inner = json!([{"id": format!("g{i}"), "tasks": inner}]);
        }
        assert_eq!(walk_steps(&inner, None).too_deep.len(), 1);
        let parsed = dataflow_rs::def::parse(
            &json!({"id": "w", "name": "w", "condition": true, "tasks": inner}),
            crate::parse_options(),
        );
        assert!(parsed.def.is_none(), "the parser refuses it too");
    }

    /// [`leaf_tasks`] and [`walk_steps`] must not drift apart: this pins
    /// Orion's two spellings of the walk against each other.
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
            let expected: Vec<&Value> = walk_steps(&tasks, None)
                .tasks
                .into_iter()
                .map(|(_, t)| t)
                .collect();
            assert_eq!(
                leaf_tasks(&tasks, None),
                expected,
                "disagreement on {tasks}"
            );
        }
    }
}
