use orion_authoring::definitions::Boundary;

use super::{
    Catalog, boxed, format_lint_error, load_and_gate, offline_registry, placeholder_entries,
    read_expanded_workflow, unverifiable_functions,
};

/// Statically validate a workflow JSON file.
///
/// Runs the same checks a workflow in a definition set is held to:
/// name/id/description, task uniqueness, and the A1 function-input
/// schema registry. Exits non-zero with field-pathed errors on
/// failure — wire into CI to catch broken workflows before deploy.
#[derive(clap::Args)]
pub struct LintCmd {
    /// Path to a workflow JSON file, or a directory of definitions.
    ///
    /// A directory is linted as a **set**: every channel, workflow and
    /// connector under it, plus the references between them — the checks
    /// a per-file lint cannot make.
    pub workflow: String,
    /// Channel name that may be referenced without being in the set.
    /// Repeatable. Directory mode only; a package declares this in
    /// `requires`.
    #[arg(long = "requires-channel", value_name = "NAME")]
    pub requires_channels: Vec<String>,
    /// Connector name that may be referenced without being in the set.
    /// Repeatable.
    #[arg(long = "requires-connector", value_name = "NAME")]
    pub requires_connectors: Vec<String>,
    /// Exit non-zero on advisory findings too, not just errors.
    ///
    /// Named for `cargo clippy -- -D warnings` rather than `--strict`,
    /// which would read as a no-op on a command whose whole job is strict
    /// validation.
    #[arg(long)]
    pub deny_warnings: bool,
    /// Directory holding the set's shared definitions — the `constants`,
    /// `errors` and `fragments` documents a `$from` or a `use` resolves
    /// against. Expansion happens before validation, so what is checked
    /// is the expanded form. Implicit when linting a directory.
    #[arg(long, value_name = "DIR")]
    pub definitions: Option<String>,
    /// Directory of plugin manifests (`plugin.toml`) beyond the set's
    /// own tree, so a workflow naming a plugin function is checked
    /// against the manifest. Repeatable. Without one such a function is
    /// reported as unverifiable, never as an error.
    #[arg(long = "plugin-dir", value_name = "DIR")]
    pub plugin_dirs: Vec<String>,
    /// The target node's capabilities, as `orion-cli deployments
    /// capabilities -o json` prints them: validates channels of an edition's
    /// protocols (e.g. `kafka`) against that node's drivers instead of
    /// refusing them as unknown
    #[arg(long, value_name = "FILE")]
    pub capabilities: Option<String>,
}

impl LintCmd {
    pub fn run(&self) -> anyhow::Result<i32> {
        let boundary = Boundary {
            channels: self.requires_channels.clone(),
            connectors: self.requires_connectors.clone(),
            channel_drivers: super::capabilities_drivers(self.capabilities.as_deref())
                .map_err(boxed)?,
        };
        run_lint(
            &self.workflow,
            self.deny_warnings,
            boundary,
            self.definitions.as_deref(),
            &self.plugin_dirs,
        )
        .map_err(boxed)?;
        Ok(0)
    }
}

/// Lint a workflow JSON file. Mirrors the checks the admin create
/// endpoint performs (`validate_create_workflow` plus A1 function-input
/// schema validation), printing field-pathed errors and exiting non-zero
/// on failure so this can be wired into pre-commit / CI hooks.
/// `deny_warnings` promotes the advisory findings to failures, for a PR gate
/// that wants them to block.
pub(crate) fn run_lint(
    workflow_path: &str,
    deny_warnings: bool,
    boundary: orion_authoring::definitions::Boundary,
    definitions: Option<&str>,
    plugin_dirs: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    use orion_authoring::definitions::documents::CreateWorkflowRequest;

    // A directory is a definition set, and the checks that matter there are
    // the ones *between* files — which a per-file lint cannot see by
    // construction (#286).
    if std::path::Path::new(workflow_path).is_dir() {
        return run_lint_set(workflow_path, deny_warnings, boundary, plugin_dirs);
    }

    let catalog = Catalog::load_opt(definitions, plugin_dirs)?;
    let doc = read_expanded_workflow(workflow_path, catalog.as_ref())?;
    let req: CreateWorkflowRequest = serde_json::from_value(doc)
        .map_err(|e| format!("'{workflow_path}' is not a valid workflow JSON: {e}"))?;

    // The registry: the built-ins, the functions of every manifest the
    // catalog carries, and a schema-less placeholder for each plugin function
    // no manifest here accounts for — those are unverifiable offline (the
    // admin API checks them against the active plugin), and refusing the
    // file for them would make `lint` unusable for a set whose plugins live
    // elsewhere. They are reported as notes below.
    let registry = offline_registry(catalog.as_ref())?;
    let manifests = catalog
        .as_ref()
        .map(|c| c.plugins.as_slice())
        .unwrap_or(&[]);
    let unverifiable =
        unverifiable_functions(&req.tasks, req.loop_config.as_ref(), &registry, manifests);
    let registry = registry.with_entries(placeholder_entries(&unverifiable))?;

    // No config here: `lint` reads a file, not a server. The default ceiling
    // is used, which can only make lint *stricter* than an instance that has
    // raised `engine.max_loop_iterations` — the safe direction for a
    // pre-flight tool (R20).
    let loop_cap = orion_common::config::EngineConfig::default().max_loop_iterations;
    if let Err(err) =
        orion_authoring::validation::validate_create_workflow(&req, loop_cap, &registry)
    {
        return Err(format_lint_error(workflow_path, err).into());
    }

    for name in &unverifiable {
        eprintln!(
            "{}",
            orion_authoring::definitions::Diagnostic::note(
                "plugin.unverifiable",
                format!("workflow '{}'", req.name),
                format!(
                    "names plugin function '{name}', and no manifest for its plugin was given, \
                     so its input cannot be checked here; the admin API validates it against \
                     the active plugin"
                ),
            )
            .with_remedy("pass --plugin-dir <dir> with the plugin's plugin.toml")
        );
    }

    // Advisory findings the create path does not refuse. On stderr so stdout
    // stays the one-line verdict a script greps.
    //
    // Through `Finding`, in the same shape `check_workflows` gives the same
    // advisory in set mode: the `check` id is what lets a pipeline grandfather
    // one rule instead of reaching for `--deny-warnings` and silencing every
    // rule. Printing it as a bare string here would leave the most-used entry
    // point — one file — as the one that cannot be selected against.
    let mut warnings: Vec<orion_authoring::definitions::Diagnostic> =
        orion_authoring::validation::unresolvable_logic_warnings(
            &req.tasks,
            req.loop_config.as_ref(),
            &registry,
        )
        .into_iter()
        .map(|(path, message)| {
            orion_authoring::definitions::Diagnostic::warning(
                "logic.unresolvable",
                format!("workflow '{}' {path}", req.name),
                message,
            )
        })
        .collect();
    // The informational findings: a stripped `$`, and the two control-flow keys
    // that do nothing. `check_workflow` reports all three and `build` refuses
    // none, so this is the surface where an author still can act on one.
    warnings.extend(
        orion_authoring::validation::engine_advisories(
            &req.tasks,
            req.loop_config.as_ref(),
            &registry,
        )
        .into_iter()
        .map(|advisory| {
            orion_authoring::definitions::Diagnostic::warning(
                advisory.check,
                format!("workflow '{}' {}", req.name, advisory.path),
                advisory.message,
            )
        }),
    );
    for finding in &warnings {
        eprintln!("{finding}");
    }

    if deny_warnings && !warnings.is_empty() {
        return Err(format!(
            "'{workflow_path}' has {} warning(s) and --deny-warnings is set",
            warnings.len()
        )
        .into());
    }

    println!("'{workflow_path}' is valid.");
    Ok(())
}

/// `lint <dir>`: load a definition set and run the cross-reference pass.
///
/// The per-entity validators run here too. A set lint that checked only the
/// references would be a *weaker* gate than the per-file one it is meant to
/// supersede, and an author who pointed `lint` at a directory would silently
/// lose the schema checking they had.
fn run_lint_set(
    dir: &str,
    deny_warnings: bool,
    boundary: orion_authoring::definitions::Boundary,
    plugin_dirs: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    // `false`: a directory being authored may hold a workflow with no id yet.
    // A package must carry explicit ids because channels reference them across
    // the artifact; a directory has no such contract, and refusing an id-less
    // draft would make the gate unusable exactly when it is most wanted.
    load_and_gate(dir, boundary, false, deny_warnings, plugin_dirs)?;
    Ok(())
}
