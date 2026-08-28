//! Rules about work the engine does for nothing. Each is a certain fact
//! about evaluation order; the suggestion is a suggestion.

use super::list_ids;
use crate::definitions::analysis::dataflow::overlaps;
use crate::definitions::analysis::keys::value_key;
use crate::definitions::analysis::{Analysis, Expr, StepFacts, StepKind};
use crate::definitions::clippy::{Diagnostic, Group, Level, Rule, Scope};

/// The functions whose only effect is the write to `data.<target>`.
const PURE_WRITERS: &[&str] = &["parse_json", "parse_xml", "publish_json", "publish_xml"];

/// Whether `step` might read the context in ways the walk cannot see: a
/// connector or composition call resolves its inputs at run time through
/// shapes this analysis does not model.
fn opaque(step: &StepFacts) -> bool {
    step.function
        .as_deref()
        .is_some_and(|f| crate::engine::CONNECTOR_FUNCTIONS.contains(&f) || f == "channel_call")
}

// ============================================================
// perf.parse_result_overwritten
// ============================================================

pub struct ParseResultOverwritten;

impl Rule for ParseResultOverwritten {
    fn id(&self) -> &'static str {
        "perf.parse_result_overwritten"
    }
    fn group(&self) -> Group {
        Group::Perf
    }
    fn level(&self) -> Level {
        Level::Warn
    }
    fn scope(&self) -> Scope {
        Scope::Workflow
    }
    fn summary(&self) -> &'static str {
        "a parse/publish target is overwritten by a later unconditional task before anything reads it"
    }
    fn explain(&self) -> &'static str {
        "`parse_json`, `parse_xml`, `publish_json` and `publish_xml` do one thing: write \
         `data.<target>`. When a later task that always runs writes that path (or a path \
         above it) and nothing in between reads it, the first task's work is discarded.\n\n\
         Proof: the registry — these four functions have no effect beyond the write; the \
         intervening steps' reads are literal paths that do not overlap the target; the \
         overwriting task has no condition and sits in no conditional group, so the \
         overwrite always happens.\n\n\
         Silent when: any step between them has a computed or element-scoped read, reads \
         the target or anything inside or above it, or is a connector or `channel_call` \
         task (their runtime reads are not modelled); the overwriting task is conditional; \
         the workflow has a `loop`."
    }

    fn check(&self, cx: &Analysis<'_>, out: &mut Vec<Diagnostic>) {
        for wf in &cx.workflows {
            if wf.has_loop {
                continue;
            }
            for (a, first) in wf.steps.iter().enumerate() {
                if first.kind != StepKind::Task
                    || !first
                        .function
                        .as_deref()
                        .is_some_and(|f| PURE_WRITERS.contains(&f))
                {
                    continue;
                }
                let Some(target) = first.writes.first() else {
                    continue;
                };
                for later in &wf.steps[a + 1..] {
                    let reads = later.reads();
                    if reads.uncertain() || reads.touches(target) || opaque(later) {
                        break;
                    }
                    if later.kind == StepKind::Group {
                        continue;
                    }
                    let overwrites = later
                        .writes
                        .iter()
                        .any(|w| w == target || target.starts_with(&format!("{w}.")));
                    if !overwrites {
                        continue;
                    }
                    if later.certain {
                        out.push(
                            Diagnostic::on_workflow(
                                self,
                                cx,
                                wf,
                                Some(&first.path),
                                format!(
                                    "`{}` writes `{target}`, but `{}` ({}) always overwrites it before \
                                     anything reads it; the first task does nothing observable",
                                    first.id, later.id, later.path
                                ),
                            )
                            .with_remedy("remove the first task, or give the two different targets"),
                        );
                    }
                    break;
                }
            }
        }
    }
}

// ============================================================
// perf.redundant_step_condition
// ============================================================

pub struct RedundantStepCondition;

/// A condition whose evaluation is a pure function of the context: it
/// compiles, is not already a constant, every read is named, nothing in it
/// is nondeterministic.
fn stable(expr: &Expr) -> bool {
    expr.compiles && expr.constant.is_none() && !expr.reads.uncertain() && !expr.nondeterministic()
}

impl Rule for RedundantStepCondition {
    fn id(&self) -> &'static str {
        "perf.redundant_step_condition"
    }
    fn group(&self) -> Group {
        Group::Perf
    }
    fn level(&self) -> Level {
        Level::Warn
    }
    fn scope(&self) -> Scope {
        Scope::Workflow
    }
    fn summary(&self) -> &'static str {
        "consecutive steps repeat one condition that none of them can change; a task group evaluates it once"
    }
    fn explain(&self) -> &'static str {
        "Two or more consecutive steps in one list carry the same `condition`. If no step \
         in the run writes any path the condition reads, it evaluates to the same value for \
         each of them, and a task group with that condition would evaluate it once and \
         guard the run.\n\n\
         Proof: canonical-JSON equality of the conditions; every read is a literal path; the \
         run's writes (`task_writes` and mapping paths, a group's members included) overlap \
         none of them; no `now`/`random`/`secret`.\n\n\
         Silent when: the condition has a computed or element-scoped read, a nondeterministic \
         operator, or is already constant; any step in the run writes a path the condition \
         reads or anything inside or above it. Suggestion only: no automatic rewrite."
    }

    fn check(&self, cx: &Analysis<'_>, out: &mut Vec<Diagnostic>) {
        for wf in &cx.workflows {
            let lists: std::collections::BTreeSet<&str> =
                wf.steps.iter().map(|s| s.list.as_str()).collect();
            for list in lists {
                let siblings: Vec<&StepFacts> =
                    wf.steps.iter().filter(|s| s.list == list).collect();
                let mut i = 0;
                while i < siblings.len() {
                    let Some(c) = &siblings[i].condition else {
                        i += 1;
                        continue;
                    };
                    let key = value_key(&c.value);
                    let mut j = i + 1;
                    while j < siblings.len()
                        && siblings[j]
                            .condition
                            .as_ref()
                            .is_some_and(|d| value_key(&d.value) == key)
                    {
                        j += 1;
                    }
                    let run = &siblings[i..j];
                    if run.len() >= 2 && stable(c) {
                        let unchanged = run
                            .iter()
                            .flat_map(|s| s.writes.iter())
                            .all(|w| !c.reads.paths.iter().any(|r| overlaps(r, w)));
                        if unchanged {
                            let ids: Vec<&str> = run.iter().map(|s| s.id.as_str()).collect();
                            out.push(
                                Diagnostic::on_workflow(
                                    self,
                                    cx,
                                    wf,
                                    Some(&format!("{}.condition", run[0].path)),
                                    format!(
                                        "{} consecutive steps ({}) repeat this condition, and none of \
                                         them writes what it reads; it is evaluated {} times for one answer",
                                        run.len(),
                                        list_ids(&ids),
                                        run.len()
                                    ),
                                )
                                .with_remedy(
                                    "wrap them in a task group carrying the condition once: \
                                     { \"id\": …, \"condition\": …, \"tasks\": [ … ] }",
                                ),
                            );
                        }
                    }
                    i = j.max(i + 1);
                }
            }
        }
    }
}

// ============================================================
// perf.group_condition_repeated
// ============================================================

pub struct GroupConditionRepeated;

impl Rule for GroupConditionRepeated {
    fn id(&self) -> &'static str {
        "perf.group_condition_repeated"
    }
    fn group(&self) -> Group {
        Group::Perf
    }
    fn level(&self) -> Level {
        Level::Warn
    }
    fn scope(&self) -> Scope {
        Scope::Workflow
    }
    fn summary(&self) -> &'static str {
        "a group member repeats the group's own condition, which was already true on entry"
    }
    fn explain(&self) -> &'static str {
        "A group's `condition` is evaluated once, on entry; its members run only if it \
         held. A member that carries the same condition re-evaluates something already \
         known to be true — unless an earlier member changed what it reads.\n\n\
         Proof: canonical-JSON equality with the group's condition; the group's reads are \
         literal; no earlier member of the group writes any of them; no nondeterministic \
         operator.\n\n\
         Silent when: the group condition has a computed or element-scoped read or a \
         nondeterministic operator; any earlier member writes a path it reads."
    }

    fn check(&self, cx: &Analysis<'_>, out: &mut Vec<Diagnostic>) {
        for wf in &cx.workflows {
            for (g, group) in wf.steps.iter().enumerate() {
                if group.kind != StepKind::Group {
                    continue;
                }
                let Some(gc) = &group.condition else {
                    continue;
                };
                if !stable(gc) {
                    continue;
                }
                let key = value_key(&gc.value);
                let mut written: Vec<&str> = Vec::new();
                for member in wf.steps.iter().filter(|s| s.parent == Some(g)) {
                    let repeats = member
                        .condition
                        .as_ref()
                        .is_some_and(|c| value_key(&c.value) == key);
                    let untouched = !written
                        .iter()
                        .any(|w| gc.reads.paths.iter().any(|r| overlaps(r, w)));
                    if repeats && untouched {
                        out.push(
                            Diagnostic::on_workflow(
                                self,
                                cx,
                                wf,
                                Some(&format!("{}.condition", member.path)),
                                format!(
                                    "`{}` repeats the condition of its group `{}`, which was already \
                                     true on entry and which no earlier member changes",
                                    member.id, group.id
                                ),
                            )
                            .with_remedy("remove the member's condition"),
                        );
                    }
                    written.extend(member.writes.iter().map(String::as_str));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pure_writers_are_exactly_the_engine_builtins_that_take_a_target() {
        for f in PURE_WRITERS {
            let writes = crate::definitions::analysis::dataflow::task_writes(&json!({
                "function": {"name": f, "input": {"source": "payload", "target": "x"}}
            }));
            assert_eq!(writes, ["data.x"], "{f}");
        }
    }
}
