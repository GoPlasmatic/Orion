//! Rules whose finding is "this workflow cannot behave as written".
//!
//! Each one names its proof and its exclusions in `explain()`, which is the
//! text `--explain` prints and the contract the rule's `quiet` fixture
//! writes down.

use std::collections::BTreeSet;

use serde_json::Value;

use super::{list_ids, walk_values};
use crate::definitions::analysis::dataflow::{overlaps, reads};
use crate::definitions::analysis::{Analysis, StepKind, selection_context};
use crate::definitions::clippy::{Diagnostic, Group, Level, Rule, Scope};

// ============================================================
// correctness.workflow_never_matches
// ============================================================

pub struct WorkflowNeverMatches;

impl Rule for WorkflowNeverMatches {
    fn id(&self) -> &'static str {
        "correctness.workflow_never_matches"
    }
    fn group(&self) -> Group {
        Group::Correctness
    }
    fn level(&self) -> Level {
        Level::Deny
    }
    fn scope(&self) -> Scope {
        Scope::Workflow
    }
    fn summary(&self) -> &'static str {
        "the workflow-level condition is false for every request, so the workflow never runs"
    }
    fn explain(&self) -> &'static str {
        "A workflow's `condition` decides whether the workflow matches a request. It is \
         evaluated before any task runs, against a context in which `data` and `temp_data` \
         are empty objects — the request body is the *payload*, which only `parse_json` \
         brings into `data`. A condition that reads only `data`/`temp_data` therefore has \
         one possible result, and this rule asks the engine for it.\n\n\
         Proof: either the datalogic compiler folded the condition to a constant `false`/\
         `null` (`Logic::is_constant`), or every read is a literal path under `data` or \
         `temp_data` and the engine's own evaluator, run on exactly the selection-time \
         context, returns `false` or `null`.\n\n\
         Silent when: the condition reads `metadata` (populated at ingress, unknown \
         offline); it has a computed `val` or a read inside an element-scoped operator; \
         it uses `now`, `random` or `secret`; it reads the loop counter (written before \
         the first sweep); it evaluates to anything but exactly `false` or `null`.\n\n\
         Before:  \"condition\": { \"==\": [{ \"var\": \"data.type\" }, \"order\"] }\n\
         After:   \"condition\": true, and a task condition on `data.type` after `parse_json`."
    }

    fn check(&self, cx: &Analysis<'_>, out: &mut Vec<Diagnostic>) {
        for wf in &cx.workflows {
            let c = &wf.condition;
            if !c.compiles {
                continue;
            }
            let verdict = if let Some(constant) = &c.constant {
                if c.is_constant_falsy() {
                    Some(format!("`condition` is constant {constant}"))
                } else {
                    None
                }
            } else {
                if c.reads.uncertain() || c.nondeterministic() {
                    continue;
                }
                let only_empty_roots = c.reads.paths.iter().all(|p| {
                    (p == "data"
                        || p.starts_with("data.")
                        || p == "temp_data"
                        || p.starts_with("temp_data."))
                        && wf
                            .loop_counter
                            .as_deref()
                            .is_none_or(|counter| !overlaps(p, counter))
                });
                if !only_empty_roots {
                    continue;
                }
                match cx.evaluator.evaluate(&c.value, &selection_context()) {
                    Some(v @ (Value::Bool(false) | Value::Null)) => Some(format!(
                        "`condition` reads only `data`/`temp_data`, which are empty when the engine \
                         selects a workflow, and evaluates to {v} there"
                    )),
                    _ => None,
                }
            };
            if let Some(why) = verdict {
                out.push(
                    Diagnostic::on_workflow(
                        self,
                        cx,
                        wf,
                        Some("condition"),
                        format!("{why}; the workflow never matches a request"),
                    )
                    .with_remedy(
                        "a workflow condition can only branch on `metadata`; parse the payload in a \
                         task and put the test on a task or group condition",
                    ),
                );
            }
        }
    }
}

// ============================================================
// correctness.task_never_runs
// ============================================================

pub struct TaskNeverRuns;

impl Rule for TaskNeverRuns {
    fn id(&self) -> &'static str {
        "correctness.task_never_runs"
    }
    fn group(&self) -> Group {
        Group::Correctness
    }
    fn level(&self) -> Level {
        Level::Warn
    }
    fn scope(&self) -> Scope {
        Scope::Workflow
    }
    fn summary(&self) -> &'static str {
        "a step's condition folds to a constant false, so the step never runs"
    }
    fn explain(&self) -> &'static str {
        "A step whose `condition` the compiler folds to `false` or `null` is skipped on \
         every request.\n\n\
         Proof: `Logic::is_constant` — the datalogic compiler's own verdict that the \
         expression has no data dependency and what it folded to.\n\n\
         Silent when: the condition depends on any read at all, or folds to anything but \
         exactly `false` or `null`. A warning rather than an error: `\"condition\": false` is \
         a way to switch a step off deliberately."
    }

    fn check(&self, cx: &Analysis<'_>, out: &mut Vec<Diagnostic>) {
        for wf in &cx.workflows {
            for step in &wf.steps {
                if let Some(c) = &step.condition
                    && c.compiles
                    && c.is_constant_falsy()
                {
                    let constant = c.constant.as_ref().expect("constant");
                    out.push(
                        Diagnostic::on_workflow(
                            self,
                            cx,
                            wf,
                            Some(&format!("{}.condition", step.path)),
                            format!(
                                "`{}` has a condition that is constant {constant}, so it never runs",
                                step.id
                            ),
                        )
                        .with_remedy("remove the step, or the condition if it is meant to run"),
                    );
                }
            }
        }
    }
}

// ============================================================
// correctness.unreachable_step
// ============================================================

pub struct UnreachableStep;

impl Rule for UnreachableStep {
    fn id(&self) -> &'static str {
        "correctness.unreachable_step"
    }
    fn group(&self) -> Group {
        Group::Correctness
    }
    fn level(&self) -> Level {
        Level::Deny
    }
    fn scope(&self) -> Scope {
        Scope::Workflow
    }
    fn summary(&self) -> &'static str {
        "steps after an unconditional terminal step can never run"
    }
    fn explain(&self) -> &'static str {
        "A `terminal: true` step ends the workflow. When that step is certain to be reached \
         — no condition on it or on any enclosing group — everything after it in document \
         order is dead.\n\n\
         Proof: read from the dataflow-rs executor. A terminal *task* halts after it has \
         run, so it must be unconditional to be certain; a terminal *group* halts when its \
         span closes even if no member ran, so an unconditional group is certain whatever \
         its members do. A halt ends the whole workflow.\n\n\
         Silent when: the terminal step, or any group enclosing it, has a condition that \
         does not fold to `true`."
    }

    fn check(&self, cx: &Analysis<'_>, out: &mut Vec<Diagnostic>) {
        for wf in &cx.workflows {
            let Some(halt) = wf.steps.iter().position(|s| s.terminal && s.certain) else {
                continue;
            };
            let descends_from = |mut i: usize| {
                while let Some(p) = wf.steps[i].parent {
                    if p == halt {
                        return true;
                    }
                    i = p;
                }
                false
            };
            let unreachable: Vec<usize> = (halt + 1..wf.steps.len())
                .filter(|&i| !descends_from(i))
                // Report the outermost steps only; their members are implied.
                .filter(|&i| wf.steps[i].parent.is_none_or(|p| p <= halt))
                .collect();
            let Some(&first) = unreachable.first() else {
                continue;
            };
            let halting = &wf.steps[halt];
            let ids: Vec<&str> = unreachable
                .iter()
                .map(|&i| wf.steps[i].id.as_str())
                .collect();
            out.push(
                Diagnostic::on_workflow(
                    self,
                    cx,
                    wf,
                    Some(&wf.steps[first].path),
                    format!(
                        "`{}` ({}) is unconditional and terminal, so the workflow always ends there; \
                         {} can never run",
                        halting.id,
                        halting.path,
                        list_ids(&ids)
                    ),
                )
                .with_remedy("move the terminal step after them, give it a condition, or delete them"),
            );
        }
    }
}

// ============================================================
// correctness.unconditional_call_cycle
// ============================================================

pub struct UnconditionalCallCycle;

impl Rule for UnconditionalCallCycle {
    fn id(&self) -> &'static str {
        "correctness.unconditional_call_cycle"
    }
    fn group(&self) -> Group {
        Group::Correctness
    }
    fn level(&self) -> Level {
        Level::Deny
    }
    fn scope(&self) -> Scope {
        Scope::Set
    }
    fn summary(&self) -> &'static str {
        "channel_call edges that are all unconditional form a cycle, so every request into it fails at the depth limit"
    }
    fn explain(&self) -> &'static str {
        "`channel_call` runs another channel's workflow in-process. If workflow A always \
         calls a channel bound to workflow B, and B always calls back into A, every request \
         that reaches either recurses until `engine.max_channel_call_depth` and fails.\n\n\
         Proof: the static call graph — each `channel_call` with a literal `channel`, joined \
         to the set's channel → `workflow_id` binding — restricted to edges that are certain: \
         the calling task has no condition (or one folded to `true`), sits in no conditional \
         group, and its workflow's condition is absent or `true`. `channel_call.rs` fails the \
         call once the parent depth reaches the limit.\n\n\
         Silent when: any edge on the cycle is conditional (bounded recursion with a base \
         case is a legal pattern — the depth cap exists for it); a target is resolved by \
         `channel_logic`; a target channel or its workflow is not in the set."
    }

    fn check(&self, cx: &Analysis<'_>, out: &mut Vec<Diagnostic>) {
        // edges[a] = (b, calling step path, channel name)
        let mut edges: Vec<Vec<(usize, String, String)>> = vec![Vec::new(); cx.workflows.len()];
        for (a, wf) in cx.workflows.iter().enumerate() {
            if !wf.condition.is_constant_true() {
                continue;
            }
            for step in &wf.steps {
                if step.kind != StepKind::Task
                    || !step.certain
                    || step.function.as_deref() != Some("channel_call")
                {
                    continue;
                }
                let Some(target) = step
                    .node
                    .pointer("/function/input/channel")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                else {
                    continue;
                };
                let Some(id) = cx.channels.get(target) else {
                    continue;
                };
                if let Some(b) = cx
                    .workflows
                    .iter()
                    .position(|w| w.workflow_id.as_deref() == Some(id))
                {
                    edges[a].push((b, step.path.clone(), target.to_string()));
                }
            }
        }

        // A cycle is reported once, from its lowest-indexed workflow.
        for start in 0..cx.workflows.len() {
            if let Some(cycle) = find_cycle(&edges, start) {
                let wf = &cx.workflows[start];
                let chain: Vec<String> = cycle
                    .iter()
                    .map(|(from, channel)| {
                        format!("'{}' calls channel '{channel}'", cx.workflows[*from].name)
                    })
                    .collect();
                let (_, first_path, _) = &edges[start]
                    .iter()
                    .find(|(b, _, _)| *b == cycle[1 % cycle.len()].0 || cycle.len() == 1)
                    .cloned()
                    .unwrap_or_else(|| edges[start][0].clone());
                out.push(
                    Diagnostic::on_workflow(
                        self,
                        cx,
                        wf,
                        Some(first_path),
                        format!(
                            "unconditional channel_call cycle: {} → back to '{}'; every request \
                             recurses until max_channel_call_depth and fails",
                            chain.join(", which "),
                            wf.name
                        ),
                    )
                    .with_remedy("put a condition on one of the calls, or break the cycle"),
                );
            }
        }
    }
}

/// A cycle through `start` using only the given edges, as `(workflow,
/// channel called)` pairs in call order, reported only when `start` is the
/// lowest index on it.
fn find_cycle(
    edges: &[Vec<(usize, String, String)>],
    start: usize,
) -> Option<Vec<(usize, String)>> {
    fn dfs(
        edges: &[Vec<(usize, String, String)>],
        start: usize,
        at: usize,
        path: &mut Vec<(usize, String)>,
        seen: &mut BTreeSet<usize>,
    ) -> bool {
        for (next, _, channel) in &edges[at] {
            if *next < start {
                // A cycle through a lower index is that index's to report.
                continue;
            }
            path.push((at, channel.clone()));
            if *next == start {
                return true;
            }
            if seen.insert(*next) && dfs(edges, start, *next, path, seen) {
                return true;
            }
            path.pop();
        }
        false
    }
    let mut path = Vec::new();
    let mut seen = BTreeSet::from([start]);
    dfs(edges, start, start, &mut path, &mut seen).then_some(path)
}

// ============================================================
// correctness.payload_var
// ============================================================

pub struct PayloadVar;

impl Rule for PayloadVar {
    fn id(&self) -> &'static str {
        "correctness.payload_var"
    }
    fn group(&self) -> Group {
        Group::Correctness
    }
    fn level(&self) -> Level {
        Level::Deny
    }
    fn scope(&self) -> Scope {
        Scope::Workflow
    }
    fn summary(&self) -> &'static str {
        "a read of `payload` — which is not in the data context — is always null"
    }
    fn explain(&self) -> &'static str {
        "The raw request payload lives beside the data context, not in it. \
         `{\"var\": \"payload.x\"}` resolves to `null` everywhere an expression is evaluated; \
         only `parse_json`/`parse_xml` with `source: \"payload\"` bring it into `data`.\n\n\
         Proof: the ingress builds the message with the body as the payload \
         (`routes/data`, `channel_call`), and the context the engine evaluates against holds \
         `data`, `metadata` and `temp_data` only.\n\n\
         Silent when: the read sits inside an element-scoped argument of `map`, `filter`, \
         `reduce`, `all`, `some`, `none`, `group_by`, `distinct`, `sort`, `try`, `switch` or \
         `match`, where `payload` may be an element's own field. Only expressions the engine \
         evaluates are examined — conditions, mapping logic, filter and validation rules, \
         `log` fields, `channel_call` logic, and registry fields marked resolvable; a \
         connector payload that merely contains the text is data, not a read."
    }

    fn check(&self, cx: &Analysis<'_>, out: &mut Vec<Diagnostic>) {
        for wf in &cx.workflows {
            let mut exprs: Vec<(String, &crate::definitions::analysis::Expr)> =
                vec![("condition".to_string(), &wf.condition)];
            for step in &wf.steps {
                if let Some(c) = &step.condition {
                    exprs.push((format!("{}.condition", step.path), c));
                }
                for (p, e) in &step.expressions {
                    exprs.push((format!("{}.function.input.{p}", step.path), e));
                }
            }
            for (path, expr) in exprs {
                for read in &expr.reads.paths {
                    if read == "payload" || read.starts_with("payload.") {
                        out.push(
                            Diagnostic::on_workflow(
                                self,
                                cx,
                                wf,
                                Some(&path),
                                format!(
                                    "reads `{read}`, but `payload` is not in the data context — the \
                                     value is always null"
                                ),
                            )
                            .with_remedy(
                                "parse it first: a `parse_json` task with `source: \"payload\"` and \
                                 `target: \"<name>\"`, then read `data.<name>`",
                            ),
                        );
                    }
                }
            }
        }
    }
}

// ============================================================
// correctness.mapping_overwritten
// ============================================================

pub struct MappingOverwritten;

impl Rule for MappingOverwritten {
    fn id(&self) -> &'static str {
        "correctness.mapping_overwritten"
    }
    fn group(&self) -> Group {
        Group::Correctness
    }
    fn level(&self) -> Level {
        Level::Warn
    }
    fn scope(&self) -> Scope {
        Scope::Workflow
    }
    fn summary(&self) -> &'static str {
        "two mappings in one map write the same path with nothing reading it in between"
    }
    fn explain(&self) -> &'static str {
        "`map` applies its mappings in order. When two of them write the same `path` and \
         no mapping between them — nor the second one itself — reads that path, the first \
         write is dead: nothing can observe it.\n\n\
         Proof: the documented in-order semantics of `map`; the reads of every intervening \
         mapping and of the overwriting one are literal paths that do not overlap the \
         written path (prefix in either direction).\n\n\
         Silent when: any mapping between them, or the overwriting one, reads the path or \
         anything inside or above it, or has a computed or element-scoped read. \
         `data.x = 1` followed by `data.x = data.x + 1` is a pattern, not a mistake."
    }

    fn check(&self, cx: &Analysis<'_>, out: &mut Vec<Diagnostic>) {
        for wf in &cx.workflows {
            for step in &wf.steps {
                if step.function.as_deref() != Some("map") {
                    continue;
                }
                let Some(mappings) = step
                    .node
                    .pointer("/function/input/mappings")
                    .and_then(Value::as_array)
                else {
                    continue;
                };
                let paths: Vec<Option<&str>> = mappings
                    .iter()
                    .map(|m| m.get("path").and_then(Value::as_str))
                    .collect();
                let logic_reads: Vec<_> = mappings
                    .iter()
                    .map(|m| m.get("logic").map(reads).unwrap_or_default())
                    .collect();
                for i in 0..mappings.len() {
                    let Some(path) = paths[i] else {
                        continue;
                    };
                    let Some(j) = (i + 1..mappings.len()).find(|&j| paths[j] == Some(path)) else {
                        continue;
                    };
                    let observed = (i + 1..=j)
                        .any(|k| logic_reads[k].uncertain() || logic_reads[k].touches(path));
                    if observed {
                        continue;
                    }
                    out.push(
                        Diagnostic::on_workflow(
                            self,
                            cx,
                            wf,
                            Some(&format!("{}.function.input.mappings[{i}]", step.path)),
                            format!(
                                "writes `{path}`, which mappings[{j}] of the same task overwrites \
                                 before anything reads it"
                            ),
                        )
                        .with_remedy("remove the first mapping"),
                    );
                }
            }
        }
    }
}

// ============================================================
// correctness.metadata_var_undeclared  (needs -c)
// ============================================================

pub struct MetadataVarUndeclared;

impl Rule for MetadataVarUndeclared {
    fn id(&self) -> &'static str {
        "correctness.metadata_var_undeclared"
    }
    fn group(&self) -> Group {
        Group::Correctness
    }
    fn level(&self) -> Level {
        Level::Deny
    }
    fn scope(&self) -> Scope {
        Scope::Workflow
    }
    fn needs_config(&self) -> bool {
        true
    }
    fn summary(&self) -> &'static str {
        "a read of `metadata.vars.<name>` that the config given with -c does not declare"
    }
    fn explain(&self) -> &'static str {
        "`metadata.vars` is the `[vars]` section of the serving instance's config, stamped \
         onto every message at ingress. A caller cannot supply it, and a name the section \
         does not declare is `null` on every request.\n\n\
         Proof: `config/vars.rs` and the ingress routes — `vars` is force-stamped and \
         stripped from caller metadata; the config passed with `-c` is the declaration.\n\n\
         Silent when: no `-c` was given (the rule is skipped with a note); the read is \
         computed or element-scoped; the read is of `metadata.vars` as a whole."
    }

    fn check(&self, cx: &Analysis<'_>, out: &mut Vec<Diagnostic>) {
        let Some(config) = cx.config else {
            return;
        };
        let declared: BTreeSet<&str> = config.vars.0.keys().map(String::as_str).collect();
        for wf in &cx.workflows {
            let mut exprs: Vec<(String, &crate::definitions::analysis::Expr)> =
                vec![("condition".to_string(), &wf.condition)];
            for step in &wf.steps {
                if let Some(c) = &step.condition {
                    exprs.push((format!("{}.condition", step.path), c));
                }
                for (p, e) in &step.expressions {
                    exprs.push((format!("{}.function.input.{p}", step.path), e));
                }
            }
            for (path, expr) in exprs {
                for read in &expr.reads.paths {
                    let Some(rest) = read.strip_prefix("metadata.vars.") else {
                        continue;
                    };
                    let name = rest.split('.').next().unwrap_or(rest);
                    if name.is_empty() || declared.contains(name) {
                        continue;
                    }
                    let why = if declared.is_empty() {
                        "the config declares no [vars] at all, so `metadata.vars` is absent"
                            .to_string()
                    } else {
                        format!("the config's [vars] does not declare `{name}`")
                    };
                    out.push(
                        Diagnostic::on_workflow(
                            self,
                            cx,
                            wf,
                            Some(&path),
                            format!("reads `{read}`, but {why}; the value is always null"),
                        )
                        .with_remedy(format!(
                            "add `{name}` under [vars], or read the name it declares"
                        )),
                    );
                }
            }
        }
    }
}

// ============================================================
// correctness.secret_undeclared  (needs -c)
// ============================================================

pub struct SecretUndeclared;

impl Rule for SecretUndeclared {
    fn id(&self) -> &'static str {
        "correctness.secret_undeclared"
    }
    fn group(&self) -> Group {
        Group::Correctness
    }
    fn level(&self) -> Level {
        Level::Deny
    }
    fn scope(&self) -> Scope {
        Scope::Set
    }
    fn needs_config(&self) -> bool {
        true
    }
    fn summary(&self) -> &'static str {
        "a {\"secret\": name} that the config given with -c does not declare"
    }
    fn explain(&self) -> &'static str {
        "`{\"secret\": \"<name>\"}` reads the engine's secret store, which is the `[secrets]` \
         section of the serving instance's config. The engine refuses to build a workflow \
         that names a secret the store lacks, and Orion quarantines the channel at load.\n\n\
         Proof: the dataflow-rs secret store's build-time check, and the config passed with \
         `-c` as the declaration.\n\n\
         Silent when: no `-c` was given (skipped with a note)."
    }

    fn check(&self, cx: &Analysis<'_>, out: &mut Vec<Diagnostic>) {
        let Some(config) = cx.config else {
            return;
        };
        let declared: BTreeSet<&str> = config.secrets.0.keys().map(String::as_str).collect();
        for def in &cx.compiled.definitions {
            let entity = format!(
                "{} '{}'",
                def.entity.as_str(),
                def.doc
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(&def.origin)
            );
            let mut hits = Vec::new();
            walk_values(&def.doc, "", &mut |path, value| {
                if let Some(name) = crate::engine::functions::secret_ref::secret_name(value)
                    && !declared.contains(name)
                {
                    hits.push((path.to_string(), name.to_string()));
                }
            });
            for (path, name) in hits {
                out.push(
                    Diagnostic::at(
                        self,
                        cx,
                        entity.clone(),
                        &def.origin,
                        Some(&path),
                        format!(
                            "names secret `{name}`, which the config does not declare under [secrets]; \
                             the engine refuses to build this and the channel is quarantined"
                        ),
                    )
                    .with_remedy(format!("declare `{name}` under [secrets] in the serving config")),
                );
            }
        }
    }
}
