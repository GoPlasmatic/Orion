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
    model_dirs: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    use orion::storage::repositories::workflows::CreateWorkflowRequest;

    // A directory is a definition set, and the checks that matter there are
    // the ones *between* files — which a per-file lint cannot see by
    // construction (#286).
    if std::path::Path::new(workflow_path).is_dir() {
        return run_lint_set(
            workflow_path,
            deny_warnings,
            boundary,
            plugin_dirs,
            model_dirs,
        );
    }

    let catalog = Catalog::load_opt(definitions, plugin_dirs, model_dirs)?;
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

    // The models the workflow names. With manifests in hand a literal id
    // that matches none of them is the `closure.model` error set mode gives;
    // without any, a literal id is unverifiable here the way a plugin
    // function is — the node it deploys to decides. A computed id is
    // unverifiable everywhere.
    let manifests = catalog.as_ref().map(|c| c.models.as_slice()).unwrap_or(&[]);
    let mut model_errors = 0usize;
    for finding in model_findings(&req.tasks, req.loop_config.as_ref(), &req.name, manifests) {
        if finding.is_error() {
            model_errors += 1;
        }
        eprintln!("{finding}");
    }
    if model_errors > 0 {
        return Err(format!(
            "'{workflow_path}' names {model_errors} model(s) the given manifests do not describe"
        )
        .into());
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
    /// not resolve against it. `None` when only `--plugin-dir` or
    /// `--model-dir` was given.
    dir: Option<String>,
    shared: orion::definitions::SharedDefinitions,
    /// The plugin manifests found under the definitions directory and every
    /// `--plugin-dir`, with the component beside each when there is one.
    /// What the registry validates against, and what an offline run loads.
    pub(super) plugins: Vec<orion::definitions::PluginDefinition>,
    /// The sandbox over those components, built on first use: a `test` run
    /// over many cases compiles each component once.
    sandbox: std::sync::OnceLock<Result<OfflineSandbox, String>>,
    /// The model manifests found under the definitions directory and every
    /// `--model-dir`, each with its artifact hashed and read when the file
    /// is beside it. What a literal `model_infer` reference is checked
    /// against, and what an offline run executes.
    pub(super) models: Vec<orion::definitions::ModelDefinition>,
    /// The runtimes, the resident sessions and the inference slots every
    /// offline run of this catalog shares, built on first use: a `test`
    /// suite loads each model into the runtime once, however many cases
    /// call it. The compiled *set* is per engine and is not here — see
    /// [`Self::model_handler`].
    model_host: std::sync::OnceLock<std::sync::Arc<orion::model::InferenceHost>>,
}

/// The `[models]` an offline run uses: enabled, every other ceiling the
/// default. No cache directory is read or written — nothing is fetched.
fn offline_models_config() -> orion::config::ModelsConfig {
    orion::config::ModelsConfig {
        enabled: true,
        ..orion::config::ModelsConfig::default()
    }
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
        model_dirs: &[String],
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
            findings.extend(set.add_model_dirs(&[dir.to_string()])?);
        }
        findings.extend(set.add_plugin_dirs(plugin_dirs)?);
        findings.extend(set.add_model_dirs(model_dirs)?);
        let errors = findings.iter().filter(|f| f.is_error()).count();
        for finding in &findings {
            eprintln!("{finding}");
        }
        if errors > 0 {
            return Err(format!(
                "{errors} error(s) in the definitions under '{}'",
                dir.unwrap_or(if plugin_dirs.is_empty() {
                    "--model-dir"
                } else {
                    "--plugin-dir"
                })
            )
            .into());
        }
        Ok(Self {
            dir: dir.map(str::to_string),
            shared,
            plugins: set.plugins,
            sandbox: std::sync::OnceLock::new(),
            models: set.models,
            model_host: std::sync::OnceLock::new(),
        })
    }

    /// [`Self::load`] for the optional `--definitions`, `--plugin-dir` and
    /// `--model-dir` arguments: `None` when none was given.
    pub(crate) fn load_opt(
        dir: Option<&str>,
        plugin_dirs: &[String],
        model_dirs: &[String],
    ) -> Result<Option<Self>, Box<dyn std::error::Error>> {
        if dir.is_none() && plugin_dirs.is_empty() && model_dirs.is_empty() {
            return Ok(None);
        }
        Self::load(dir, plugin_dirs, model_dirs).map(Some)
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

    /// The `model_infer` handler for one offline engine: every model whose
    /// artifact is on disk, run for real through the same handler a node
    /// registers, over the host this catalog shares between runs. The set
    /// the handler resolves against ([`OfflineModels`]) is compiled, the
    /// first time a task asks, on Orion's expressions as an offline run
    /// builds them: no secrets, since an adapter may read none, and no
    /// budget, as an offline engine has none. The second value names the
    /// models whose manifest is here and whose artifact is not, with the
    /// manifest's origin: a run naming one of them is refused as
    /// `MODEL_ARTIFACT_UNAVAILABLE` rather than stubbed.
    ///
    /// [`OfflineModels`]: orion::model::OfflineModels
    pub(crate) fn model_handler(
        &self,
    ) -> (orion::model::ModelInferFunction, Vec<(String, String)>) {
        let mut entries = Vec::new();
        let mut unavailable = Vec::new();
        for model in &self.models {
            match model.manifest_entry() {
                Some(entry) => entries.push(entry),
                None => unavailable.push((model.manifest.name.clone(), model.origin.clone())),
            }
        }
        let config = std::sync::Arc::new(offline_models_config());
        let offline = std::sync::Arc::new(orion::model::OfflineModels::new(
            entries,
            config.clone(),
            orion::engine::kit::test_exprs(),
        ));
        let artifacts: std::sync::Arc<dyn orion::model::ArtifactSource> =
            std::sync::Arc::new(offline.artifacts());
        let host = self
            .model_host
            .get_or_init(|| {
                std::sync::Arc::new(orion::model::InferenceHost::offline(&config, artifacts))
            })
            .clone();
        let function = orion::model::ModelInferFunction {
            source: orion::model::ModelSource::Offline(offline),
            host: Some(host),
            config,
        };
        (function, unavailable)
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

/// One `model_infer` task: where it is, what it is called, and the model it
/// names when that is a literal.
pub(super) struct InferTask {
    /// The JSON path of the `model` field — what a refusal points at.
    pub(super) path: String,
    /// The task's id, or its step path when it has none.
    pub(super) id: String,
    /// `None` for a computed `model`.
    pub(super) model: Option<String>,
}

/// Every `model_infer` task in a workflow's steps, its loop's `setup` and
/// task groups included. The literal
/// ids are the engine's own [`literal_references`]; the computed ones are
/// what that walk deliberately skips, listed here because an offline run
/// has to say what it will do about them.
///
/// [`literal_references`]: orion::model::literal_references
pub(super) fn model_infer_tasks(
    tasks: &serde_json::Value,
    loop_config: Option<&serde_json::Value>,
) -> Vec<InferTask> {
    let literal = orion::model::literal_references(tasks, loop_config);
    let mut out = Vec::new();
    for (path, task) in orion::engine::walk_steps(tasks, loop_config).tasks {
        let Some(function) = task.get("function") else {
            continue;
        };
        if function.get("name").and_then(serde_json::Value::as_str)
            != Some(orion::model::handler::NAME)
        {
            continue;
        }
        let id = task
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| path.clone());
        let model = literal
            .iter()
            .find(|(task_id, _)| *task_id == id)
            .map(|(_, model)| model.clone());
        out.push(InferTask {
            path: format!("{path}.function.input.model"),
            id,
            model,
        });
    }
    out
}

/// What a single-file lint says about the models `tasks` names, given the
/// `manifests` in hand. A literal id none of them describes is an error
/// when there are manifests to check against, and an unverifiable note when
/// there are none — the same distinction the set-mode check draws with
/// `closure.model`, which is the authority when a directory is linted.
fn model_findings(
    tasks: &serde_json::Value,
    loop_config: Option<&serde_json::Value>,
    workflow: &str,
    manifests: &[orion::definitions::ModelDefinition],
) -> Vec<orion::definitions::Diagnostic> {
    let entity = format!("workflow '{workflow}'");
    let mut out = Vec::new();
    for InferTask {
        id: task, model, ..
    } in model_infer_tasks(tasks, loop_config)
    {
        match model {
            Some(model) if manifests.iter().any(|m| m.manifest.name == model) => {}
            Some(model) if manifests.is_empty() => out.push(
                orion::definitions::Diagnostic::note(
                    "model.unverifiable",
                    &entity,
                    format!(
                        "task '{task}' names model '{model}', and no manifest for it was given, \
                         so the reference cannot be checked here; a node that does not serve \
                         the model quarantines the workflow"
                    ),
                )
                .with_remedy("pass --model-dir <dir> with the model's manifest"),
            ),
            Some(model) => out.push(orion::definitions::Diagnostic::error(
                "closure.model",
                &entity,
                format!(
                    "task '{task}' names model '{model}', which none of the given manifests \
                     describes"
                ),
            )),
            None => out.push(orion::definitions::Diagnostic::note(
                "model.unverifiable",
                &entity,
                format!(
                    "task '{task}' computes its model, so the model it names is decided per \
                     message and cannot be checked here"
                ),
            )),
        }
    }
    out
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
    model_dirs: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    // `false`: a directory being authored may hold a workflow with no id yet.
    // A package must carry explicit ids because channels reference them across
    // the artifact; a directory has no such contract, and refusing an id-less
    // draft would make the gate unusable exactly when it is most wanted.
    load_and_gate(dir, boundary, false, deny_warnings, plugin_dirs, model_dirs)?;
    Ok(())
}

/// Load a definition set, compile it, and run every gate `lint <dir>` runs.
///
/// Shared with `compile <dir>`, which is `lint` plus an emitter: a compile
/// that wrote out a set its own linter would reject is how an artifact comes
/// to fail at `package apply` having passed CI. The two differ in one
/// argument — `require_ids` — and in nothing else, on purpose.
fn load_and_gate(
    dir: &str,
    boundary: orion::definitions::Boundary,
    require_ids: bool,
    deny_warnings: bool,
    plugin_dirs: &[String],
    model_dirs: &[String],
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
        model_dirs,
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
    if report.set.is_empty() && report.set.models.is_empty() && errors == 0 {
        return Err(format!(
            "no definitions found under '{dir}'. A definition is a JSON object with \
             'tasks' (workflow), 'channel_type' (channel), 'connector_type' (connector) or \
             an 'abi' of orion:model@… (model manifest)."
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
    let models = if report.set.models.is_empty() {
        String::new()
    } else {
        format!(", {} model(s)", report.set.models.len())
    };
    println!(
        "{dir}: {} connector(s), {} workflow(s), {} channel(s){models}{shared} — {errors} \
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
    /// One promotion artifact, the shape `package plan|apply|diff` consume.
    Artifact,
    /// The input tree mirrored, compiled, one file per entity.
    Dir,
    /// `connectors.json`, `workflows.json`, `channels.json` — arrays in the
    /// bulk-import body shape.
    Bulk,
    /// A revision-free v2 deployment directory: `deployment.json`,
    /// `definitions/<hex>.json` and `connections/<id>.json`, the shape
    /// `orion-cli deployments apply -d` deploys.
    Deployment,
}

/// `compile <dir>`: gate a definition set, then write it out in a form the
/// admin API accepts.
///
/// The authoring conveniences a set may use — `$from`, `use` — resolve when
/// the set is *loaded*, and the admin API loads no set. Until this command
/// existed the only path from `definitions/` to a running instance was a
/// deploy tool that reimplemented the expander, and #295 is what that costs
/// when the reimplementation is missing a case: the reference arrives as
/// literal JSON and is refused for the fields it would have supplied.
///
/// Gate first, emit second, and the gate is `lint <dir>`'s own
/// ([`load_and_gate`]) rather than a second one written beside it — a compile
/// that emitted a set its own linter rejects is how an artifact comes to fail
/// at `package apply` having passed CI.
pub(crate) struct CompileRequest<'a> {
    pub(crate) dir: &'a str,
    pub(crate) output: Option<&'a str>,
    pub(crate) format: CompileFormat,
    /// Package name and version. Required for the artifact form, meaningless
    /// for the other two, which emit no package envelope.
    pub(crate) name: Option<&'a str>,
    pub(crate) version: Option<&'a str>,
    /// With `--version content`, what stands before the hex.
    pub(crate) version_prefix: Option<&'a str>,
    /// A directory of detached signatures to write into the entries.
    pub(crate) signatures: Option<&'a str>,
    /// `--requires-orion`: the range the artifact declares, over the set's.
    pub(crate) requires_orion: Option<&'a str>,
    /// Names the set may reference without containing — the linter's boundary,
    /// and the artifact's `requires`.
    pub(crate) boundary: orion::definitions::Boundary,
    pub(crate) deny_warnings: bool,
    pub(crate) no_activate: bool,
    /// Directories of plugin manifests beyond the set's own tree.
    pub(crate) plugin_dirs: &'a [String],
    /// Directories of model manifests beyond the set's own tree.
    pub(crate) model_dirs: &'a [String],
}

pub(crate) fn run_compile(req: CompileRequest<'_>) -> Result<(), Box<dyn std::error::Error>> {
    // Only the artifact form. Its entries are addressed by id across the file —
    // `apply` activates a channel by `channel_id`, and reads activation intent
    // off it — so an id-less entity in an artifact is one `apply` would stage
    // and never activate.
    //
    // The other two forms are request bodies, and the API assigns an id from
    // the name exactly as it does for a hand-written POST. Demanding one there
    // would refuse a set that deploys correctly today: leaving `channel_id` out
    // and letting the server derive it is an ordinary way to author a set, and
    // it is what the sets that motivated this command do.
    // A deployment's members are addressed by id too: a member is its kind
    // and id, and the target matches a recompiled one to what it deploys by
    // them.
    let requires_ids = matches!(
        req.format,
        CompileFormat::Artifact | CompileFormat::Deployment
    );
    if req.format == CompileFormat::Deployment
        && !(req.boundary.channels.is_empty() && req.boundary.connectors.is_empty())
    {
        // A deployment is the whole of what the target serves, so nothing it
        // references can live outside it.
        return Err(
            "--requires-channel and --requires-connector do not apply to \
                    --format deployment: a deployment carries everything it references"
                .into(),
        );
    }
    let version = match req.format {
        CompileFormat::Artifact => match req.version {
            Some(v) => crate::package_cli::VersionSpec::parse(v, req.version_prefix)?,
            None => {
                return Err("--version is required for --format artifact (and --name, \
                            unless the set declares package.name)"
                    .into());
            }
        },
        _ => crate::package_cli::VersionSpec::Literal(""),
    };
    if let Some(name) = req.name {
        // Refused here rather than by the target at `apply`.
        orion::validation::package_key("--name", name, orion::validation::MAX_PACKAGE_NAME_LEN)?;
    }
    let requires_orion = req
        .requires_orion
        .map(|range| {
            orion::version::OrionRequirement::parse(range)
                .map(|r| r.as_str().to_string())
                .map_err(|e| format!("--requires-orion {e}"))
        })
        .transpose()?;

    let requires = req.boundary.clone();
    let (set, package) = load_and_gate(
        req.dir,
        req.boundary,
        requires_ids,
        req.deny_warnings,
        req.plugin_dirs,
        req.model_dirs,
    )?;

    // The name: the flag, else what the set declares. The range: the flag,
    // else the set's — carried into `requires.orion`, where `plan` and
    // `apply` hold the target to it.
    let declared_name = package.as_ref().and_then(|p| p.name.clone());
    let name = match (req.name, &declared_name) {
        (Some(flag), Some(declared)) if flag != declared => {
            eprintln!(
                "note: --name '{flag}' overrides package.name '{declared}' ({})",
                package
                    .as_ref()
                    .map(|p| p.origin.as_str())
                    .unwrap_or_default()
            );
            flag.to_string()
        }
        (Some(flag), _) => flag.to_string(),
        (None, Some(declared)) => declared.clone(),
        (None, None) if req.format == CompileFormat::Artifact => {
            return Err(
                "--name is required for --format artifact (or declare package.name in the set)"
                    .into(),
            );
        }
        (None, None) => String::new(),
    };
    let requires_orion =
        requires_orion.or_else(|| package.as_ref().and_then(|p| p.requires_orion.clone()));

    match req.format {
        CompileFormat::Artifact => emit_artifact(
            &set,
            req.dir,
            &name,
            version,
            requires,
            requires_orion,
            req.no_activate,
            req.signatures,
            req.output,
        ),
        CompileFormat::Dir => emit_dir(&set, req.dir, require_output(req.output, "--format dir")?),
        CompileFormat::Bulk => emit_bulk(&set, require_output(req.output, "--format bulk")?),
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

/// Both directory formats write several files, so there is nowhere for a
/// stdout default to put them.
fn require_output<'a>(
    output: Option<&'a str>,
    what: &str,
) -> Result<&'a str, Box<dyn std::error::Error>> {
    output.ok_or_else(|| format!("-o <DIR> is required for {what}").into())
}

/// Emit the compiled set as a promotion artifact.
///
/// Built through `package_cli`'s own shapes and hashed with its own
/// `artifact_content_hash`, so an artifact this command writes and one
/// `package export` writes are the same kind of document — including the
/// hash, which `plan`, `apply` and `diff` all verify before doing anything.
#[allow(clippy::too_many_arguments)]
fn emit_artifact(
    set: &orion::definitions::DefinitionSet,
    dir: &str,
    name: &str,
    version: crate::package_cli::VersionSpec<'_>,
    requires: orion::definitions::Boundary,
    requires_orion: Option<String>,
    no_activate: bool,
    signatures: Option<&str>,
    output: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    use orion::definitions::Entity;

    let collect = |kind: Entity| -> Vec<serde_json::Value> {
        set.iter(kind).map(|d| d.doc.clone()).collect()
    };
    let mut workflows = collect(Entity::Workflow);
    let mut channels = collect(Entity::Channel);

    // Carry activation intent. `package export` reads it from the stored
    // `status`; a directory has no status, so the default is that a compiled
    // definition is meant to run — a package whose entities never activate
    // applies cleanly and serves nothing. An author who wants otherwise says
    // so per entity with `"activate": false`, or for the whole set with
    // --no-activate.
    if !no_activate {
        for entity in workflows.iter_mut().chain(channels.iter_mut()) {
            if let Some(obj) = entity.as_object_mut() {
                obj.entry("activate")
                    .or_insert_with(|| serde_json::Value::Bool(true));
            }
        }
    }

    let plugins = plugin_import_entries(set, no_activate)?;
    let models = model_import_entries(set, no_activate)?;
    // The storage connectors the models' references name, unless the set
    // carries a connector of that name itself: the target fetches every
    // artifact through one, and `plan` checks it is there before anything
    // is written.
    let connectors = collect(Entity::Connector);
    let carried: Vec<&str> = connectors
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    let mut storage: Vec<String> = Vec::new();
    for entry in &models {
        if let Some(name) = entry["artifact"]["connector"].as_str()
            && !carried.contains(&name)
            && !storage.iter().any(|s| s == name)
        {
            storage.push(name.to_string());
        }
    }

    let mut artifact = crate::package_cli::PackageArtifact {
        package: crate::package_cli::PackageMeta {
            name: name.to_string(),
            version: String::new(),
            orion: env!("CARGO_PKG_VERSION").to_string(),
            content_hash: String::new(),
            exported_from: dir.to_string(),
            exported_at: chrono::Utc::now().to_rfc3339(),
        },
        requires: crate::package_cli::Requires {
            orion: requires_orion,
            channels: requires.channels,
            connectors: requires.connectors,
            plugins: Vec::new(),
            models: Vec::new(),
            storage,
        },
        plugins,
        models,
        connectors,
        workflows,
        channels,
    };
    // A build-time signer's signatures, before anything is written — an
    // orphan or malformed `.sig` stops the compile. The set still knows each
    // model's local artifact file, so its name is a candidate too.
    if let Some(dir) = signatures {
        let dir = std::path::Path::new(dir);
        let local_name = |subject: &orion::signatures::Subject| -> Vec<String> {
            if subject.kind != orion::signatures::Kind::Model {
                return Vec::new();
            }
            set.models
                .iter()
                .filter(|m| m.manifest.name == subject.id)
                .filter_map(|m| m.artifact_path.as_deref()?.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .collect()
        };
        let report = orion::package::sign::attach_signatures(
            &mut artifact,
            dir,
            &local_name,
            &orion::package::Console,
        )?;
        for (subject, outcome) in &report {
            if let orion::signatures::Outcome::Unsigned { looked_for } = outcome {
                eprintln!(
                    "note: {} '{}' is unsigned (no {} in {})",
                    subject.kind.noun(),
                    subject.id,
                    looked_for.join(" or "),
                    dir.display()
                );
            }
        }
    }

    // The version is outside the hash, so it can be derived from it: hash
    // first, name second.
    artifact.package.content_hash = crate::package_cli::artifact_content_hash(&artifact)?;
    artifact.package.version = version.resolve(&artifact.package.content_hash)?;
    version.note_what_the_hash_excludes(&artifact);

    let rendered = serde_json::to_string_pretty(&artifact)?;
    match output {
        Some(path) => {
            if let Some(parent) = std::path::Path::new(path).parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create '{}': {e}", parent.display()))?;
            }
            std::fs::write(path, rendered).map_err(|e| format!("write '{path}': {e}"))?;
            println!(
                "wrote {}@{} ({}) to {path}",
                artifact.package.name,
                artifact.package.version,
                crate::package_cli::member_counts(&artifact),
            );
        }
        None => println!("{rendered}"),
    }
    Ok(())
}

/// The set's plugins as `/plugins/import` items, each with its component
/// inlined: an artifact is what installs a plugin on a target that has never
/// seen it, so a manifest with no component beside it cannot be compiled into
/// one — that is a refusal here, not a digest-only entry the target will fail
/// to import.
fn plugin_import_entries(
    set: &orion::definitions::DefinitionSet,
    no_activate: bool,
) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error>> {
    use base64::Engine as _;
    let mut plugins = Vec::with_capacity(set.plugins.len());
    for plugin in &set.plugins {
        let Some(path) = &plugin.component_path else {
            return Err(format!(
                "plugin '{}' ({}): no component beside the manifest, and an artifact must \
                 carry the bytes — build it, or name it with `component = …`",
                plugin.manifest.name, plugin.origin
            )
            .into());
        };
        let bytes =
            std::fs::read(path).map_err(|e| format!("reading '{}': {e}", path.display()))?;
        let mut entry = serde_json::json!({
            "plugin_id": plugin.manifest.name,
            "manifest": serde_json::to_value(&plugin.manifest)?,
            "component": base64::engine::general_purpose::STANDARD.encode(&bytes),
            "digest": orion::plugin::WasmRuntime::digest(&bytes),
            "tags": [],
        });
        if !no_activate {
            entry["activate"] = serde_json::Value::Bool(true);
        }
        plugins.push(entry);
    }
    Ok(plugins)
}

/// The set's models as `/models/import` items: the manifest without its
/// local `artifact` path, and the reference a serving instance fetches the
/// bytes through — connector and key from the manifest's `reference`, the
/// digest computed from the file `artifact` names. Both halves are
/// required: an artifact has to name bytes a target can reach, and the
/// digest is what the target checks them against, so a manifest missing
/// either is a refusal here, naming what to add, rather than an item the
/// target will refuse at import or fail at admission.
fn model_import_entries(
    set: &orion::definitions::DefinitionSet,
    no_activate: bool,
) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error>> {
    let mut models = Vec::with_capacity(set.models.len());
    for model in &set.models {
        let name = &model.manifest.name;
        let Some(digest) = &model.digest else {
            return Err(format!(
                "model '{name}' ({}): no artifact beside the manifest, and an artifact must be \
                 reachable by the target — put the file beside the manifest and name it with \
                 `artifact`, so its digest can be computed",
                model.origin
            )
            .into());
        };
        let Some(reference) = &model.manifest.reference else {
            return Err(format!(
                "model '{name}' ({}): the manifest names no `reference`, and an artifact must be \
                 reachable by the target — put the bytes in the bucket and name them with \
                 `reference = {{ connector, key }}`",
                model.origin
            )
            .into());
        };
        let mut manifest = serde_json::to_value(&model.manifest)?;
        if let Some(obj) = manifest.as_object_mut() {
            // The path is where the bytes were on this machine; a served
            // row names them by connector, key and digest.
            obj.remove("artifact");
        }
        let mut entry = serde_json::json!({
            "model_id": name,
            "manifest": manifest,
            "artifact": {
                "connector": reference.connector,
                "key": reference.key,
                "digest": digest,
            },
            "tags": [],
        });
        if !no_activate {
            entry["activate"] = serde_json::Value::Bool(true);
        }
        models.push(entry);
    }
    Ok(models)
}

/// Mirror the input tree into `out`, compiled.
///
/// One file in, one file out, at the same relative path — so a diff of the two
/// trees is exactly what the compiler did, and nothing else. Shared documents
/// are consumed rather than copied: they are the compiler's input, and an
/// admin API sent one would refuse it as no entity at all. Plugin manifests
/// and their components are copied as they are: they are already what
/// `orion-cli plugins create -f` reads.
fn emit_dir(
    set: &orion::definitions::DefinitionSet,
    dir: &str,
    out: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let root = std::path::Path::new(dir);
    let out_root = std::path::Path::new(out);
    for def in &set.definitions {
        let origin = std::path::Path::new(&def.origin);
        let relative = origin.strip_prefix(root).unwrap_or(origin);
        let target = out_root.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create '{}': {e}", parent.display()))?;
        }
        std::fs::write(&target, serde_json::to_string_pretty(&def.doc)?)
            .map_err(|e| format!("write '{}': {e}", target.display()))?;
    }
    for plugin in &set.plugins {
        let origin = std::path::Path::new(&plugin.origin);
        let Ok(relative) = origin.strip_prefix(root) else {
            // A manifest from `--plugin-dir` has no place in the mirrored
            // tree; it is deployed from where it is.
            continue;
        };
        let target = out_root.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create '{}': {e}", parent.display()))?;
        }
        std::fs::copy(origin, &target).map_err(|e| format!("copy '{}': {e}", target.display()))?;
        if let (Some(component), Some(rel)) =
            (&plugin.component_path, plugin.manifest.component.as_deref())
            && let Some(parent) = target.parent()
        {
            std::fs::copy(component, parent.join(rel))
                .map_err(|e| format!("copy '{}': {e}", component.display()))?;
        }
    }
    // Model manifests likewise, with the artifact beside each when it is
    // there: they are already what a registration reads.
    for model in &set.models {
        let origin = std::path::Path::new(&model.origin);
        let Ok(relative) = origin.strip_prefix(root) else {
            continue;
        };
        let target = out_root.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create '{}': {e}", parent.display()))?;
        }
        std::fs::copy(origin, &target).map_err(|e| format!("copy '{}': {e}", target.display()))?;
        if let (Some(artifact), Some(rel)) =
            (&model.artifact_path, model.manifest.artifact.as_deref())
            && let Some(parent) = target.parent()
        {
            std::fs::copy(artifact, parent.join(rel))
                .map_err(|e| format!("copy '{}': {e}", artifact.display()))?;
        }
    }
    println!(
        "wrote {} compiled definition(s){}{} to {out}",
        set.definitions.len(),
        if set.plugins.is_empty() {
            String::new()
        } else {
            format!(" and {} plugin manifest(s)", set.plugins.len())
        },
        if set.models.is_empty() {
            String::new()
        } else {
            format!(" and {} model manifest(s)", set.models.len())
        }
    );
    Ok(())
}

/// Emit three bulk-import bodies.
///
/// Named in the order they must be sent — connectors, then the workflows that
/// reference them, then the channels that reference those — because that is
/// the only ordering in which each import's references already exist.
fn emit_bulk(
    set: &orion::definitions::DefinitionSet,
    out: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use orion::definitions::Entity;
    std::fs::create_dir_all(out).map_err(|e| format!("create '{out}': {e}"))?;
    for (kind, file) in [
        (Entity::Connector, "connectors.json"),
        (Entity::Workflow, "workflows.json"),
        (Entity::Channel, "channels.json"),
    ] {
        let entries: Vec<&serde_json::Value> = set.iter(kind).map(|d| &d.doc).collect();
        let path = std::path::Path::new(out).join(file);
        std::fs::write(&path, serde_json::to_string_pretty(&entries)?)
            .map_err(|e| format!("write '{}': {e}", path.display()))?;
        println!(
            "wrote {} {}(s) to {}",
            entries.len(),
            kind.as_str(),
            path.display()
        );
    }
    // The fourth body, only when there is one to send: `POST /plugins/import`
    // takes the same items an artifact carries, component inlined.
    if !set.plugins.is_empty() {
        let entries = plugin_import_entries(set, true)?;
        let path = std::path::Path::new(out).join("plugins.json");
        std::fs::write(&path, serde_json::to_string_pretty(&entries)?)
            .map_err(|e| format!("write '{}': {e}", path.display()))?;
        println!("wrote {} plugin(s) to {}", entries.len(), path.display());
    }
    // And the fifth: `POST /models/import` takes the manifest and the
    // reference, never bytes — sent after the connectors it fetches through.
    if !set.models.is_empty() {
        let entries = model_import_entries(set, true)?;
        let path = std::path::Path::new(out).join("models.json");
        std::fs::write(&path, serde_json::to_string_pretty(&entries)?)
            .map_err(|e| format!("write '{}': {e}", path.display()))?;
        println!("wrote {} model(s) to {}", entries.len(), path.display());
    }
    Ok(())
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
    /// Directories of model manifests beyond the set's own tree.
    pub(crate) model_dirs: &'a [String],
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
            req.model_dirs,
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
    let catalog = Catalog::load_opt(req.definitions, req.plugin_dirs, req.model_dirs)?;
    let (shared, plugins, models) = catalog
        .map(|c| (c.shared, c.plugins, c.models))
        .unwrap_or_else(|| (SharedDefinitions::default(), Vec::new(), Vec::new()));
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
    compiled.models = models;
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
