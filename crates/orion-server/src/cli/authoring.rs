// ============================================================
// A6: CLI subcommands — lint / dry-run / test-connectivity
// ============================================================

/// Lint a workflow JSON file. Mirrors the checks the admin create
/// endpoint performs (`validate_create_workflow` plus A1 function-input
/// schema validation), printing field-pathed errors and exiting non-zero
/// on failure so this can be wired into pre-commit / CI hooks.
/// `deny_warnings` promotes the advisory findings to failures, for a PR gate
/// that wants them to block.
pub(crate) fn run_lint(
    workflow_path: &str,
    deny_warnings: bool,
    boundary: orion::definitions::Boundary,
    definitions: Option<&str>,
    plugin_dirs: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    use orion::storage::models::CreateWorkflowRequest;

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
    let loop_cap = orion::config::EngineConfig::default().max_loop_iterations;
    if let Err(err) = orion::validation::validate_create_workflow(&req, loop_cap, &registry) {
        return Err(format_lint_error(workflow_path, err).into());
    }

    for name in &unverifiable {
        eprintln!(
            "{}",
            orion::definitions::Diagnostic::note(
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
    let mut warnings: Vec<orion::definitions::Diagnostic> =
        orion::validation::unresolvable_logic_warnings(
            &req.tasks,
            req.loop_config.as_ref(),
            &registry,
        )
        .into_iter()
        .map(|(path, message)| {
            orion::definitions::Diagnostic::warning(
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
        orion::validation::engine_advisories(&req.tasks, req.loop_config.as_ref(), &registry)
            .into_iter()
            .map(|advisory| {
                orion::definitions::Diagnostic::warning(
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

/// Read a workflow file and expand its shared references, if a catalog is
/// named.
///
/// The one place `lint <file>`, `dry-run` and `test` all pass through, so a
/// `$from` means the same thing to each of them. Expansion is on the raw JSON,
/// before `CreateWorkflowRequest` parses, because everything downstream —
/// validation, the synthetic row, the dataflow conversion — must see the
/// expanded form or the offline gates stop covering what actually runs.
pub(crate) fn read_expanded_workflow(
    path: &str,
    definitions: Option<&Catalog>,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("Failed to read '{path}': {e}"))?;
    let mut doc: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("'{path}' is not valid JSON: {e}"))?;

    // Every document runs the pipeline, catalog or not: a `$sql` file
    // reference resolves against the workflow's own directory and needs no
    // `--definitions`. The set root, when one is named, bounds where it may
    // point.
    let catalog = definitions.filter(|c| c.dir.is_some());
    let empty = orion::definitions::SharedDefinitions::default();
    let base_dir = std::path::Path::new(path).parent().map(|p| {
        if p.as_os_str().is_empty() {
            std::path::PathBuf::from(".")
        } else {
            p.to_path_buf()
        }
    });
    let root = catalog
        .and_then(|c| c.dir.as_deref())
        .map(std::path::Path::new);
    let mut findings = Vec::new();
    orion::definitions::compile::compile(
        &mut doc,
        &orion::definitions::Cx {
            shared: catalog.map_or(&empty, |c| &c.shared),
            origin: path,
            base_dir: base_dir.as_deref(),
            root,
        },
        &mut findings,
    );

    let errors = findings.iter().filter(|f| f.is_error()).count();
    for finding in &findings {
        eprintln!("{finding}");
    }
    if errors > 0 {
        // An unresolved reference cannot be run past — the document that
        // reaches the engine would be missing whatever the reference stood for.
        return Err(match catalog.and_then(|c| c.dir.as_deref()) {
            Some(dir) => {
                format!("{errors} unresolved reference(s) expanding '{path}' against '{dir}'")
            }
            // Without a catalog, a fragment or shared value cannot resolve —
            // say that, rather than leave the author reading a finding about
            // a name that is simply not in scope.
            None if findings
                .iter()
                .any(|f| matches!(f.check, "closure.fragment" | "closure.shared_value")) =>
            {
                format!(
                    "{errors} unresolved reference(s) in '{path}': no --definitions directory \
                     was given to resolve it against"
                )
            }
            None => format!("{errors} unresolved reference(s) in '{path}'"),
        }
        .into());
    }
    Ok(doc)
}

/// A `--definitions` catalog, loaded once.
///
/// Loading walks the whole tree and parses every JSON file under it. The
/// `test` runner expands a workflow per case, so taking a directory path here
/// meant re-reading and re-parsing all of them — and re-printing the load's
/// findings — once per case, which on a set of any size is most of what the
/// run does. The catalog is immutable once built, so one load serves every
/// case.
pub(crate) struct Catalog {
    /// The `--definitions` directory, for the message when a reference does
    /// not resolve against it. `None` when only `--plugin-dir` was given.
    dir: Option<String>,
    shared: orion::definitions::SharedDefinitions,
    /// The plugin manifests found under the definitions directory and every
    /// `--plugin-dir`, with the component beside each when there is one.
    /// What the registry validates against, and what an offline run loads.
    pub(super) plugins: Vec<orion::definitions::PluginDefinition>,
    /// The sandbox over those components, built on first use: a `test` run
    /// over many cases compiles each component once.
    sandbox: std::sync::OnceLock<Result<OfflineSandbox, String>>,
}

/// The plugin components an offline run executes — for real, in the same
/// sandbox the server uses, with the host's default ceilings.
pub(crate) struct OfflineSandbox {
    runtime: std::sync::Arc<orion::plugin::WasmRuntime>,
    /// Plugin id → its compiled component. A manifest with no component
    /// beside it is absent here, and a run naming one of its functions fails
    /// as `PLUGIN_ARTIFACT_UNAVAILABLE` rather than being stubbed.
    loaded: std::collections::HashMap<String, std::sync::Arc<orion::plugin::LoadedComponent>>,
    config: orion::config::PluginsConfig,
}

impl Catalog {
    /// Load the catalog under `dir`, reporting what it found once.
    pub(crate) fn load(
        dir: Option<&str>,
        plugin_dirs: &[String],
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut findings = Vec::new();
        let mut shared = orion::definitions::SharedDefinitions::default();
        let mut set = orion::definitions::DefinitionSet::default();
        if let Some(dir) = dir {
            let (loaded, shared_findings) =
                orion::definitions::SharedDefinitions::from_directory(std::path::Path::new(dir))?;
            // Before anything the catalog reports: a set this binary is too
            // old for says so in one line.
            if let Some(decl) = &loaded.package {
                decl.check_this_binary()?;
            }
            shared = loaded;
            findings.extend(shared_findings);
            // The manifests in the definitions tree are part of its catalog
            // as much as its shared values are.
            findings.extend(set.add_plugin_dirs(&[dir.to_string()])?);
        }
        findings.extend(set.add_plugin_dirs(plugin_dirs)?);
        let errors = findings.iter().filter(|f| f.is_error()).count();
        for finding in &findings {
            eprintln!("{finding}");
        }
        if errors > 0 {
            return Err(format!(
                "{errors} error(s) in the definitions under '{}'",
                dir.unwrap_or("--plugin-dir")
            )
            .into());
        }
        Ok(Self {
            dir: dir.map(str::to_string),
            shared,
            plugins: set.plugins,
            sandbox: std::sync::OnceLock::new(),
        })
    }

    /// [`Self::load`] for the optional `--definitions` and `--plugin-dir`
    /// arguments: `None` when neither was given.
    pub(crate) fn load_opt(
        dir: Option<&str>,
        plugin_dirs: &[String],
    ) -> Result<Option<Self>, Box<dyn std::error::Error>> {
        if dir.is_none() && plugin_dirs.is_empty() {
            return Ok(None);
        }
        Self::load(dir, plugin_dirs).map(Some)
    }

    /// The registry an offline command validates against: the built-ins plus
    /// every function the catalog's manifests declare.
    pub(crate) fn registry(
        &self,
    ) -> Result<orion::engine::FunctionRegistry, Box<dyn std::error::Error>> {
        let set = orion::definitions::DefinitionSet {
            plugins: self.plugins.clone(),
            ..orion::definitions::DefinitionSet::default()
        };
        Ok(set.function_registry()?)
    }

    /// The compiled components, built once.
    fn sandbox(&self) -> Result<&OfflineSandbox, Box<dyn std::error::Error>> {
        self.sandbox
            .get_or_init(|| {
                let config = orion::config::PluginsConfig {
                    enabled: true,
                    ..orion::config::PluginsConfig::default()
                };
                let runtime = orion::plugin::WasmRuntime::new(&config)
                    .map_err(|e| format!("the plugin sandbox could not be created: {e}"))?;
                let mut loaded = std::collections::HashMap::new();
                for plugin in &self.plugins {
                    let Some(path) = &plugin.component_path else {
                        continue;
                    };
                    let bytes = std::fs::read(path)
                        .map_err(|e| format!("reading component '{}': {e}", path.display()))?;
                    let component = runtime.load_blocking(&bytes).map_err(|e| {
                        format!(
                            "plugin '{}': component '{}' does not load: {e}",
                            plugin.manifest.name,
                            path.display()
                        )
                    })?;
                    loaded.insert(plugin.manifest.name.clone(), component);
                }
                Ok(OfflineSandbox {
                    runtime,
                    loaded,
                    config,
                })
            })
            .as_ref()
            .map_err(|e| e.clone().into())
    }

    /// One function per function of every plugin whose component is here,
    /// for an engine that runs them for real; and the functions of the
    /// plugins whose component is *not* here, which such an engine must
    /// refuse to build against rather than stub.
    pub(crate) fn plugin_handlers(
        &self,
    ) -> Result<OfflinePluginHandlers, Box<dyn std::error::Error>> {
        let mut out = OfflinePluginHandlers::default();
        if self.plugins.is_empty() {
            return Ok(out);
        }
        let sandbox = self.sandbox()?;
        let handlers = &mut out.handlers;
        let unavailable = &mut out.unavailable;
        for plugin in &self.plugins {
            match sandbox.loaded.get(&plugin.manifest.name) {
                Some(component) => {
                    let limits =
                        orion::plugin::Limits::effective(&sandbox.config, &plugin.manifest.name);
                    for (entry, decl) in
                        plugin.entries().into_iter().zip(&plugin.manifest.functions)
                    {
                        let handler = orion::plugin::PluginFunctionHandler::new(
                            std::sync::Arc::new(entry),
                            component.clone(),
                            sandbox.runtime.clone(),
                            limits,
                        );
                        handlers.push(orion::plugin::PluginFunction::new(
                            handler,
                            decl.runtime_fields(),
                        ));
                    }
                }
                None => {
                    for name in plugin.manifest.function_names() {
                        unavailable.push((name.to_string(), plugin.origin.clone()));
                    }
                }
            }
        }
        Ok(out)
    }
}

/// What an offline run gets from the catalog's plugins.
#[derive(Default)]
pub(crate) struct OfflinePluginHandlers {
    /// The functions of every plugin whose component loaded.
    pub(crate) handlers: Vec<orion::plugin::PluginFunction>,
    /// Function name → the manifest's origin, for every plugin whose
    /// component is not beside its manifest and so cannot run here.
    pub(crate) unavailable: Vec<(String, String)>,
}

/// The registry an offline command reads: the catalog's when there is one,
/// the built-ins otherwise.
pub(super) fn offline_registry(
    catalog: Option<&Catalog>,
) -> Result<orion::engine::FunctionRegistry, Box<dyn std::error::Error>> {
    match catalog {
        Some(catalog) => catalog.registry(),
        None => Ok(orion::engine::FunctionRegistry::builtin()
            .with_entries(Vec::new())
            .expect("the built-in registry extends by nothing")),
    }
}

/// Every plugin function `tasks` names that `registry` cannot account for —
/// a dotted name with no manifest behind it. Registered as schema-less
/// placeholders so the create-time validator does not refuse the workflow
/// for a name only the serving instance can verify; the caller says so with
/// a `plugin.unverifiable` note.
///
/// A name under a plugin one of `manifests` *does* describe is not
/// unverifiable: the manifest is the authority, and a function it does not
/// declare stays an unknown function — the same distinction the set-mode
/// check draws with `closure.plugin`.
pub(super) fn unverifiable_functions(
    tasks: &serde_json::Value,
    loop_config: Option<&serde_json::Value>,
    registry: &orion::engine::FunctionRegistry,
    manifests: &[orion::definitions::PluginDefinition],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for task in orion::engine::leaf_tasks(tasks, loop_config) {
        let Some(name) = task
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        let covered = manifests
            .iter()
            .any(|p| name.starts_with(&format!("{}.", p.manifest.name)));
        if name.contains('.')
            && !covered
            && !registry.contains(name)
            && !out.iter().any(|n| n == name)
        {
            out.push(name.to_string());
        }
    }
    out
}

/// [`unverifiable_functions`] as registry entries: no schema, so nothing
/// about their input is checked, exactly like an engine built-in.
fn placeholder_entries(names: &[String]) -> Vec<orion::engine::FunctionEntry> {
    names
        .iter()
        .map(|name| orion::engine::FunctionEntry {
            name: name.clone(),
            description: "plugin function with no manifest in this run".to_string(),
            category: "plugin".to_string(),
            source: orion::engine::functions::schema::Source::Plugin,
            input_fields: None,
            writes: orion::engine::functions::schema::WriteShape::OutputPath { default_root: None },
            retry_safety: orion::engine::functions::schema::RetrySafety::Pure,
            deny_unknown: false,
            validate_static: None,
            connector: None,
            plugin: None,
        })
        .collect()
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
    boundary: orion::definitions::Boundary,
    plugin_dirs: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    // `false`: a directory being authored may hold a workflow with no id yet.
    // A package must carry explicit ids because channels reference them across
    // the artifact; a directory has no such contract, and refusing an id-less
    // draft would make the gate unusable exactly when it is most wanted.
    load_and_gate(dir, boundary, false, deny_warnings, plugin_dirs)?;
    Ok(())
}

/// Load a definition set, compile it, and run every gate `lint <dir>` runs.
///
/// Shared with `compile <dir>`, which is `lint` plus an emitter: a compile
/// that wrote out a set its own linter would reject is how a deployment comes
/// to fail at apply having passed CI. The two differ in their boundary and in
/// `require_ids`, and in nothing else, on purpose.
fn load_and_gate(
    dir: &str,
    boundary: orion::definitions::Boundary,
    require_ids: bool,
    deny_warnings: bool,
    plugin_dirs: &[String],
) -> Result<
    (
        orion::definitions::DefinitionSet,
        Option<orion::definitions::PackageDecl>,
    ),
    Box<dyn std::error::Error>,
> {
    let report = orion::definitions::gate_directory(
        std::path::Path::new(dir),
        &boundary,
        orion::definitions::GateOpts {
            require_ids,
            want_raw: false,
        },
        plugin_dirs,
    )?;

    // Say what was not read. A set lint that silently ignores a file reports
    // green over a set it did not finish reading, which is the failure this
    // command exists to remove rather than relocate.
    for notice in report.notices() {
        eprintln!("{notice}");
    }

    let errors = report.errors();
    let warnings = report.warnings();
    for finding in &report.findings {
        eprintln!("{finding}");
    }

    // "Nothing here" is only true when nothing was *rejected* either. A file
    // the walk recognised and the validator then refused leaves the set
    // empty while the reason sits in the findings above, and returning here
    // first reported a manifest with a bad shape as not a manifest at all —
    // citing, as the reason it was ignored, the `abi` rule the file
    // satisfied. A set that refused everything it read fails below with what
    // was wrong.
    if report.set.is_empty() && errors == 0 {
        return Err(format!(
            "no definitions found under '{dir}'. A definition is a JSON object with \
             'tasks' (workflow), 'channel_type' (channel) or 'connector_type' (connector)."
        )
        .into());
    }

    for (pass, count) in &report.compiled {
        println!("compiled: {pass} rewrote {count} document(s)");
    }

    use orion::definitions::Entity;
    let shared = if report.shared.is_empty() {
        String::new()
    } else {
        format!(
            ", {} shared value(s), {} fragment(s)",
            report
                .shared
                .namespaces
                .values()
                .map(|n| n.len())
                .sum::<usize>(),
            report.shared.fragments.len(),
        )
    };
    println!(
        "{dir}: {} connector(s), {} workflow(s), {} channel(s){shared} — {errors} \
         error(s), {warnings} warning(s)",
        report.set.count(Entity::Connector),
        report.set.count(Entity::Workflow),
        report.set.count(Entity::Channel),
    );

    if errors > 0 {
        return Err(format!("{errors} error(s) in '{dir}'").into());
    }
    if deny_warnings && warnings > 0 {
        return Err(format!("{warnings} warning(s) in '{dir}' and --deny-warnings is set").into());
    }
    Ok((report.set, report.shared.package))
}

/// What `compile` writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum CompileFormat {
    /// A revision-free v2 deployment directory: `deployment.json`,
    /// `definitions/<hex>.json` and `connections/<id>.json`, the shape
    /// `orion-cli deployments apply -d` deploys.
    Deployment,
}

/// `compile <dir>`: gate a definition set, then write it out in a form a
/// node deploys.
///
/// The authoring conveniences a set may use — `$from`, `use` — resolve when
/// the set is *loaded*, and the admin API loads no set, so this is the step
/// between the two.
///
/// Gate first, emit second, and the gate is `lint <dir>`'s own
/// ([`load_and_gate`]) rather than a second one written beside it: a compile
/// that emitted a set its own linter rejects is how a deployment comes to
/// fail at apply having passed CI.
pub(crate) struct CompileRequest<'a> {
    pub(crate) dir: &'a str,
    pub(crate) output: Option<&'a str>,
    pub(crate) format: CompileFormat,
    pub(crate) deny_warnings: bool,
    /// Directories of plugin manifests beyond the set's own tree.
    pub(crate) plugin_dirs: &'a [String],
}

pub(crate) fn run_compile(req: CompileRequest<'_>) -> Result<(), Box<dyn std::error::Error>> {
    // A deployment's members are addressed by id: a member is its kind and
    // id, and the target matches a recompiled one to what it deploys by them.
    // A deployment is the whole of what the target serves, so nothing it
    // references may live outside it: the boundary is empty.
    let (set, _) = load_and_gate(
        req.dir,
        orion::definitions::Boundary::default(),
        true,
        req.deny_warnings,
        req.plugin_dirs,
    )?;
    match req.format {
        CompileFormat::Deployment => {
            emit_deployment(&set, require_output(req.output, "--format deployment")?)
        }
    }
}

/// Write the set as a revision-free deployment directory
/// ([`orion::definitions::deployment`]).
///
/// The files a previous compile left in `definitions/` and `connections/`
/// that this one does not write are removed: they are named by digest and
/// id, so a stale one is a definition the set no longer has, and a deployer
/// that read the directory would upload it for nothing.
fn emit_deployment(
    set: &orion::definitions::DefinitionSet,
    out: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let deployment = match orion::definitions::deployment::build(set) {
        Ok(deployment) => deployment,
        Err(errors) => {
            for error in &errors {
                eprintln!("error: {error}");
            }
            return Err(format!(
                "{} error(s) compiling the deployment; nothing was written",
                errors.len()
            )
            .into());
        }
    };
    for note in &deployment.notes {
        eprintln!("{note}");
    }
    let root = std::path::Path::new(out);
    for sub in ["definitions", "connections"] {
        let dir = root.join(sub);
        std::fs::create_dir_all(&dir).map_err(|e| format!("create '{}': {e}", dir.display()))?;
        for entry in
            std::fs::read_dir(&dir).map_err(|e| format!("read '{}': {e}", dir.display()))?
        {
            let path = entry?.path();
            let relative = format!(
                "{sub}/{}",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
            if path.extension().is_some_and(|ext| ext == "json")
                && !deployment.files.contains_key(&relative)
            {
                std::fs::remove_file(&path)
                    .map_err(|e| format!("remove stale '{}': {e}", path.display()))?;
            }
        }
    }
    for (relative, bytes) in &deployment.files {
        let path = root.join(relative);
        std::fs::write(&path, bytes).map_err(|e| format!("write '{}': {e}", path.display()))?;
    }
    let counts = deployment.counts;
    println!(
        "wrote a deployment of {} workflow(s), {} channel(s), {} plugin(s) and {} \
         connection(s) to {out}",
        counts.workflows, counts.channels, counts.plugins, counts.connections
    );
    Ok(())
}

/// A deployment is several files, so there is nowhere for a stdout default
/// to put them.
fn require_output<'a>(
    output: Option<&'a str>,
    what: &str,
) -> Result<&'a str, Box<dyn std::error::Error>> {
    output.ok_or_else(|| format!("-o <DIR> is required for {what}").into())
}

// ============================================================
// fmt — one canonical layout for every definition file
// ============================================================

/// `fmt [PATH]... [--check] [--stdin]`: rewrite definition files to the house
/// style, the way `cargo fmt` rewrites Rust.
///
/// Returns the process exit code rather than an error, because the three
/// outcomes are not one failure: `0` clean or written, `1` `--check` found a
/// file it would rewrite, `2` a file could not be read, parsed or written.
/// Every file is attempted — one unparseable fixture must not leave the rest
/// of the tree unformatted — and the code reflects the worst outcome.
///
/// Diffs and errors go to stderr, the one-line summary to stdout, the same
/// split `lint` makes so a script can grep the verdict.
pub(crate) fn run_fmt(
    paths: &[String],
    check: bool,
    stdin: bool,
) -> Result<i32, Box<dyn std::error::Error>> {
    use orion::definitions::fmt::{FmtError, Outcome, format_str};

    if stdin {
        return run_fmt_stdin();
    }

    let mut files: Vec<std::path::PathBuf> = Vec::new();
    let mut errors = 0usize;
    for path in paths {
        let path = std::path::Path::new(path);
        if path.is_dir() {
            match orion::definitions::json_files(path) {
                Ok(found) => files.extend(found),
                Err(e) => {
                    eprintln!("error: {e}");
                    errors += 1;
                }
            }
        } else if path.is_file() {
            // Named explicitly, so taken as given whatever its extension.
            files.push(path.to_path_buf());
        } else {
            eprintln!("error: '{}' is not a file or directory", path.display());
            errors += 1;
        }
    }

    // A set declaring a range this binary is outside of is not formatted at
    // all: the style tables differ between versions, so formatting with the
    // wrong binary is itself the failure. Only a directory's files are
    // consulted — `fmt examples/` spans many sets, each checked on its own.
    for path in paths {
        let path = std::path::Path::new(path);
        if !path.is_dir() {
            continue;
        }
        for file in orion::definitions::json_files(path).unwrap_or_default() {
            let Some(doc) = std::fs::read_to_string(&file)
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                .filter(orion::definitions::SharedDefinitions::is_package_declaration)
            else {
                continue;
            };
            let decl = orion::definitions::PackageDecl {
                origin: file.display().to_string(),
                name: None,
                requires_orion: doc["package"]["requires"]["orion"]
                    .as_str()
                    .map(str::to_string),
            };
            if let Err(e) = decl.check_this_binary() {
                eprintln!("error: {e} — nothing was formatted");
                return Ok(2);
            }
        }
    }

    let mut changed = 0usize;
    let mut unchanged = 0usize;
    for file in &files {
        let shown = file.display();
        let text = match std::fs::read(file) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(e) => {
                    eprintln!(
                        "error: {shown}: not valid UTF-8 at byte {}",
                        e.utf8_error().valid_up_to()
                    );
                    errors += 1;
                    continue;
                }
            },
            Err(e) => {
                eprintln!("error: {shown}: {e}");
                errors += 1;
                continue;
            }
        };
        match format_str(&text, &shown.to_string()) {
            Ok(Outcome::Unchanged) => unchanged += 1,
            Ok(Outcome::Changed(formatted)) => {
                changed += 1;
                if check {
                    eprint!("{}", unified_diff(&shown.to_string(), &text, &formatted));
                } else if let Err(e) = write_atomically(file, &formatted) {
                    eprintln!("error: {shown}: {e}");
                    errors += 1;
                }
            }
            Err(FmtError::Parse(e)) => {
                eprintln!("error: {shown}: {e}");
                errors += 1;
            }
            Err(e) => {
                eprintln!("error: {e}");
                errors += 1;
            }
        }
    }

    let verb = if check {
        "would be reformatted"
    } else {
        "reformatted"
    };
    println!(
        "{changed} file(s) {verb}, {unchanged} unchanged{}",
        if errors > 0 {
            format!(", {errors} error(s)")
        } else {
            String::new()
        }
    );
    Ok(if errors > 0 {
        2
    } else if check && changed > 0 {
        1
    } else {
        0
    })
}

/// `fmt --stdin`: one document in, its formatted form out. Nothing reaches
/// stdout on failure, so an editor that replaces the buffer with the output
/// never replaces it with an error message.
fn run_fmt_stdin() -> Result<i32, Box<dyn std::error::Error>> {
    use orion::definitions::fmt::{Outcome, format_str};
    use std::io::{Read, Write};

    let mut text = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut text) {
        eprintln!("error: <stdin>: {e}");
        return Ok(2);
    }
    match format_str(&text, "<stdin>") {
        Ok(Outcome::Unchanged) => {
            std::io::stdout().write_all(text.as_bytes())?;
            Ok(0)
        }
        Ok(Outcome::Changed(formatted)) => {
            std::io::stdout().write_all(formatted.as_bytes())?;
            Ok(0)
        }
        Err(e) => {
            eprintln!("error: <stdin>: {e}");
            Ok(2)
        }
    }
}

/// A unified diff in the shape `git diff` prints, so the `--check` output
/// reads in a terminal and applies with `patch -p1`.
fn unified_diff(path: &str, before: &str, after: &str) -> String {
    // An absolute path would print as `a//Users/…`; `git diff` headers are
    // relative, and so are these.
    let path = path.trim_start_matches('/');
    similar::TextDiff::from_lines(before, after)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

/// Replace `path`'s contents without a window in which it is half-written:
/// write a sibling temp file, copy the original's permissions onto it, and
/// rename it over the original. On any failure the temp file is removed and
/// the original is untouched.
///
/// A symlink is resolved first, because renaming over the link's own path
/// would replace the link with a regular file rather than update its target.
fn write_atomically(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    let target = path.canonicalize()?;
    let dir = target
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let tmp = dir.join(format!(".{name}.fmt-tmp-{}", std::process::id()));

    let attempt = (|| {
        std::fs::write(&tmp, content)?;
        let permissions = std::fs::metadata(&target)?.permissions();
        std::fs::set_permissions(&tmp, permissions)?;
        std::fs::rename(&tmp, &target)
    })();
    if attempt.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    attempt
}

// ============================================================
// clippy — what a set could do better, said only when certain
// ============================================================

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
    pub(crate) boundary: orion::definitions::Boundary,
    /// The serving instance's config when `-c` named one — the rules that
    /// need it are skipped otherwise, and say so.
    pub(crate) config: Option<&'a orion::config::AppConfig>,
    /// Apply the fixes the rules proved to the source files.
    pub(crate) fix: bool,
    /// With `fix`: print the diffs and write nothing.
    pub(crate) fix_check: bool,
}

/// `clippy --list`: the registry as a table.
pub(crate) fn run_clippy_list() -> Result<i32, Box<dyn std::error::Error>> {
    print!("{}", orion::definitions::clippy::list_table());
    Ok(0)
}

/// `clippy --explain <rule>`.
pub(crate) fn run_clippy_explain(rule: &str) -> Result<i32, Box<dyn std::error::Error>> {
    match orion::definitions::clippy::find(rule) {
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
    raw: orion::definitions::DefinitionSet,
    compiled: orion::definitions::DefinitionSet,
    shared: orion::definitions::SharedDefinitions,
    findings: Vec<orion::definitions::clippy::Diagnostic>,
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
    use orion::definitions::Entity;
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
                        orion::definitions::clippy::registry().len() - skipped.len()
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
    use orion::definitions::{DefinitionSet, Entity, SharedDefinitions};

    let path = std::path::Path::new(req.path);
    if path.is_dir() {
        // The same gate `lint <dir>` runs — one sequence, so a file this
        // command cannot read is reported by both or by neither. `want_raw`
        // because the duplication rules read the *source* form: two documents
        // that share a `$from` are not duplicates of each other.
        let report = orion::definitions::gate_directory(
            path,
            &req.boundary,
            orion::definitions::GateOpts {
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
    let mut provenance = orion::definitions::SourceMap::default();
    orion::definitions::compile::compile_with_map(
        &mut compiled_doc,
        &orion::definitions::Cx {
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
        def.spans = orion::definitions::json::Document::parse(&text).ok();
    }
    let mut compiled = DefinitionSet::from_entries([(entity, req.path.to_string(), compiled_doc)]);
    if let Some(def) = compiled.definitions.first_mut() {
        def.provenance = provenance;
    }
    compiled.plugins = plugins;
    let registry = compiled.function_registry()?;
    findings.extend(orion::definitions::check(
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
    diagnostics: Vec<orion::definitions::clippy::Diagnostic>,
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
        let analysis = orion::definitions::analysis::Analysis::new(
            &loaded.raw,
            &loaded.compiled,
            &loaded.shared,
            req.config,
            &registry,
        );
        let report = orion::definitions::clippy::run(&analysis);
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
    diagnostics: &[orion::definitions::clippy::Diagnostic],
    reported: &mut std::collections::BTreeSet<String>,
) -> Result<usize, Box<dyn std::error::Error>> {
    use orion::definitions::fix::{Fix, Refusal};
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
            orion::definitions::compile::compile(
                &mut doc,
                &orion::definitions::Cx {
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
        let outcome = orion::definitions::fix::apply(
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
        orion::definitions::check(&edited, &req.boundary, false, &registry)
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

/// Format an `OrionError::Validation` into a multi-line CLI message
/// listing each field detail with its path and code.
pub(super) fn format_lint_error(workflow_path: &str, err: orion::errors::OrionError) -> String {
    use orion::errors::OrionError;
    match err {
        OrionError::Validation {
            code: _,
            message,
            details,
        } => {
            let mut out = format!("'{workflow_path}' is invalid: {message}\n");
            for d in &details {
                out.push_str(&format!("  - {} [{}]: {}\n", d.path, d.code, d.message));
            }
            out
        }
        other => format!("'{workflow_path}' is invalid: {other}"),
    }
}
