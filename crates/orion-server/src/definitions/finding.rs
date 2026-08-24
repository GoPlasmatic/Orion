//! What a set check reports.

/// Whether a finding fails the command or only informs it.
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
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

/// One thing a set check found.
///
/// Mirrors [`crate::preflight::Finding`], which solved the same reporting
/// problem for the upgrade scan: a stable machine-readable `check`, the entity
/// named the way the author named it, what is wrong, and what to do. The one
/// addition is `severity` — `preflight` reports only breaks.
///
/// `check` is the load-bearing field for tooling. It is what lets a pipeline
/// grandfather one rule rather than reaching for `--deny-warnings` and
/// silencing the lot, and it is the hook a future reference kind (`$from`, a
/// fragment `use`) arrives on without new plumbing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    /// Stable dotted id: `closure.connector`, `duplicate.route_pattern`.
    /// Grouped by the kind of check so a prefix selects a family.
    pub check: &'static str,
    /// What is affected, named as the author named it —
    /// `workflow 'auth-login'`, `channels[2]`.
    pub entity: String,
    pub message: String,
    /// What to change. `None` when the message already says it.
    pub remedy: Option<String>,
}

impl Finding {
    pub fn error(
        check: &'static str,
        entity: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity: Severity::Error,
            check,
            entity: entity.into(),
            message: message.into(),
            remedy: None,
        }
    }

    pub fn warning(
        check: &'static str,
        entity: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity: Severity::Warning,
            check,
            entity: entity.into(),
            message: message.into(),
            remedy: None,
        }
    }

    pub fn with_remedy(mut self, remedy: impl Into<String>) -> Self {
        self.remedy = Some(remedy.into());
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl std::fmt::Display for Finding {
    /// One line, plus an indented remedy when there is one. The `check` id is
    /// printed because it is the thing a pipeline greps for.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: [{}] {}: {}",
            self.severity.as_str(),
            self.check,
            self.entity,
            self.message
        )?;
        if let Some(remedy) = &self.remedy {
            write!(f, "\n        fix: {remedy}")?;
        }
        Ok(())
    }
}
