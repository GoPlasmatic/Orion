//! One shape for "something is wrong with a definition, here is where".
//!
//! Three types used to say this. `preflight::Finding` reported upgrade breaks
//! in the stored estate; `definitions::Finding` reported set-check results and
//! its own doc admitted it mirrored preflight's; `clippy::Diagnostic` was
//! `Finding` plus a location, with a `from_finding` upcast to carry lint
//! results through a clippy run. They agreed on every field that mattered —
//! a stable machine-readable check id, the entity named as the author named it,
//! what is wrong, and what to do — and differed only in what each had not
//! needed yet.
//!
//! So there is one type, and the location is optional rather than a second
//! type. A caller with no file to name leaves it `None` and renders exactly the
//! line it rendered before.
//!
//! **`FieldError` is deliberately not folded in.** It is the wire contract:
//! a closed `code` vocabulary (`orion_api::error::field_codes::ALL`) pinned by
//! `field_codes_drift_test`, serialized inside every error envelope a client
//! reads. Merging it would make an internal report shape a published API. What
//! the merge *is* for is the boundary between them — see
//! [`Diagnostic::from_field_error`], which used to be `e.to_string()` over a
//! whole `Vec<FieldError>` and threw away every path and code in it.

use serde_json::Value;

/// Whether a diagnostic fails the command or only informs it.
///
/// The split exists because cross-reference findings are a different class
/// from schema errors: a workflow whose `channel_call` target resolves
/// dynamically cannot be verified statically, and reporting that as a failure
/// would make the gate unusable for every set that uses `channel_logic`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The set is wrong. Non-zero exit.
    Error,
    /// Worth saying; the set may still be correct. Exits zero unless the
    /// caller denies warnings.
    Warning,
    /// Neither a defect nor a suspicion — an inventory line the report is
    /// expected to carry. Printed, never counted, and `--deny-warnings` does
    /// not gate on it.
    ///
    /// The level exists because `check` reports two different things. "This
    /// set requires `$VAR` in its environment" is a fact about a correct set,
    /// not a doubt about it; counting it as a warning made `--deny-warnings`
    /// fail on every set that authors a secret the documented way, which
    /// left the flag with no usable setting.
    Note,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        }
    }
}

/// One thing a check, a rule or a scan found.
///
/// `check` is the load-bearing field for tooling. It is what lets a pipeline
/// grandfather one rule rather than reaching for `--deny-warnings` and
/// silencing the lot, and it is the hook a future reference kind (`$from`, a
/// fragment `use`) arrives on without new plumbing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    /// Stable dotted id: a clippy rule id (`perf.redundant_run`), a set-check
    /// id (`closure.connector`, `duplicate.route_pattern`), or the
    /// `upgrading.md` checklist row a preflight break belongs to.
    pub check: &'static str,
    /// What is affected, named as the author named it —
    /// `workflow 'auth-login'`, `channels[2]`.
    pub entity: String,
    /// The file the entity was read from, when there is one. `None` for a scan
    /// over stored rows, which have no file to point at.
    pub file: Option<String>,
    /// The coordinate inside that document — `tasks[2].condition`.
    pub path: Option<String>,
    /// 1-based line and column in `file`, when the source has the same
    /// coordinates as the compiled form.
    pub line: Option<(usize, usize)>,
    pub message: String,
    /// What to change. `None` when the message already says it.
    pub remedy: Option<String>,
}

impl Diagnostic {
    fn new(severity: Severity, check: &'static str, entity: String, message: String) -> Self {
        Self {
            severity,
            check,
            entity,
            file: None,
            path: None,
            line: None,
            message,
            remedy: None,
        }
    }

    pub fn error(
        check: &'static str,
        entity: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::new(Severity::Error, check, entity.into(), message.into())
    }

    pub fn warning(
        check: &'static str,
        entity: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::new(Severity::Warning, check, entity.into(), message.into())
    }

    /// An inventory line: printed, never counted. See [`Severity::Note`].
    pub fn note(
        check: &'static str,
        entity: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::new(Severity::Note, check, entity.into(), message.into())
    }

    pub fn with_remedy(mut self, remedy: impl Into<String>) -> Self {
        self.remedy = Some(remedy.into());
        self
    }

    /// Attach where this was found. `line` is `None` when the source could not
    /// be located — a computed coordinate, or a document the reader has no
    /// spans for.
    pub fn with_location(
        mut self,
        file: impl Into<String>,
        path: Option<&str>,
        line: Option<(usize, usize)>,
    ) -> Self {
        self.file = Some(file.into());
        self.path = path.map(str::to_string);
        self.line = line;
        self
    }

    /// One structured validation error, kept structured.
    ///
    /// The admin validators answer in `Vec<FieldError>` — a field path and a
    /// code per problem. The set check used to collapse the whole vector with
    /// `e.to_string()`, so a set gate could say *that* a workflow failed schema
    /// validation but never which field, and reported one finding where there
    /// were five. One diagnostic per field error, carrying its `path`, is the
    /// fix.
    ///
    /// The `code` is deliberately not carried. It belongs to the wire
    /// vocabulary a client reads, and `check` is already the id a pipeline
    /// grandfathers on — a per-code check id would have to be built at runtime,
    /// and `check` is `&'static str` precisely so it cannot be.
    pub fn from_field_error(
        check: &'static str,
        entity: &str,
        err: &orion_api::FieldError,
    ) -> Self {
        let mut out = Self::error(check, entity.to_string(), err.message.clone());
        if !err.path.is_empty() {
            out.path = Some(err.path.clone());
        }
        out
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }

    /// Whether `--deny-warnings` should gate on this diagnostic.
    pub fn is_warning(&self) -> bool {
        self.severity == Severity::Warning
    }

    /// The text line: `file:line:col: warning: [check] workflow 'x' at
    /// tasks[1]: message`, with each half of the location prefix omitted when
    /// it is not known. With no location at all this is exactly the line the
    /// set checks have always printed.
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        match (&self.file, self.line) {
            (Some(file), Some((line, col))) => out.push_str(&format!("{file}:{line}:{col}: ")),
            (Some(file), None) => out.push_str(&format!("{file}: ")),
            (None, _) => {}
        }
        out.push_str(&format!(
            "{}: [{}] {}",
            self.severity.as_str(),
            self.check,
            self.entity
        ));
        if let Some(path) = &self.path {
            out.push_str(&format!(" at {path}"));
        }
        out.push_str(&format!(": {}", self.message));
        if let Some(remedy) = &self.remedy {
            out.push_str(&format!("\n        fix: {remedy}"));
        }
        out
    }

    /// The `--format json` line.
    ///
    /// The key is `rule`, not `check`, because this shape is what `clippy
    /// --format json` has always emitted and a consumer is parsing it. The
    /// field was renamed in Rust when the three types merged; the wire name
    /// stays.
    pub fn render_json(&self) -> Value {
        serde_json::json!({
            "level": self.severity.as_str(),
            "rule": self.check,
            "entity": self.entity,
            "file": self.file,
            "path": self.path,
            "line": self.line.map(|(l, _)| l),
            "column": self.line.map(|(_, c)| c),
            "message": self.message,
            "remedy": self.remedy,
        })
    }

    /// The three-line form `preflight` reports in.
    ///
    /// Severity is not printed: `run_preflight` groups its findings by it and
    /// heads each group, so a per-line level would be the same word for every
    /// line under a heading that already said it. The remedy is printed,
    /// because the whole point of the scan is that each break has one.
    pub fn render_preflight(&self) -> String {
        let mut out = format!("[{}] {}\n      {}", self.check, self.entity, self.message);
        if let Some(remedy) = &self.remedy {
            out.push_str(&format!("\n      fix: {remedy}"));
        }
        out
    }
}

impl std::fmt::Display for Diagnostic {
    /// [`Self::render_text`] — the `check` id is printed because it is the
    /// thing a pipeline greps for.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.render_text())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The un-located line is what `lint` and `check` have always printed, and
    /// merging with clippy's located form must not have changed it.
    #[test]
    fn an_unlocated_diagnostic_renders_the_line_lint_always_printed() {
        let d = Diagnostic::error("closure.connector", "workflow 'a'", "no such connector");
        assert_eq!(
            d.to_string(),
            "error: [closure.connector] workflow 'a': no such connector"
        );
    }

    #[test]
    fn a_remedy_is_indented_under_its_line() {
        let d = Diagnostic::warning("x.y", "workflow 'a'", "odd").with_remedy("do the other thing");
        assert_eq!(
            d.to_string(),
            "warning: [x.y] workflow 'a': odd\n        fix: do the other thing"
        );
    }

    #[test]
    fn a_located_diagnostic_carries_file_line_and_path() {
        let d = Diagnostic::error("perf.x", "workflow 'a'", "slow").with_location(
            "wf.json",
            Some("tasks[1]"),
            Some((7, 3)),
        );
        assert_eq!(
            d.to_string(),
            "wf.json:7:3: error: [perf.x] workflow 'a' at tasks[1]: slow"
        );
    }

    /// A file with no resolvable line still names the file — better than
    /// dropping the location entirely.
    #[test]
    fn a_file_without_a_line_still_prefixes_the_file() {
        let d = Diagnostic::error("perf.x", "workflow 'a'", "slow")
            .with_location("wf.json", None, None);
        assert_eq!(d.to_string(), "wf.json: error: [perf.x] workflow 'a': slow");
    }

    /// The JSON key stays `rule` even though the Rust field is `check`: the
    /// shape is what `clippy --format json` consumers parse.
    #[test]
    fn the_json_shape_keeps_its_published_key_names() {
        let d = Diagnostic::error("perf.x", "workflow 'a'", "slow").with_location(
            "wf.json",
            Some("tasks[1]"),
            Some((7, 3)),
        );
        let json = d.render_json();
        assert_eq!(json["rule"], "perf.x");
        assert_eq!(json["level"], "error");
        assert_eq!(json["line"], 7);
        assert_eq!(json["column"], 3);
        assert!(json.get("check").is_none(), "the wire name is `rule`");
    }

    #[test]
    fn preflight_renders_its_three_line_form() {
        let d = Diagnostic::error("14", "workflow 'w'", "its stored tasks are not valid JSON")
            .with_remedy("repair the tasks_json column");
        assert_eq!(
            d.render_preflight(),
            "[14] workflow 'w'\n      its stored tasks are not valid JSON\n      fix: repair the \
             tasks_json column"
        );
    }

    /// The defect the merge exists to fix: a `FieldError`'s path and code used
    /// to be flattened into one prose string.
    #[test]
    fn a_field_error_keeps_its_path() {
        let err = orion_api::FieldError::new(
            "tasks[0].function.name",
            "unknown_value",
            "no such function",
        );
        let d = Diagnostic::from_field_error("schema.workflow", "workflow 'w'", &err);
        assert_eq!(d.path.as_deref(), Some("tasks[0].function.name"));
        assert_eq!(d.message, "no such function");
    }
}
