//! The offline authoring commands — `lint`, `compile`, `fmt` and `clippy` —
//! over a definition set on disk. Nothing here talks to a server: each runs
//! the checks and passes `orion-authoring` and the engine define, the same
//! ones a node holds an upload to.

mod clippy;
mod compile;
mod fmt;
mod lint;

pub use clippy::ClippyCmd;
pub(crate) use clippy::ClippyFormat;
pub use compile::CompileCmd;
pub use fmt::FmtCmd;
pub use lint::LintCmd;

/// An error from the offline tools, which predate this crate's `anyhow`:
/// its message, then each cause.
pub(crate) fn boxed(err: Box<dyn std::error::Error>) -> anyhow::Error {
    let mut message = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        message.push_str(&format!("\n  Caused by: {cause}"));
        source = cause.source();
    }
    anyhow::anyhow!(message)
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
    let empty = orion_authoring::definitions::SharedDefinitions::default();
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
    orion_authoring::definitions::compile::compile(
        &mut doc,
        &orion_authoring::definitions::Cx {
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
    shared: orion_authoring::definitions::SharedDefinitions,
    /// The plugin manifests found under the definitions directory and every
    /// `--plugin-dir`, with the component beside each when there is one.
    /// What the registry validates against, and what an offline run loads.
    pub(super) plugins: Vec<orion_authoring::definitions::PluginDefinition>,
    /// The sandbox over those components, built on first use: a `test` run
    /// over many cases compiles each component once.
    sandbox: std::sync::OnceLock<Result<OfflineSandbox, String>>,
}

/// The plugin components an offline run executes — for real, in the same
/// sandbox the server uses, with the host's default ceilings.
pub(crate) struct OfflineSandbox {
    runtime: std::sync::Arc<orion_plugin_host::WasmRuntime>,
    /// Plugin id → its compiled component. A manifest with no component
    /// beside it is absent here, and a run naming one of its functions fails
    /// as `PLUGIN_ARTIFACT_UNAVAILABLE` rather than being stubbed.
    loaded: std::collections::HashMap<String, std::sync::Arc<orion_plugin_host::LoadedComponent>>,
    config: orion_common::config::PluginsConfig,
}

impl Catalog {
    /// Load the catalog under `dir`, reporting what it found once.
    pub(crate) fn load(
        dir: Option<&str>,
        plugin_dirs: &[String],
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut findings = Vec::new();
        let mut shared = orion_authoring::definitions::SharedDefinitions::default();
        let mut set = orion_authoring::definitions::DefinitionSet::default();
        if let Some(dir) = dir {
            let (loaded, shared_findings) =
                orion_authoring::definitions::SharedDefinitions::from_directory(
                    std::path::Path::new(dir),
                )?;
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
    ) -> Result<orion_engine::FunctionRegistry, Box<dyn std::error::Error>> {
        let set = orion_authoring::definitions::DefinitionSet {
            plugins: self.plugins.clone(),
            ..orion_authoring::definitions::DefinitionSet::default()
        };
        Ok(set.function_registry()?)
    }

    /// The compiled components, built once.
    fn sandbox(&self) -> Result<&OfflineSandbox, Box<dyn std::error::Error>> {
        self.sandbox
            .get_or_init(|| {
                let config = orion_common::config::PluginsConfig {
                    enabled: true,
                    ..orion_common::config::PluginsConfig::default()
                };
                let runtime = orion_plugin_host::WasmRuntime::new(&config)
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
                    let limits = orion_plugin_host::Limits::effective(
                        &sandbox.config,
                        &plugin.manifest.name,
                    );
                    for (entry, decl) in
                        plugin.entries().into_iter().zip(&plugin.manifest.functions)
                    {
                        let handler = orion_plugin_host::PluginFunctionHandler::new(
                            std::sync::Arc::new(entry),
                            component.clone(),
                            sandbox.runtime.clone(),
                            limits,
                        );
                        handlers.push(orion_plugin_host::PluginFunction::new(
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
    pub(crate) handlers: Vec<orion_plugin_host::PluginFunction>,
    /// Function name → the manifest's origin, for every plugin whose
    /// component is not beside its manifest and so cannot run here.
    pub(crate) unavailable: Vec<(String, String)>,
}

/// The registry an offline command reads: the catalog's when there is one,
/// the built-ins otherwise.
pub(super) fn offline_registry(
    catalog: Option<&Catalog>,
) -> Result<orion_engine::FunctionRegistry, Box<dyn std::error::Error>> {
    match catalog {
        Some(catalog) => catalog.registry(),
        None => Ok(orion_engine::FunctionRegistry::builtin()
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
    registry: &orion_engine::FunctionRegistry,
    manifests: &[orion_authoring::definitions::PluginDefinition],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for task in orion_engine::leaf_tasks(tasks, loop_config) {
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
fn placeholder_entries(names: &[String]) -> Vec<orion_engine::FunctionEntry> {
    names
        .iter()
        .map(|name| orion_engine::FunctionEntry {
            name: name.clone(),
            description: "plugin function with no manifest in this run".to_string(),
            category: "plugin".to_string(),
            source: orion_engine::functions::schema::Source::Plugin,
            input_fields: None,
            writes: orion_engine::functions::schema::WriteShape::OutputPath { default_root: None },
            retry_safety: orion_engine::functions::schema::RetrySafety::Pure,
            deny_unknown: false,
            validate_static: None,
            connector: None,
            plugin: None,
        })
        .collect()
}

/// Load a definition set, compile it, and run every gate `lint <dir>` runs.
///
/// Shared with `compile <dir>`, which is `lint` plus an emitter: a compile
/// that wrote out a set its own linter would reject is how a deployment comes
/// to fail at apply having passed CI. The two differ in their boundary and in
/// `require_ids`, and in nothing else, on purpose.
/// The channel drivers `--capabilities FILE` describes: a target node's
/// `/runtime-capabilities` answer, as `orion-cli deployments capabilities -o
/// json` prints it (bare, or inside a `data` envelope). `None` without the
/// flag, which leaves the offline tools on Community's drivers.
pub(super) fn capabilities_drivers(
    path: Option<&str>,
) -> Result<Option<orion_runtime::extensions::ChannelDriverRegistry>, Box<dyn std::error::Error>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let raw = std::fs::read_to_string(path).map_err(|e| format!("--capabilities {path}: {e}"))?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("--capabilities {path}: {e}"))?;
    let answer = value
        .get("data")
        .filter(|d| d.is_object())
        .unwrap_or(&value);
    let Some(info) = answer.get("channel_driver_info") else {
        return Err(format!(
            "--capabilities {path}: names no `channel_driver_info`; write it with \
             `orion-cli deployments capabilities -o json` against a server that reports it"
        )
        .into());
    };
    let described: Vec<orion_api::v2::ChannelDriverInfo> = serde_json::from_value(info.clone())
        .map_err(|e| format!("--capabilities {path}: `channel_driver_info`: {e}"))?;
    let registry = orion_runtime::extensions::ChannelDriverRegistry::with_described(&described)
        .map_err(|protocol| {
            format!("--capabilities {path}: protocol '{protocol}' is described twice")
        })?;
    Ok(Some(registry))
}

fn load_and_gate(
    dir: &str,
    boundary: orion_authoring::definitions::Boundary,
    require_ids: bool,
    deny_warnings: bool,
    plugin_dirs: &[String],
) -> Result<
    (
        orion_authoring::definitions::DefinitionSet,
        Option<orion_authoring::definitions::PackageDecl>,
    ),
    Box<dyn std::error::Error>,
> {
    let report = orion_authoring::definitions::gate_directory(
        std::path::Path::new(dir),
        &boundary,
        orion_authoring::definitions::GateOpts {
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

    use orion_authoring::definitions::Entity;
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

/// Format an `OrionError::Validation` into a multi-line CLI message
/// listing each field detail with its path and code.
pub(super) fn format_lint_error(
    workflow_path: &str,
    err: orion_common::errors::OrionError,
) -> String {
    use orion_common::errors::OrionError;
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
