use orion_authoring::definitions::Boundary;

use super::fmt::{unified_diff, write_atomically};
use super::{Catalog, boxed};

/// Advisory checks beyond `lint`, said only when certain (like `cargo clippy`).
///
/// Runs `lint`'s gate over the set, then every rule: a workflow condition
/// that can never match, steps after an unconditional terminal step, an
/// unconditional channel_call cycle, a read of `payload`, a mapping
/// overwritten before it is read, runs of steps an existing fragment
/// already expresses, objects repeated across the set, and more —
/// `--list` names them, `--explain <rule>` states each one's proof and
/// when it stays silent. There is no configuration and no suppression:
/// a rule fires only when its finding is certain.
#[derive(clap::Args)]
pub struct ClippyCmd {
    /// A directory of definitions (set mode: every rule), or one file.
    pub path: Option<String>,
    /// Exit non-zero on warnings too, not just errors.
    #[arg(long)]
    pub deny_warnings: bool,
    /// `text` (default) or `json` — one object per diagnostic on stdout.
    /// Also respects the global `--output json` flag when omitted.
    #[arg(long, value_enum)]
    pub format: Option<ClippyFormat>,
    /// Print every rule with its level, scope and summary, and exit.
    #[arg(long, conflicts_with_all = ["explain", "path"])]
    pub list: bool,
    /// Print one rule's rationale, proof and exclusions, and exit.
    #[arg(long, value_name = "RULE", conflicts_with = "path")]
    pub explain: Option<String>,
    /// Directory holding the set's shared definitions, for a single-file
    /// run. Implicit in set mode.
    #[arg(long, value_name = "DIR")]
    pub definitions: Option<String>,
    /// Channel name that may be referenced without being in the set.
    /// Repeatable.
    #[arg(long = "requires-channel", value_name = "NAME")]
    pub requires_channels: Vec<String>,
    /// Connector name that may be referenced without being in the set.
    /// Repeatable.
    #[arg(long = "requires-connector", value_name = "NAME")]
    pub requires_connectors: Vec<String>,
    /// Directory of plugin manifests beyond the set's own tree, so a
    /// plugin function's template fields are analysed as the server
    /// evaluates them. Repeatable.
    #[arg(long = "plugin-dir", value_name = "DIR")]
    pub plugin_dirs: Vec<String>,
    /// The target node's capabilities, as `orion-cli deployments
    /// capabilities -o json` prints them: validates channels of an edition's
    /// protocols (e.g. `kafka`) against that node's drivers instead of
    /// refusing them as unknown
    #[arg(long, value_name = "FILE")]
    pub capabilities: Option<String>,
    /// Apply the fixes the rules can prove — today, folding a run of
    /// steps that repeat one condition into a task group — to the source
    /// files, each verified by recompiling the edited file, then report
    /// what remains.
    #[arg(long, conflicts_with_all = ["list", "explain"])]
    pub fix: bool,
    /// With --fix: print the diff of each file that would change, write
    /// nothing, and exit 1 when anything would.
    #[arg(long, requires = "fix")]
    pub check: bool,
    /// The server config (`orion-server -c`) whose `[vars]` and `[secrets]`
    /// the `vars.*`/`secrets.*` rules check against. Without one those rules
    /// stay silent: the defaults say nothing about a deployment's config.
    #[arg(short = 'c', long = "server-config", value_name = "FILE")]
    pub server_config: Option<String>,
}

impl ClippyCmd {
    pub fn run(&self, global_output: &crate::output::OutputFormat) -> anyhow::Result<i32> {
        if self.list {
            return run_clippy_list().map_err(boxed);
        }
        if let Some(rule) = &self.explain {
            return run_clippy_explain(rule).map_err(boxed);
        }
        let Some(path) = &self.path else {
            anyhow::bail!("clippy needs a directory or file to check (or --list / --explain)");
        };
        let format = self.format.unwrap_or(match global_output {
            crate::output::OutputFormat::Json => ClippyFormat::Json,
            _ => ClippyFormat::Text,
        });
        // Only a config the author named counts as "the serving config".
        let config = self
            .server_config
            .as_deref()
            .map(|file| orion_common::config::load_config(Some(file)))
            .transpose()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        run_clippy(ClippyRequest {
            path,
            deny_warnings: self.deny_warnings,
            format,
            definitions: self.definitions.as_deref(),
            plugin_dirs: &self.plugin_dirs,
            boundary: Boundary {
                channels: self.requires_channels.clone(),
                connectors: self.requires_connectors.clone(),
                ..super::capabilities_boundary(self.capabilities.as_deref())
                    .map_err(|e| anyhow::anyhow!("{e}"))?
            },
            config: config.as_ref(),
            fix: self.fix,
            fix_check: self.check,
        })
        .map_err(boxed)
    }
}

/// Output format for `clippy`.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ClippyFormat {
    /// `lint`'s line format with a `file:line:col` prefix; diagnostics on
    /// stderr, the summary on stdout.
    Text,
    /// One JSON object per diagnostic on stdout, nothing else.
    Json,
}

pub(crate) struct ClippyRequest<'a> {
    pub(crate) path: &'a str,
    pub(crate) deny_warnings: bool,
    pub(crate) format: ClippyFormat,
    /// Shared-definitions catalog for a single-file run (set mode has its
    /// own).
    pub(crate) definitions: Option<&'a str>,
    /// Directories of plugin manifests beyond the set's own tree.
    pub(crate) plugin_dirs: &'a [String],
    pub(crate) boundary: orion_authoring::definitions::Boundary,
    /// The serving instance's config when `-c` named one — the rules that
    /// need it are skipped otherwise, and say so.
    pub(crate) config: Option<&'a orion_common::config::AppConfig>,
    /// Apply the fixes the rules proved to the source files.
    pub(crate) fix: bool,
    /// With `fix`: print the diffs and write nothing.
    pub(crate) fix_check: bool,
}

/// `clippy --list`: the registry as a table.
pub(crate) fn run_clippy_list() -> Result<i32, Box<dyn std::error::Error>> {
    print!("{}", orion_authoring::definitions::clippy::list_table());
    Ok(0)
}

/// `clippy --explain <rule>`.
pub(crate) fn run_clippy_explain(rule: &str) -> Result<i32, Box<dyn std::error::Error>> {
    match orion_authoring::definitions::clippy::find(rule) {
        Some(found) => {
            println!(
                "{} — {} ({}, {})\n\n{}",
                found.id(),
                found.summary(),
                found.level().as_str(),
                found.scope().as_str(),
                found.explain()
            );
            Ok(0)
        }
        None => {
            eprintln!("error: no rule named '{rule}' — `clippy --list` names them");
            Ok(2)
        }
    }
}

/// How many times `clippy --fix` re-analyses after writing: one pass to fix
/// and one to verify, normally; the cap bounds a fix that keeps exposing
/// another.
const MAX_FIX_PASSES: usize = 8;

/// What `clippy` loaded: the source set, the compiled one, the catalog and
/// the gate's findings.
struct ClippyLoaded {
    raw: orion_authoring::definitions::DefinitionSet,
    compiled: orion_authoring::definitions::DefinitionSet,
    shared: orion_authoring::definitions::SharedDefinitions,
    findings: Vec<orion_authoring::definitions::clippy::Diagnostic>,
}

/// `clippy <path>`: `lint`'s gate first, then every rule over the set.
///
/// Exit `1` on any error — a `lint` error or a `deny` rule — and on a
/// warning under `--deny-warnings`; `2` when the path cannot be read as a
/// set. Rules run only when `lint` is clean: a rule over a document the API
/// would refuse produces a second finding about the same mistake, and a
/// false one.
///
/// With `--fix`, the fixes the rules proved are applied to the source files
/// — each verified by recompiling the edited file — and the set is analysed
/// again; what is reported, and the exit code, are the last analysis. With
/// `--fix --check` nothing is written: the diffs are printed, and exit `1`
/// says something would change.
pub(crate) fn run_clippy(req: ClippyRequest<'_>) -> Result<i32, Box<dyn std::error::Error>> {
    use orion_authoring::definitions::Entity;
    let mut pass = 0usize;
    let mut reported: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut would_change = false;
    loop {
        let Some(loaded) = clippy_load(&req, pass == 0)? else {
            return Ok(2);
        };
        let ClippyAnalysis {
            diagnostics,
            skipped,
            lint_errors,
        } = clippy_analyse(&req, &loaded)?;
        let fixing = req.fix && lint_errors == 0 && pass < MAX_FIX_PASSES;
        let changed = if fixing {
            clippy_fix(&req, &loaded, &diagnostics, &mut reported)?
        } else {
            0
        };
        if changed > 0 && !req.fix_check {
            pass += 1;
            continue;
        }
        would_change |= changed > 0;

        let errors = diagnostics.iter().filter(|d| d.is_error()).count();
        let warnings = diagnostics.iter().filter(|d| d.is_warning()).count();
        match req.format {
            ClippyFormat::Json => {
                for d in &diagnostics {
                    println!("{}", d.render_json());
                }
            }
            ClippyFormat::Text => {
                for d in &diagnostics {
                    eprintln!("{}", d.render_text());
                }
                for rule in &skipped {
                    eprintln!(
                        "note: [{rule}] skipped — needs the serving config (-c <config.toml>)"
                    );
                }
                if lint_errors > 0 {
                    println!(
                        "{}: {lint_errors} lint error(s) — fix those first; clippy's rules did not \
                         run",
                        req.path
                    );
                } else {
                    println!(
                        "{}: {} workflow(s), {} channel(s), {} connector(s) — {errors} error(s), \
                         {warnings} warning(s) from {} rule(s)",
                        req.path,
                        loaded.compiled.count(Entity::Workflow),
                        loaded.compiled.count(Entity::Channel),
                        loaded.compiled.count(Entity::Connector),
                        orion_authoring::definitions::clippy::registry().len() - skipped.len()
                    );
                }
            }
        }
        return Ok(
            if errors > 0 || (req.deny_warnings && warnings > 0) || would_change {
                1
            } else {
                0
            },
        );
    }
}

/// Load the set (or the one file) `clippy` was pointed at. `None` when the
/// path cannot be read as a set — already reported, exit `2`.
fn clippy_load(
    req: &ClippyRequest<'_>,
    report_notices: bool,
) -> Result<Option<ClippyLoaded>, Box<dyn std::error::Error>> {
    use orion_authoring::definitions::{DefinitionSet, Entity, SharedDefinitions};

    let path = std::path::Path::new(req.path);
    if path.is_dir() {
        // The same gate `lint <dir>` runs — one sequence, so a file this
        // command cannot read is reported by both or by neither. `want_raw`
        // because the duplication rules read the *source* form: two documents
        // that share a `$from` are not duplicates of each other.
        let report = orion_authoring::definitions::gate_directory(
            path,
            &req.boundary,
            orion_authoring::definitions::GateOpts {
                require_ids: false,
                want_raw: true,
            },
            req.plugin_dirs,
        )?;
        if report_notices {
            for notice in report.notices() {
                eprintln!("{notice}");
            }
        }
        if report.set.is_empty() {
            eprintln!("error: no definitions found under '{}'", req.path);
            return Ok(None);
        }
        let raw = report
            .raw
            .unwrap_or_else(|| DefinitionSet::from_entries([]));
        return Ok(Some(ClippyLoaded {
            raw,
            compiled: report.set,
            shared: report.shared,
            findings: report.findings,
        }));
    }
    if !path.is_file() {
        eprintln!("error: '{}' is not a file or directory", req.path);
        return Ok(None);
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("read '{}': {e}", req.path))?;
    let doc: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("'{}' is not valid JSON: {e}", req.path))?;
    let Some(entity) = Entity::classify(&doc) else {
        eprintln!(
            "error: '{}' is not a channel, workflow or connector (no 'tasks', 'channel_type' \
             or 'connector_type')",
            req.path
        );
        return Ok(None);
    };
    let catalog = Catalog::load_opt(req.definitions, req.plugin_dirs)?;
    let (shared, plugins) = catalog
        .map(|c| (c.shared, c.plugins))
        .unwrap_or_else(|| (SharedDefinitions::default(), Vec::new()));
    let mut findings = Vec::new();
    let mut compiled_doc = doc.clone();
    let base_dir = clippy_base_dir(req.path);
    let mut provenance = orion_authoring::definitions::SourceMap::default();
    orion_authoring::definitions::compile::compile_with_map(
        &mut compiled_doc,
        &orion_authoring::definitions::Cx {
            shared: &shared,
            origin: req.path,
            base_dir: base_dir.as_deref(),
            root: req.definitions.map(std::path::Path::new),
        },
        &mut findings,
        &mut provenance,
    );
    let mut raw = DefinitionSet::from_entries([(entity, req.path.to_string(), doc)]);
    // The source text, for `--fix` — the edit lands in it.
    if let Some(def) = raw.definitions.first_mut() {
        def.spans = orion_authoring::definitions::json::Document::parse(&text).ok();
    }
    let mut compiled = DefinitionSet::from_entries([(entity, req.path.to_string(), compiled_doc)]);
    if let Some(def) = compiled.definitions.first_mut() {
        def.provenance = provenance;
    }
    compiled.plugins = plugins;
    let registry = compiled.function_registry()?;
    findings.extend(orion_authoring::definitions::check(
        &compiled,
        &req.boundary,
        false,
        &registry,
    ));
    Ok(Some(ClippyLoaded {
        raw,
        compiled,
        shared,
        findings,
    }))
}

/// The directory a file's relative references resolve against.
fn clippy_base_dir(path: &str) -> Option<std::path::PathBuf> {
    std::path::Path::new(path).parent().map(|p| {
        if p.as_os_str().is_empty() {
            std::path::PathBuf::from(".")
        } else {
            p.to_path_buf()
        }
    })
}

/// What one analysis produced: every diagnostic, the rules skipped for want
/// of `-c`, and how many of the diagnostics are `lint` errors.
struct ClippyAnalysis {
    diagnostics: Vec<orion_authoring::definitions::clippy::Diagnostic>,
    skipped: Vec<&'static str>,
    lint_errors: usize,
}

/// The gate's findings plus, when `lint` is clean, every rule's.
fn clippy_analyse(
    req: &ClippyRequest<'_>,
    loaded: &ClippyLoaded,
) -> Result<ClippyAnalysis, Box<dyn std::error::Error>> {
    // No conversion: `check` and the clippy rules report the same type, so
    // a clippy run is a superset of a lint run by construction.
    let mut diagnostics = loaded.findings.clone();
    let lint_errors = diagnostics.iter().filter(|d| d.is_error()).count();
    let mut skipped = Vec::new();
    if lint_errors == 0 {
        // The same registry the gate read: the set's own manifests join the
        // built-ins, so a plugin function's template fields are analysed
        // exactly as the server would evaluate them.
        let registry = loaded.compiled.function_registry()?;
        let analysis = orion_authoring::definitions::analysis::Analysis::new(
            &loaded.raw,
            &loaded.compiled,
            &loaded.shared,
            req.config,
            &registry,
        );
        let report = orion_authoring::definitions::clippy::run(&analysis);
        diagnostics.extend(report.diagnostics);
        skipped = report.skipped;
    }
    Ok(ClippyAnalysis {
        diagnostics,
        skipped,
        lint_errors,
    })
}

/// Apply every fix the rules proved, file by file, each verified against
/// the compiler; then re-check the whole set with the edits in place and
/// drop the edit of any file an error names. Writes the files — or, with
/// `--check`, prints their diffs — and returns how many changed.
fn clippy_fix(
    req: &ClippyRequest<'_>,
    loaded: &ClippyLoaded,
    diagnostics: &[orion_authoring::definitions::clippy::Diagnostic],
    reported: &mut std::collections::BTreeSet<String>,
) -> Result<usize, Box<dyn std::error::Error>> {
    use orion_authoring::definitions::fix::{Fix, Refusal};
    let mut by_file: std::collections::BTreeMap<&str, Vec<(&'static str, Fix)>> =
        std::collections::BTreeMap::new();
    for d in diagnostics {
        if let (Some(fix), Some(file)) = (&d.fix, &d.file) {
            by_file
                .entry(file.as_str())
                .or_default()
                .push((d.check, fix.clone()));
        }
    }
    let mut note = |rule: &str, file: &str, fix: &Fix, refusal: &Refusal| {
        let line = format!("note: [{rule}] {file}: not fixed — {refusal}");
        if reported.insert(format!("{file}\u{1f}{fix}")) {
            eprintln!("{line}");
        }
    };
    let mut edits: Vec<(String, String, serde_json::Value, Vec<Fix>)> = Vec::new();
    for (file, fixes) in &by_file {
        let rule_of = |fix: &Fix| {
            fixes
                .iter()
                .find(|(_, f)| f == fix)
                .map_or("clippy", |(rule, _)| *rule)
        };
        let source = loaded.raw.definitions.iter().find(|d| d.origin == *file);
        let Some(compiled) = loaded
            .compiled
            .definitions
            .iter()
            .find(|d| d.origin == *file)
        else {
            continue;
        };
        let base_dir = clippy_base_dir(file);
        let root = if std::path::Path::new(req.path).is_dir() {
            Some(std::path::Path::new(req.path))
        } else {
            req.definitions.map(std::path::Path::new)
        };
        let recompile = |value: &serde_json::Value| {
            let mut doc = value.clone();
            let mut findings = Vec::new();
            orion_authoring::definitions::compile::compile(
                &mut doc,
                &orion_authoring::definitions::Cx {
                    shared: &loaded.shared,
                    origin: file,
                    base_dir: base_dir.as_deref(),
                    root,
                },
                &mut findings,
            );
            (!findings.iter().any(|f| f.is_error())).then_some(doc)
        };
        let only: Vec<Fix> = fixes.iter().map(|(_, fix)| fix.clone()).collect();
        let outcome = orion_authoring::definitions::fix::apply(
            source.and_then(|d| d.spans.as_ref()),
            &compiled.doc,
            &only,
            &recompile,
        );
        for (fix, refusal) in &outcome.refused {
            note(rule_of(fix), file, fix, refusal);
        }
        if let (Some(text), Some(folded)) = (outcome.text, outcome.compiled) {
            edits.push((file.to_string(), text, folded, outcome.applied));
        }
    }
    if edits.is_empty() {
        return Ok(0);
    }

    // The set with every edit in place: anything set-level — a duplicate id
    // across the estate, a depth the per-file fold could not see — refuses
    // the edit of the file it names.
    let mut edited = loaded.compiled.clone();
    for (file, _, folded, _) in &edits {
        if let Some(def) = edited.definitions.iter_mut().find(|d| d.origin == *file) {
            def.doc = folded.clone();
        }
    }
    let registry = edited.function_registry()?;
    let broken: std::collections::BTreeMap<String, String> =
        orion_authoring::definitions::check(&edited, &req.boundary, false, &registry)
            .into_iter()
            .filter(|f| f.is_error())
            .filter_map(|f| Some((f.file.clone()?, f.message.clone())))
            .collect();

    let mut changed = 0usize;
    for (file, text, _, applied) in edits {
        if let Some(reason) = broken.get(&file) {
            for fix in &applied {
                note(
                    "clippy",
                    &file,
                    fix,
                    &Refusal::Format(format!("the set would not lint afterwards: {reason}")),
                );
            }
            continue;
        }
        let before = std::fs::read_to_string(&file).map_err(|e| format!("read '{file}': {e}"))?;
        if before == text {
            continue;
        }
        changed += 1;
        if req.fix_check {
            print!("{}", unified_diff(&file, &before, &text));
            continue;
        }
        write_atomically(std::path::Path::new(&file), &text)
            .map_err(|e| format!("write '{file}': {e}"))?;
        for fix in &applied {
            eprintln!("fixed {file}: {fix}");
        }
    }
    Ok(changed)
}
