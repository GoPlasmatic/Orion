//! `orion-server clippy`: what a definition set could do better, said only
//! when it is certain.
//!
//! `lint` is `cargo check` — what the admin API and the engine refuse.
//! `clippy` is everything advisory beyond that, and it has no configuration
//! and no suppression, which makes a false positive expensive: an author
//! cannot silence a wrong rule, so a rule that is wrong once is one people
//! learn to ignore, and then every rule is. So the bar: **a rule ships only
//! with a proof** — the engine's own evaluation (`Logic::is_constant`, the
//! evaluator on the exact selection-time context), an ingress fact
//! established in the code (`data` and `temp_data` are empty at selection;
//! `payload` is not in the context; `metadata.vars` is force-stamped),
//! engine semantics read from source (what `terminal` does, when
//! `channel_call` fails), the function registry, structural identity, or
//! the config passed with `-c` — and it declares the shapes on which that
//! proof does not hold, where it stays silent. Patterns over strings,
//! "usually a mistake" and near-matches are not admitted;
//! `docs/src/reference/clippy.md` keeps the list of what was turned down
//! and why. Silence is never wrong; a wrong warning is.
//!
//! ## Adding a rule
//!
//! Implement [`Rule`] in the group's file under `rules/`, add it to
//! [`registry`], give it a stable `group.snake_name` id, and add
//! `tests/fixtures/clippy/<id>/{fires,quiet}/` — a definition set the rule
//! fires on and one it must stay silent on, the second being the proof's
//! exclusions written down. `clippy_registry_test` fails on a rule missing
//! either; `clippy_docs_drift_test` fails while
//! `docs/src/reference/clippy.md` does not list it.

pub mod rules;

pub use crate::definitions::Diagnostic;
use crate::definitions::Severity;
use crate::definitions::analysis::{Analysis, WorkflowFacts};

/// Fixed per rule. There is no `Allow`: a rule not worth warning about is
/// not shipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// The workflow cannot behave as written. Fails the command.
    Deny,
    /// A certain fact the author would want to know; the command passes
    /// unless `--deny-warnings`.
    Warn,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Deny => "deny",
            Level::Warn => "warn",
        }
    }

    fn severity(self) -> Severity {
        match self {
            Level::Deny => Severity::Error,
            Level::Warn => Severity::Warning,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Correctness,
    Perf,
    Duplication,
    Style,
}

impl Group {
    pub fn as_str(self) -> &'static str {
        match self {
            Group::Correctness => "correctness",
            Group::Perf => "perf",
            Group::Duplication => "duplication",
            Group::Style => "style",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Runs over each workflow on its own.
    Workflow,
    /// Needs the whole set — other workflows, the channels, the fragments.
    Set,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Workflow => "workflow",
            Scope::Set => "set",
        }
    }
}

/// One rule.
pub trait Rule: Send + Sync {
    /// Stable, never renamed: `correctness.workflow_never_matches`.
    fn id(&self) -> &'static str;
    fn group(&self) -> Group;
    fn level(&self) -> Level;
    fn scope(&self) -> Scope;
    /// Whether the rule can only decide with the serving instance's config
    /// (`-c`). Skipped, with one note, when none was given.
    fn needs_config(&self) -> bool {
        false
    }
    /// One line, for `--list` and the docs table.
    fn summary(&self) -> &'static str;
    /// The rationale, the proof, the exclusions, and a before/after — what
    /// `--explain` prints.
    fn explain(&self) -> &'static str;
    fn check(&self, cx: &Analysis<'_>, out: &mut Vec<Diagnostic>);
}

/// The rules, in the order they run and are listed.
pub fn registry() -> &'static [&'static dyn Rule] {
    rules::ALL
}

pub fn find(id: &str) -> Option<&'static dyn Rule> {
    registry().iter().copied().find(|r| r.id() == id)
}

/// The rule-aware constructors.
///
/// An inherent impl on the shared [`Diagnostic`], written here rather than in
/// `diagnostic.rs`: they need a `Rule` and an `Analysis`, which are clippy's,
/// and the shared type must not know about either. Rust allows an inherent impl
/// anywhere in the defining crate, so this costs the call sites nothing.
impl Diagnostic {
    /// A diagnostic on a workflow, located if the source allows it.
    pub fn on_workflow(
        rule: &dyn Rule,
        cx: &Analysis<'_>,
        wf: &WorkflowFacts,
        path: Option<&str>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity: rule.level().severity(),
            check: rule.id(),
            entity: format!("workflow '{}'", wf.name),
            file: Some(wf.origin.clone()),
            path: path.map(str::to_string),
            line: path.and_then(|p| cx.locate(&wf.origin, p)),
            message: message.into(),
            remedy: None,
        }
    }

    /// A diagnostic on any entity by origin.
    pub fn at(
        rule: &dyn Rule,
        cx: &Analysis<'_>,
        entity: impl Into<String>,
        origin: &str,
        path: Option<&str>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity: rule.level().severity(),
            check: rule.id(),
            entity: entity.into(),
            file: Some(origin.to_string()),
            path: path.map(str::to_string),
            line: path.and_then(|p| cx.locate(origin, p)),
            message: message.into(),
            remedy: None,
        }
    }
}

/// What a run produced.
#[derive(Debug, Default)]
pub struct Report {
    pub diagnostics: Vec<Diagnostic>,
    /// Rules that needed `-c` and had none.
    pub skipped: Vec<&'static str>,
}

/// Run every rule over an analysed set.
pub fn run(cx: &Analysis<'_>) -> Report {
    let mut report = Report::default();
    for rule in registry() {
        if rule.needs_config() && cx.config.is_none() {
            report.skipped.push(rule.id());
            continue;
        }
        rule.check(cx, &mut report.diagnostics);
    }
    // Stable order, so two runs on two machines diff to nothing.
    report.diagnostics.sort_by(|a, b| {
        (&a.file, &a.path, a.check, &a.message).cmp(&(&b.file, &b.path, b.check, &b.message))
    });
    report
}

/// The `--list` table: one row per rule, the same columns as the docs.
pub fn list_table() -> String {
    let mut out =
        String::from("rule                                        level  scope     summary\n");
    for rule in registry() {
        out.push_str(&format!(
            "{:<43} {:<6} {:<9} {}\n",
            rule.id(),
            rule.level().as_str(),
            rule.scope().as_str(),
            rule.summary()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_named_by_group() {
        let mut seen = std::collections::BTreeSet::new();
        for rule in registry() {
            assert!(seen.insert(rule.id()), "duplicate rule id {}", rule.id());
            let (group, name) = rule.id().split_once('.').expect("group.name");
            assert_eq!(group, rule.group().as_str(), "{}", rule.id());
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{}: snake_case",
                rule.id()
            );
            assert!(
                !rule.summary().is_empty() && !rule.explain().is_empty(),
                "{}",
                rule.id()
            );
            assert!(
                rule.explain().contains("Proof") && rule.explain().contains("Silent"),
                "{}: explain() must state its proof and when it is silent",
                rule.id()
            );
        }
    }
}
