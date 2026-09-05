//! Loading a definition directory and judging it — once, for every surface
//! that does it.
//!
//! `lint <dir>` and `clippy <dir>` each spelled out the same sequence: load the
//! directory, warn about files that were not readable JSON, note the ones that
//! are not definitions, refuse an empty set, then merge the loader's own
//! findings with [`check`]'s. Five steps, in an order that matters — a set lint
//! that silently skips a file reports green over a set it did not finish
//! reading, which is the failure `lint` exists to remove rather than relocate.
//!
//! Two copies of a five-step sequence is two places for step two to go missing.
//! It is one function now, and what stays with the callers is what genuinely
//! differs: how they render, and what they do about an empty set (`lint`
//! returns an error naming what a definition looks like; `clippy` prints and
//! exits `2`).
//!
//! [`check`]: super::check

use std::path::{Path, PathBuf};

use super::diagnostic::Diagnostic;
use super::{Boundary, DefinitionSet, SharedDefinitions};

/// What to load beyond the compiled set.
#[derive(Clone, Copy, Default)]
pub struct GateOpts {
    /// Require every definition to declare its id — `lint --require-ids` and
    /// the package boundary want this; `clippy` does not.
    pub require_ids: bool,
    /// Also load the **source** form, before the authoring passes run.
    /// `clippy`'s duplication rules read it, because two documents that share
    /// a `$from` are not duplicates of each other.
    pub want_raw: bool,
}

/// One directory, loaded and judged.
pub struct GateReport {
    /// The compiled set — what the admin API would accept.
    pub set: DefinitionSet,
    /// The source form, when [`GateOpts::want_raw`] asked for it.
    pub raw: Option<DefinitionSet>,
    pub shared: SharedDefinitions,
    /// Authoring pass id → documents rewritten, so an author can see what the
    /// compiler did without diffing the output.
    pub compiled: std::collections::BTreeMap<&'static str, usize>,
    /// The loader's findings and the check pass's, in that order — they are
    /// the same class of problem and share exit rules.
    pub findings: Vec<Diagnostic>,
    /// Files that are not readable JSON. Far more likely to be a mistake than
    /// [`Self::skipped`], and reported differently for that reason.
    pub unparseable: Vec<(PathBuf, String)>,
    /// Files that parsed but are no entity kind.
    pub skipped: Vec<PathBuf>,
}

impl GateReport {
    pub fn errors(&self) -> usize {
        self.findings.iter().filter(|f| f.is_error()).count()
    }

    /// Counted by severity rather than as "everything that is not an error":
    /// the findings also carry inventory notes, and gating on those made
    /// `--deny-warnings` fail on any set that references an environment
    /// variable.
    pub fn warnings(&self) -> usize {
        self.findings.iter().filter(|f| f.is_warning()).count()
    }

    /// The lines a surface prints about files it did not read, in the order
    /// they should appear. Returned rather than printed so the gate stays
    /// usable from a test and from a surface that renders differently.
    pub fn notices(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.unparseable.len() + self.skipped.len());
        for (path, error) in &self.unparseable {
            out.push(format!(
                "warning: {} is not readable JSON: {error}",
                path.display()
            ));
        }
        for path in &self.skipped {
            out.push(format!(
                "note: {} is not a channel, workflow or connector — skipped",
                path.display()
            ));
        }
        out
    }
}

/// Load `dir` as a definition set and run the cross-reference pass over it.
///
/// Does not decide what an empty set means — that answer differs per surface,
/// and both callers say something specific about it. Check
/// [`GateReport::set`]`.is_empty()`.
pub fn gate_directory(
    dir: &Path,
    boundary: &Boundary,
    opts: GateOpts,
    plugin_dirs: &[String],
) -> Result<GateReport, String> {
    let raw = if opts.want_raw {
        Some(DefinitionSet::from_directory_raw(dir)?.0)
    } else {
        None
    };
    let (mut set, report) = DefinitionSet::from_directory(dir)?;

    // The loader's findings first — an unresolvable `$from`, a missing
    // fragment, a name defined twice — then the check pass's. Same class,
    // same exit rules, and the loader's come first because a set that did not
    // resolve is what makes the check pass's findings hard to read.
    let mut findings = report.findings;
    // Manifests from outside the tree (`--plugin-dir`) join the ones the walk
    // found, before the registry every check reads is built from them.
    findings.extend(set.add_plugin_dirs(plugin_dirs)?);
    let registry = match set.function_registry() {
        Ok(registry) => registry,
        Err(reason) => {
            findings.push(Diagnostic::error(
                "duplicate.plugin_function",
                "plugins",
                format!("{reason} — the set's plugins could not all be active at once"),
            ));
            crate::engine::FunctionRegistry::builtin()
                .with_entries(Vec::new())
                .expect("the built-in registry extends by nothing")
        }
    };
    findings.extend(super::check(&set, boundary, opts.require_ids, &registry));

    Ok(GateReport {
        set,
        raw,
        shared: report.shared,
        compiled: report.compiled,
        findings,
        unparseable: report.unparseable,
        skipped: report.skipped,
    })
}
