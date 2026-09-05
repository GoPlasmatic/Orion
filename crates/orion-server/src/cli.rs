//! CLI subcommand implementations — everything `orion-server <subcommand>`
//! runs instead of starting the server. `main.rs` keeps the clap definitions
//! and dispatches here.

use orion::config;

/// Output format for `validate-config`.
#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum ConfigFormat {
    /// The full effective config as TOML, secrets masked (the default).
    Toml,
    /// The full effective config as JSON, secrets masked.
    Json,
    /// A short human-readable summary of the headline settings.
    Summary,
}

/// `validate-config` subcommand: dump the effective configuration and exit.
///
/// `toml`/`json` print the *entire* config — serialized from the structs the
/// server actually runs on, so a new section can never be omitted the way the
/// old hand-maintained summary omitted `[cluster]`, `[queue]` and
/// `[tracing.storage]` (O15). The validity note goes to stderr so stdout
/// stays machine-parseable.
pub(crate) fn handle_validate_config(
    config: &config::AppConfig,
    format: ConfigFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    match format {
        ConfigFormat::Summary => print_config_summary(config),
        ConfigFormat::Toml => {
            eprintln!("Configuration is valid.");
            let masked = masked_effective_config(config)?;
            print!(
                "{}",
                toml::to_string_pretty(&toml::Value::try_from(&masked)?)?
            );
        }
        ConfigFormat::Json => {
            eprintln!("Configuration is valid.");
            let masked = masked_effective_config(config)?;
            println!("{}", serde_json::to_string_pretty(&masked)?);
        }
    }
    Ok(())
}

/// The full effective config — defaults + file + env overrides, as merged at
/// startup — with secrets masked, as a JSON tree.
///
/// The tree goes through `toml::Value` first so unset `Option` fields drop
/// out the way an authored config file omits them (TOML has no null).
/// Masking reuses the connector policy (`orion::connector::mask_secrets`):
/// values under secret-looking keys (`kafka.auth.sasl_password`,
/// `admin_auth.api_keys`) are replaced wholesale, and URL userinfo passwords
/// (`storage.url`, `cluster.redis_url`) are redacted in place.
fn masked_effective_config(
    config: &config::AppConfig,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let without_unset = toml::Value::try_from(config)?;
    let mut tree = serde_json::to_value(&without_unset)?;
    orion::connector::mask_secrets(&mut tree);
    Ok(tree)
}

/// URL-shaped values the summary and the connectivity probe print verbatim can
/// embed `user:password@` credentials or secret-named query parameters; show
/// them with both positions struck out.
fn redacted(value: &str) -> String {
    orion::connector::redact_url_secrets_or_raw(value)
}

/// The `summary` format: the handful of headline settings an operator scans
/// for on a box. Everything else is in `--format toml`/`json`.
fn print_config_summary(config: &config::AppConfig) {
    println!("Configuration is valid.\n");
    println!("  environment:     {}", config.environment);
    println!(
        "  server:          {}:{}",
        config.server.host, config.server.port
    );
    println!(
        "  tls:             {}",
        if config.server.tls.enabled {
            format!("enabled (cert={})", config.server.tls.cert_path)
        } else {
            "disabled".to_string()
        }
    );
    println!("  storage:         {}", redacted(&config.storage.url));
    println!(
        "  logging:         level={}, format={}",
        config.logging.level,
        match config.logging.format {
            config::LogFormat::Json => "json",
            config::LogFormat::Pretty => "pretty",
        }
    );
    println!(
        "  admin_auth:      {}",
        if config.admin_auth.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "  cors:            {}{}",
        config.cors.allowed_origins.join(", "),
        // Credentials change what a listed origin may do on a user's behalf,
        // so a summary that omits it understates the exposure.
        if config.cors.allow_credentials {
            " (credentials allowed)"
        } else {
            ""
        }
    );
    println!(
        "  rate_limiting:   {}",
        if config.rate_limit.enabled {
            format!(
                "enabled (rps={}, burst={})",
                config.rate_limit.default_rps, config.rate_limit.default_burst
            )
        } else {
            "disabled".to_string()
        }
    );
    println!(
        "  queue:           workers={}, buffer={}",
        config.trace_queue.workers, config.trace_queue.buffer_size
    );
    println!(
        "  metrics:         {}",
        if config.metrics.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "  tracing:         {}",
        if config.tracing.enabled {
            format!(
                "enabled (endpoint={})",
                redacted(&config.tracing.otlp_endpoint)
            )
        } else {
            "disabled".to_string()
        }
    );
    println!(
        "  cluster:         {}",
        if config.cluster.enabled {
            format!("enabled (instance_id={})", config.cluster.instance_id)
        } else {
            "disabled".to_string()
        }
    );
    println!(
        "  kafka:           {}",
        if config.kafka.enabled {
            let brokers: Vec<String> = config.kafka.brokers.iter().map(|b| redacted(b)).collect();
            format!("enabled (brokers={})", brokers.join(","))
        } else {
            "disabled".to_string()
        }
    );
}

/// `migrate [--dry-run]` subcommand: list or apply pending DB migrations.
pub(crate) async fn handle_migrate(
    config: &config::AppConfig,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let pool = orion::storage::init_pool_no_migrate(&config.storage).await?;
    let backend = pool.backend();
    let pending = orion::storage::pending_migrations(&pool).await?;

    if pending.is_empty() {
        println!("No pending migrations ({backend}).");
        return Ok(());
    }

    if dry_run {
        println!("Pending migrations on {backend} ({}):", pending.len());
    } else {
        println!("Applying {} migration(s) on {backend}...", pending.len());
    }

    // D13: name the backend. Version numbers are per-backend — `004` is
    // `cluster_coordination` on SQLite, `bigint_columns` on Postgres and
    // `active_immutability` on MySQL — so a bare number never says which
    // change is pending. The description always did; printing the backend
    // alongside makes the pair unambiguous without a shared version space,
    // and lets a runbook name migrations rather than number them.
    for (version, description) in &pending {
        println!("  {backend} {version:03} — {description}");
    }

    if dry_run {
        println!(
            "\nMigration numbers are per-backend and are not comparable across \
             sqlite/postgres/mysql. Refer to a migration by its name."
        );
    } else {
        orion::storage::run_migrations(&pool).await?;
        println!("Migrations applied successfully.");
    }
    Ok(())
}

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
    use orion::storage::repositories::workflows::CreateWorkflowRequest;

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
    let unverifiable = unverifiable_functions(&req.tasks, &registry);
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
        orion::validation::unresolvable_logic_warnings(&req.tasks, &registry)
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
        orion::validation::engine_advisories(&req.tasks, &registry)
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

    let Some(catalog) = definitions.filter(|c| c.dir.is_some()) else {
        // Without a catalog an unexpanded `use` reaches validation as a task
        // with no `name` and no `function`, and is refused for *that* — an
        // error that describes the symptom and hides the cause. Say the cause.
        if let Some(reference) = orion::definitions::first_reference(&doc) {
            return Err(format!(
                "'{path}' contains {reference}, but no --definitions directory was \
                 given to resolve it against"
            )
            .into());
        }
        return Ok(doc);
    };
    let mut findings = Vec::new();
    catalog.shared.expand(&mut doc, path, &mut findings);

    let errors = findings.iter().filter(|f| f.is_error()).count();
    for finding in &findings {
        eprintln!("{finding}");
    }
    if errors > 0 {
        // An unresolved reference cannot be run past — the document that
        // reaches the engine would be missing whatever the reference stood for.
        return Err(format!(
            "{errors} unresolved reference(s) expanding '{path}' against '{}'",
            catalog.dir.as_deref().unwrap_or_default()
        )
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
    plugins: Vec<orion::definitions::PluginDefinition>,
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
            definitions: Vec::new(),
            plugins: self.plugins.clone(),
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

    /// One handler per function of every plugin whose component is here,
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
                    for entry in plugin.entries() {
                        let name = entry.name.clone();
                        let handler = orion::plugin::PluginFunctionHandler::new(
                            std::sync::Arc::new(entry),
                            component.clone(),
                            sandbox.runtime.clone(),
                            limits,
                        );
                        handlers.push((name, Box::new(handler)));
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
    /// Function name → its handler, for every plugin whose component loaded.
    pub(crate) handlers: Vec<(String, dataflow_rs::BoxedFunctionHandler)>,
    /// Function name → the manifest's origin, for every plugin whose
    /// component is not beside its manifest and so cannot run here.
    pub(crate) unavailable: Vec<(String, String)>,
}

/// The registry an offline command reads: the catalog's when there is one,
/// the built-ins otherwise.
fn offline_registry(
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
fn unverifiable_functions(
    tasks: &serde_json::Value,
    registry: &orion::engine::FunctionRegistry,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for task in orion::engine::leaf_tasks(tasks) {
        let Some(name) = task
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        if name.contains('.') && !registry.contains(name) && !out.iter().any(|n| n == name) {
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
            aliases: Vec::new(),
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
/// that wrote out a set its own linter would reject is how an artifact comes
/// to fail at `package apply` having passed CI. The two differ in one
/// argument — `require_ids` — and in nothing else, on purpose.
fn load_and_gate(
    dir: &str,
    boundary: orion::definitions::Boundary,
    require_ids: bool,
    deny_warnings: bool,
    plugin_dirs: &[String],
) -> Result<orion::definitions::DefinitionSet, Box<dyn std::error::Error>> {
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

    if report.set.is_empty() {
        return Err(format!(
            "no definitions found under '{dir}'. A definition is a JSON object with \
             'tasks' (workflow), 'channel_type' (channel) or 'connector_type' (connector)."
        )
        .into());
    }

    let errors = report.errors();
    let warnings = report.warnings();
    for finding in &report.findings {
        eprintln!("{finding}");
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
        "{dir}: {} connector(s), {} workflow(s), {} channel(s){shared} — {errors} error(s), \
         {warnings} warning(s)",
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
    Ok(report.set)
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
    /// Names the set may reference without containing — the linter's boundary,
    /// and the artifact's `requires`.
    pub(crate) boundary: orion::definitions::Boundary,
    pub(crate) deny_warnings: bool,
    pub(crate) no_activate: bool,
    /// Directories of plugin manifests beyond the set's own tree.
    pub(crate) plugin_dirs: &'a [String],
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
    let requires_ids = req.format == CompileFormat::Artifact;
    let (name, version) = match req.format {
        CompileFormat::Artifact => match (req.name, req.version) {
            (Some(n), Some(v)) => (n, v),
            _ => return Err("--name and --version are required for --format artifact".into()),
        },
        _ => ("", ""),
    };

    let requires = orion::definitions::Boundary {
        channels: req.boundary.channels.clone(),
        connectors: req.boundary.connectors.clone(),
    };
    let set = load_and_gate(
        req.dir,
        req.boundary,
        requires_ids,
        req.deny_warnings,
        req.plugin_dirs,
    )?;

    match req.format {
        CompileFormat::Artifact => emit_artifact(
            &set,
            req.dir,
            name,
            version,
            requires,
            req.no_activate,
            req.output,
        ),
        CompileFormat::Dir => emit_dir(&set, req.dir, require_output(req.output, "--format dir")?),
        CompileFormat::Bulk => emit_bulk(&set, require_output(req.output, "--format bulk")?),
    }
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
fn emit_artifact(
    set: &orion::definitions::DefinitionSet,
    dir: &str,
    name: &str,
    version: &str,
    requires: orion::definitions::Boundary,
    no_activate: bool,
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

    let mut artifact = crate::package_cli::PackageArtifact {
        package: crate::package_cli::PackageMeta {
            name: name.to_string(),
            version: version.to_string(),
            orion: env!("CARGO_PKG_VERSION").to_string(),
            content_hash: String::new(),
            exported_from: dir.to_string(),
            exported_at: chrono::Utc::now().to_rfc3339(),
        },
        requires: crate::package_cli::Requires {
            channels: requires.channels,
            connectors: requires.connectors,
            plugins: Vec::new(),
        },
        plugins,
        connectors: collect(Entity::Connector),
        workflows,
        channels,
    };
    artifact.package.content_hash = crate::package_cli::artifact_content_hash(&artifact)?;

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
    println!(
        "wrote {} compiled definition(s){} to {out}",
        set.definitions.len(),
        if set.plugins.is_empty() {
            String::new()
        } else {
            format!(" and {} plugin manifest(s)", set.plugins.len())
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

/// `clippy <path>`: `lint`'s gate first, then every rule over the set.
///
/// Exit `1` on any error — a `lint` error or a `deny` rule — and on a
/// warning under `--deny-warnings`; `2` when the path cannot be read as a
/// set. Rules run only when `lint` is clean: a rule over a document the API
/// would refuse produces a second finding about the same mistake, and a
/// false one.
pub(crate) fn run_clippy(req: ClippyRequest<'_>) -> Result<i32, Box<dyn std::error::Error>> {
    use orion::definitions::clippy::Diagnostic;
    use orion::definitions::{DefinitionSet, Entity, SharedDefinitions};

    let path = std::path::Path::new(req.path);
    let (raw, compiled, shared, mut findings) = if path.is_dir() {
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
        for notice in report.notices() {
            eprintln!("{notice}");
        }
        if report.set.is_empty() {
            eprintln!("error: no definitions found under '{}'", req.path);
            return Ok(2);
        }
        let raw = report
            .raw
            .unwrap_or_else(|| DefinitionSet::from_entries([]));
        (raw, report.set, report.shared, report.findings)
    } else if path.is_file() {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("read '{}': {e}", req.path))?;
        let doc: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| format!("'{}' is not valid JSON: {e}", req.path))?;
        let Some(entity) = Entity::classify(&doc) else {
            eprintln!(
                "error: '{}' is not a channel, workflow or connector (no 'tasks', 'channel_type' \
                 or 'connector_type')",
                req.path
            );
            return Ok(2);
        };
        let catalog = Catalog::load_opt(req.definitions, req.plugin_dirs)?;
        let (shared, plugins) = catalog
            .map(|c| (c.shared, c.plugins))
            .unwrap_or_else(|| (SharedDefinitions::default(), Vec::new()));
        let mut findings = Vec::new();
        let mut compiled_doc = doc.clone();
        orion::definitions::compile::compile(
            &mut compiled_doc,
            &orion::definitions::Cx {
                shared: &shared,
                origin: req.path,
            },
            &mut findings,
        );
        let raw = DefinitionSet::from_entries([(entity, req.path.to_string(), doc)]);
        let mut compiled =
            DefinitionSet::from_entries([(entity, req.path.to_string(), compiled_doc)]);
        compiled.plugins = plugins;
        let registry = compiled.function_registry()?;
        findings.extend(orion::definitions::check(
            &compiled,
            &req.boundary,
            false,
            &registry,
        ));
        (raw, compiled, shared, findings)
    } else {
        eprintln!("error: '{}' is not a file or directory", req.path);
        return Ok(2);
    };

    // No conversion: `check` and the clippy rules now report the same type, so
    // a clippy run is a superset of a lint run by construction rather than by
    // an upcast that dropped the location half of every field it copied.
    let mut diagnostics: Vec<Diagnostic> = std::mem::take(&mut findings);
    let lint_errors = diagnostics.iter().filter(|d| d.is_error()).count();
    let mut skipped: Vec<&str> = Vec::new();
    if lint_errors == 0 {
        // The same registry the gate read: the set's own manifests join the
        // built-ins, so a plugin function's template fields are analysed
        // exactly as the server would evaluate them.
        let registry = compiled.function_registry()?;
        let analysis = orion::definitions::analysis::Analysis::new(
            &raw, &compiled, &shared, req.config, &registry,
        );
        let report = orion::definitions::clippy::run(&analysis);
        diagnostics.extend(report.diagnostics);
        skipped = report.skipped;
    }

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
                eprintln!("note: [{rule}] skipped — needs the serving config (-c <config.toml>)");
            }
            if lint_errors > 0 {
                println!(
                    "{}: {lint_errors} lint error(s) — fix those first; clippy's rules did not run",
                    req.path
                );
            } else {
                println!(
                    "{}: {} workflow(s), {} channel(s), {} connector(s) — {errors} error(s), \
                     {warnings} warning(s) from {} rule(s)",
                    req.path,
                    compiled.count(Entity::Workflow),
                    compiled.count(Entity::Channel),
                    compiled.count(Entity::Connector),
                    orion::definitions::clippy::registry().len() - skipped.len()
                );
            }
        }
    }

    Ok(if errors > 0 || (req.deny_warnings && warnings > 0) {
        1
    } else {
        0
    })
}

/// Print the OpenAPI spec to stdout. Backs the checked-in `docs/openapi.json`
/// (regenerate with `orion-server dump-openapi > docs/openapi.json`) and needs
/// neither config, a database, nor a running server.
pub(crate) fn run_dump_openapi() -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", orion::server::routes::openapi::pretty_json());
    Ok(())
}

/// Format an `OrionError::Validation` into a multi-line CLI message
/// listing each field detail with its path and code.
fn format_lint_error(workflow_path: &str, err: orion::errors::OrionError) -> String {
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

/// Build a dry-run engine over one workflow file, with connector-backed
/// functions answered from `stubs_path` instead of from real backends.
///
/// Shared by [`run_dry_run`] and the `test` runner, which differ only in what
/// they do with the result.
pub(crate) fn build_dry_run_engine(
    workflow_path: &str,
    stubs_path: Option<&str>,
    definitions: Option<&Catalog>,
    secrets: &orion::engine::ResolvedSecrets,
) -> Result<OfflineRun, Box<dyn std::error::Error>> {
    let stubs = match stubs_path {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| format!("Failed to read stubs '{path}': {e}"))?;
            orion::engine::functions::stub::parse_stubs(&raw, path)?
        }
        None => orion::engine::functions::stub::StubTable::new(),
    };
    build_dry_run_engine_with_stubs(workflow_path, stubs, definitions, secrets)
}

/// Read a `--secrets` / case-file secrets document into a store.
///
/// The values are **stand-ins**, used verbatim: an offline run has no config
/// file and no reason to reach a real vault, and a suite that depends on the
/// machine's environment is a suite that passes on one laptop. Use throwaway
/// values, and keep the file out of the repository if any of them is not.
pub(crate) fn offline_secrets(
    value: &serde_json::Value,
    source: &str,
) -> Result<orion::engine::ResolvedSecrets, String> {
    match value {
        serde_json::Value::Object(map) => {
            // Checked here rather than left to the first task that reads one.
            // `ResolvedSecrets::resolve` always produces strings, so this is
            // the only store that can hold a shape the reader rejects — and
            // that rejection arrives as a task error inside a `*.case.json`
            // run, where the case name is the only context the author gets.
            for (name, value) in map {
                if !value.is_string() {
                    return Err(format!(
                        "{source}: secrets.{name} must be a string, got {}",
                        orion::engine::utils::json_kind(value)
                    ));
                }
            }
            Ok(orion::engine::ResolvedSecrets::from_values(map.clone()))
        }
        other => Err(format!(
            "{source}: secrets must be a JSON object of name -> value, got {}",
            orion::engine::utils::json_kind(other)
        )),
    }
}

/// Everything an offline run needs beyond the engine itself.
pub(crate) struct OfflineRun {
    pub engine: dataflow_rs::Engine,
    /// The connector calls the run makes, filled in as it runs — each one
    /// already labelled with the task that made it, since dataflow-rs 3.7
    /// carries the task id into the handler.
    pub log: std::sync::Arc<orion::engine::functions::stub::CallLog>,
}

/// [`build_dry_run_engine`] over an already-parsed stub table, for the `test`
/// runner — whose stubs may be inline in the case file rather than on disk.
pub(crate) fn build_dry_run_engine_with_stubs(
    workflow_path: &str,
    stubs: orion::engine::functions::stub::StubTable,
    definitions: Option<&Catalog>,
    secrets: &orion::engine::ResolvedSecrets,
) -> Result<OfflineRun, Box<dyn std::error::Error>> {
    use orion::storage::repositories::workflows::{CreateWorkflowRequest, workflow_to_dataflow};

    let doc = read_expanded_workflow(workflow_path, definitions)?;
    let req: CreateWorkflowRequest = serde_json::from_value(doc)
        .map_err(|e| format!("'{workflow_path}' is not a valid workflow JSON: {e}"))?;
    // Validated against the catalog's manifests, so a plugin task's input is
    // checked here as the server checks it. A plugin function no manifest
    // accounts for cannot run offline at all — there is nothing to load —
    // and is refused by name rather than reaching the engine as unknown.
    let registry = offline_registry(definitions)?;
    let unverifiable = unverifiable_functions(&req.tasks, &registry);
    if let Some(name) = unverifiable.first() {
        return Err(format!(
            "PLUGIN_ARTIFACT_UNAVAILABLE: '{workflow_path}' names plugin function '{name}', \
             and no manifest for its plugin was given — an offline run executes plugin \
             functions for real, so pass --plugin-dir <dir> holding the plugin's plugin.toml \
             and its component"
        )
        .into());
    }
    orion::validation::validate_create_workflow(
        &req,
        orion::config::EngineConfig::default().max_loop_iterations,
        &registry,
    )
    .map_err(|e| format_lint_error(workflow_path, e))?;

    // Build a synthetic Workflow row from the request to reuse the
    // existing dataflow conversion. Version + timestamps are placeholders.
    let synthetic = orion::storage::repositories::workflows::synthetic_workflow(
        &req,
        req.workflow_id.as_deref().unwrap_or("dry-run"),
    )?;
    let df_workflow = workflow_to_dataflow(&synthetic, "__dry_run__")?;

    // Stub handlers are registered for every connector-backed function, even
    // with no stub file: an unstubbed call then reports which stub to add,
    // rather than the `FunctionNotFound` an empty map used to give.
    let log = std::sync::Arc::new(orion::engine::functions::stub::CallLog::new());
    let mut functions =
        orion::engine::functions::stub::build_stub_functions_with_log(stubs, log.clone());
    // Plugin functions are never stubbed: a plugin is capability-free, so the
    // real component runs, exactly as `crypto` does. A manifest whose
    // component is not beside it has functions this run cannot execute, and
    // a workflow naming one fails here by name rather than as a missing
    // handler at build.
    if let Some(catalog) = definitions {
        let OfflinePluginHandlers {
            handlers,
            unavailable,
        } = catalog.plugin_handlers()?;
        for task in orion::engine::leaf_tasks(&req.tasks) {
            let Some(name) = task
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            if let Some((_, origin)) = unavailable.iter().find(|(f, _)| f == name) {
                return Err(format!(
                    "PLUGIN_ARTIFACT_UNAVAILABLE: '{workflow_path}' names plugin function \
                     '{name}', whose manifest ({origin}) has no component beside it — build \
                     the component, or name it with `component = …`, so the run can execute \
                     it rather than stub it"
                )
                .into());
            }
        }
        for (name, handler) in handlers {
            functions.insert(name, handler);
        }
    }
    // `build_single` registers the custom operators — a dry run must speak the
    // same expression vocabulary as the serving engine — and screens the
    // workflow against the handlers above, which is what makes a dry run's
    // verdict mean something. Screened against the *stub* table on purpose:
    // the question a dry run answers is "will this run offline", and the stub
    // table is the handler set it will run against.
    let engine = orion::engine::build_single(df_workflow, functions, secrets)?;
    Ok(OfflineRun { engine, log })
}

/// Dry-run a workflow against an input JSON file.
///
/// Connector-backed tasks are answered from the stub file when one is supplied
/// (see [`orion::engine::functions::stub`]); without one, any such task fails
/// naming the stub that would satisfy it. Nothing reaches a real backend
/// either way — this is the offline counterpart to
/// `POST /workflows/{id}/test`, which runs against live connectors.
pub(crate) async fn run_dry_run(
    workflow_path: &str,
    input_path: &str,
    stubs_path: Option<&str>,
    metadata_path: Option<&str>,
    secrets_path: Option<&str>,
    definitions: Option<&str>,
    plugin_dirs: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let input_raw = std::fs::read_to_string(input_path)
        .map_err(|e| format!("Failed to read input '{input_path}': {e}"))?;
    let input: serde_json::Value = serde_json::from_str(&input_raw)
        .map_err(|e| format!("'{input_path}' is not valid JSON: {e}"))?;

    let metadata = match metadata_path {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| format!("Failed to read metadata '{path}': {e}"))?;
            serde_json::from_str(&raw)
                .map_err(|e| format!("'{path}' is not valid JSON: {e}"))
                .and_then(|v| {
                    orion::engine::utils::prepare_offline_metadata(v)
                        .map_err(|e| format!("'{path}': {e}"))
                })?
        }
        None => serde_json::json!({}),
    };

    let secrets = match secrets_path {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| format!("Failed to read secrets '{path}': {e}"))?;
            let value: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|e| format!("'{path}' is not valid JSON: {e}"))?;
            offline_secrets(&value, path)?
        }
        None => orion::engine::ResolvedSecrets::empty(),
    };

    let catalog = Catalog::load_opt(definitions, plugin_dirs)?;
    let run = build_dry_run_engine(workflow_path, stubs_path, catalog.as_ref(), &secrets)?;
    let mut message = dataflow_rs::Message::builder()
        .payload_json(&input)
        .metadata_json(&metadata)
        .build();

    // Own the trace so a hard failure still prints the steps that ran. A dry
    // run that dies on task three and reports nothing is the least useful
    // possible answer to "what does this workflow do?".
    let mut trace = dataflow_rs::ExecutionTrace::new();
    let run_error = run
        .engine
        .process_message_tracing(&mut message, &mut trace)
        .await
        .err();

    // `output` is the data document under its historical name: CI `jq` filters
    // read it. The run's documents go in beside it under the names a case's
    // `expect` roots use, from the same builder the runner reads, so a path
    // lifted off a dry run addresses the same thing in a case.
    let mut output = serde_json::json!({
        "matched": !trace.steps.is_empty(),
        "trace": trace,
        "output": message.data(),
        "errors": message.errors().iter().filter_map(|e| serde_json::to_value(e).ok()).collect::<Vec<_>>(),
    });
    for (name, document) in orion::engine::functions::stub::run_documents(&message, &run.log) {
        output[name] = document;
    }
    if let Some(ref e) = run_error {
        output["error"] = serde_json::json!(e.to_string());
    }
    println!("{}", serde_json::to_string_pretty(&output)?);

    // The trace is on stdout either way; the exit status still reports the
    // failure, so `orion-server dry-run` stays usable as a CI gate.
    match run_error {
        Some(e) => Err(orion::errors::OrionError::Engine(e).into()),
        None => Ok(()),
    }
}

/// Scan the stored estate for anything the 1.0 rules refuse (`preflight`).
///
/// Deliberately opens the pool *without* migrating: the point is to report on
/// the database as it stands, before the upgrade, and migrating first would be
/// a side effect no one asked a read-only check for.
///
/// Config-file and `ORION_*` problems never reach this function — `load_config`
/// refuses unknown keys and retired variable names before any subcommand
/// dispatches — so getting this far is itself the result for that surface, and
/// the header says so rather than leaving it unstated.
pub(crate) async fn run_preflight(
    config: &config::AppConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    use orion::storage::repositories::channels::SqlChannelRepository;
    use orion::storage::repositories::plugins::SqlPluginRepository;
    use orion::storage::repositories::workflows::SqlWorkflowRepository;

    let pool = orion::storage::init_pool_no_migrate(&config.storage)
        .await
        .map_err(|e| format!("storage: connection failed: {e}"))?;

    println!("Config and environment: OK (checked while loading).");
    eprintln!("Scanning stored channels and workflows ...");

    let channels = SqlChannelRepository::new(pool.clone());
    let workflows = SqlWorkflowRepository::new(pool.clone());
    // The active plugins' functions are part of the vocabulary the stored
    // workflows were written against; scanned without them, every plugin
    // task reads as a break.
    let plugins = SqlPluginRepository::new(pool);
    let findings = orion::preflight::scan_with(&channels, &workflows, &plugins).await?;

    // Split by severity, not merged into one count. A break is a row the
    // upgrade refuses; an advisory is a shape that keeps serving whatever the
    // operator does about it, and dataflow-rs 3.10 made two of those reachable
    // from a scan. Reported together they would be indistinguishable — the
    // three-line form prints no level, by design — and counted together they
    // would fail the gate below over a workflow that is fine.
    let (breaks, advisories): (Vec<_>, Vec<_>) = findings
        .into_iter()
        .partition(orion::definitions::Diagnostic::is_error);

    if breaks.is_empty() {
        println!("Stored channels and workflows: OK — nothing to migrate.");
    } else {
        println!(
            "\n{} item(s) need attention before upgrading. Numbers in brackets are \
             checklist rows at https://docs.goplasmatic.io/operate/upgrading-to-1.0.html.\n",
            breaks.len()
        );
        for finding in &breaks {
            // The three-line form, not `Display`: a preflight report is read
            // alongside the upgrade guide, and every line of this section is a
            // break, so the severity `Display` prefixes would be the same word
            // on every line.
            println!("{}\n", finding.render_preflight());
        }
    }

    if !advisories.is_empty() {
        // Its own heading, and the heading is what says these do not block —
        // it is the only thing that can, since the lines themselves carry no
        // level. Printed after the breaks so the section that gates the
        // upgrade is the one an operator reads first.
        println!(
            "\n{} advisory finding(s). None of these blocks the upgrade or the exit code — \
             each names a workflow that serves correctly and says less than its author \
             meant it to. `orion-server lint` reports the same ids.\n",
            advisories.len()
        );
        for finding in &advisories {
            println!("{}\n", finding.render_preflight());
        }
    }

    if breaks.is_empty() {
        return Ok(());
    }
    // Non-zero so this can gate a deploy (`orion-server preflight || exit 1`),
    // the same way `validate-config` and `lint` already do.
    Err(format!("preflight found {} item(s) to fix", breaks.len()).into())
}

/// Probe the configured backends: open the database pool and run the
/// migrations check, and (if Kafka is enabled) fetch cluster metadata from
/// the configured brokers with the configured auth. Avoids the "wrong
/// credentials surface only at first request" footgun.
pub(crate) async fn run_test_connectivity(
    config: &config::AppConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Probing storage at {} ...", redacted(&config.storage.url));
    let pool = orion::storage::init_pool_no_migrate(&config.storage)
        .await
        .map_err(|e| format!("storage: connection failed: {e}"))?;
    let pending = orion::storage::pending_migrations(&pool)
        .await
        .map_err(|e| format!("storage: pending_migrations query failed: {e}"))?;
    println!(
        "  storage:         OK ({} pending migrations)",
        pending.len()
    );
    if config.kafka.enabled {
        let broker_list: Vec<String> = config.kafka.brokers.iter().map(|b| redacted(b)).collect();
        eprintln!("Probing Kafka brokers {} ...", broker_list.join(","));
        let kafka_config = config.kafka.clone();
        let brokers = tokio::task::spawn_blocking(move || {
            orion::kafka::probe_brokers(&kafka_config, std::time::Duration::from_secs(5))
        })
        .await
        .map_err(|e| format!("kafka: probe task failed: {e}"))?
        .map_err(|e| format!("kafka: {e}"))?;
        println!("  kafka:           OK ({brokers} brokers visible)");
    } else {
        println!("  kafka:           disabled");
    }
    Ok(())
}

// ============================================================
// `test` — a regression suite for a team's own workflows
// ============================================================

/// One test case, as authored in a `*.json` file.
///
/// `workflow` and `stubs_file` are paths resolved **relative to the case
/// file**, so a suite directory is movable and a case can be read without
/// knowing where it will be run from.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TestCase {
    /// Human name, printed in the report. Defaults to the file stem.
    #[serde(default)]
    name: Option<String>,
    /// Path to the workflow JSON under test.
    workflow: String,
    /// The message payload.
    input: serde_json::Value,
    /// The message metadata, as the HTTP ingress would have built it:
    /// `headers`, `params`, `query`, `cookies`, `auth.claims`, `channel`,
    /// `http_method`, plus any caller-supplied keys.
    ///
    /// Normalized by [`orion::engine::utils::prepare_offline_metadata`] so an
    /// offline pass means the same thing as a production pass — header keys
    /// lowercased, credential headers masked.
    #[serde(default)]
    metadata: serde_json::Value,
    /// Inline connector stubs, in the same shape as `dry-run --stubs`.
    #[serde(default)]
    stubs: Option<serde_json::Value>,
    /// Stand-in values for the `{"secret": "name"}` references the workflow
    /// reads, in the same shape as `dry-run --secrets`.
    ///
    /// An offline run has no `[secrets]` config to resolve, and an engine
    /// built with no store refuses a workflow that names one — so a workflow
    /// that signs anything is untestable without this. The values are used
    /// verbatim; use throwaway ones.
    #[serde(default)]
    secrets: Option<serde_json::Value>,
    /// Path to a stub file, as an alternative to inline `stubs`.
    #[serde(default)]
    stubs_file: Option<String>,
    /// Dotted paths to their expected values, each **rooted** at one of
    /// [`RUN_DOCUMENTS`](orion::engine::functions::stub::RUN_DOCUMENTS):
    /// `data.order.flagged`, `metadata.headers.deviceid`,
    /// `temp_data.user_id`, `calls.mongo_write[0].input.document.id`,
    /// `audit_trail[1].status`.
    ///
    /// The root is required. Every other path in Orion — a `{"var": ..}` node,
    /// a `map` mapping's `path`, a connector filter — resolves over the same
    /// `{data, metadata, temp_data}` context, and every mapping path in every
    /// shipped workflow spells its root. A bare path was accepted here alone,
    /// and silently meant `data.`, so `metadata.foo` read the data document's
    /// own `metadata` key and reported absent.
    #[serde(default)]
    expect: std::collections::BTreeMap<String, serde_json::Value>,
    /// Expected task-error codes, in order. An empty vec (the default) asserts
    /// the run produced none — which is why it is checked even when the case
    /// does not mention it.
    #[serde(default)]
    expect_errors: Vec<String>,
    /// Expected connector calls per function, in execution order.
    ///
    /// Each entry is matched as a **deep subset** of the recorded call's
    /// resolved `input`, so a case names the fields it cares about and ignores
    /// the rest — but the number of entries must equal the number of recorded
    /// calls for that function, so an unexpected extra write fails. Only the
    /// functions named here are constrained; `"publish_kafka": []` asserts that
    /// nothing was published.
    ///
    /// Unlike `expect`, presence is **strict**: a key in the expected object
    /// must be present in the record, so `"revokedAt": null` asserts *written
    /// as null* rather than *absent*. `expect` reads a data document, where
    /// JSONLogic makes a missing path and a null indistinguishable; a recorded
    /// payload is a literal document, where whether the field was written is
    /// the assertion.
    #[serde(default)]
    expect_calls: std::collections::BTreeMap<String, Vec<serde_json::Value>>,
    /// The ids of the tasks that ran, in order, matched exactly.
    ///
    /// `None` (the default) means unchecked. Unlike `expect_errors`, this
    /// cannot default to empty-and-checked: every workflow runs tasks, so that
    /// default would fail every case ever written.
    #[serde(default)]
    expect_tasks: Option<Vec<String>>,
}

/// What one case did.
struct CaseResult {
    name: String,
    failures: Vec<String>,
}

/// `test` subcommand: run every case under `path` and report.
pub(crate) async fn run_test(
    path: &str,
    definitions: Option<&str>,
    plugin_dirs: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let cases = collect_case_files(path)?;
    if cases.is_empty() {
        return Err(format!(
            "no test cases found under '{path}' (looking for *{CASE_SUFFIX}). \
             Name a case file explicitly to run one that does not follow the convention."
        )
        .into());
    }

    // Loaded once for the whole suite rather than per case: the catalog is
    // the same for all of them, and walking the tree per case was most of a
    // run's work on any set of size — and the plugin components it carries
    // are compiled once for every case that runs them.
    let catalog = Catalog::load_opt(definitions, plugin_dirs)?;

    let mut results = Vec::new();
    for case_path in &cases {
        results.push(run_case(case_path, catalog.as_ref()).await);
    }

    let failed: Vec<&CaseResult> = results.iter().filter(|r| !r.failures.is_empty()).collect();
    for result in &results {
        if result.failures.is_empty() {
            println!("  ok    {}", result.name);
        } else {
            println!("  FAIL  {}", result.name);
            for failure in &result.failures {
                println!("          {failure}");
            }
        }
    }
    println!(
        "\n{} passed, {} failed ({} case(s))",
        results.len() - failed.len(),
        failed.len(),
        results.len()
    );

    if failed.is_empty() {
        Ok(())
    } else {
        // Non-zero so a suite gates a deploy, the same way `lint`,
        // `validate-config` and `preflight` already do.
        Err(format!("{} test case(s) failed", failed.len()).into())
    }
}

/// Suffix that marks a file as a test case when scanning a directory.
pub(crate) const CASE_SUFFIX: &str = ".case.json";

/// Every `*.case.json` under `path`, or `path` itself when it names a file.
///
/// The suffix exists because a suite directory is the natural home for the
/// workflows and fixtures the cases reference. Treating every `*.json` as a
/// case reports the workflow under test as a broken case — noise that grows
/// with the size of the suite. Naming a file explicitly bypasses the
/// convention, since that is already unambiguous.
///
/// Sorted, so a report reads the same on every machine — a suite whose order
/// depends on directory iteration is a suite whose diffs are noise.
fn collect_case_files(path: &str) -> Result<Vec<std::path::PathBuf>, Box<dyn std::error::Error>> {
    let p = std::path::Path::new(path);
    if p.is_file() {
        return Ok(vec![p.to_path_buf()]);
    }
    if !p.is_dir() {
        return Err(format!("'{path}' is neither a file nor a directory").into());
    }
    let mut out: Vec<std::path::PathBuf> = std::fs::read_dir(p)
        .map_err(|e| format!("Failed to read '{path}': {e}"))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(CASE_SUFFIX))
        })
        .collect();
    out.sort();
    Ok(out)
}

/// Run one case file. Every failure is collected rather than returned early, so
/// one run reports everything wrong with a case instead of its first problem.
async fn run_case(case_path: &std::path::Path, definitions: Option<&Catalog>) -> CaseResult {
    let display = case_path.display().to_string();
    // `file_stem` on `orders.case.json` gives `orders.case`; strip the whole
    // convention suffix so the default name reads as the case, not the file.
    let stem = case_path
        .file_name()
        .and_then(|s| s.to_str())
        .map(|n| n.strip_suffix(CASE_SUFFIX).unwrap_or(n).to_string())
        .unwrap_or_else(|| display.clone());

    let fail = |name: &str, message: String| CaseResult {
        name: name.to_string(),
        failures: vec![message],
    };

    let raw = match std::fs::read_to_string(case_path) {
        Ok(raw) => raw,
        Err(e) => return fail(&stem, format!("cannot read case: {e}")),
    };
    let case: TestCase = match serde_json::from_str(&raw) {
        Ok(case) => case,
        Err(e) => return fail(&stem, format!("not a valid test case: {e}")),
    };
    let name = case.name.clone().unwrap_or(stem);

    // Relative to the case file, so a suite directory can be moved or invoked
    // from anywhere.
    let base = case_path.parent().unwrap_or(std::path::Path::new("."));
    let workflow_path = base.join(&case.workflow);
    let workflow_path = workflow_path.to_string_lossy().to_string();

    // Inline `stubs` and `stubs_file` are the same thing written two ways;
    // inline wins so a case can override a shared file. Inline stubs are already
    // a parsed value, so they go straight to the validator rather than through a
    // serialize-and-reparse.
    let stubs = match (&case.stubs, &case.stubs_file) {
        (Some(inline), _) => orion::engine::functions::stub::parse_stub_value(inline, "stubs"),
        (None, Some(file)) => match std::fs::read_to_string(base.join(file)) {
            Ok(raw) => orion::engine::functions::stub::parse_stubs(&raw, file),
            Err(e) => Err(format!("cannot read stubs '{file}': {e}")),
        },
        (None, None) => Ok(orion::engine::functions::stub::StubTable::new()),
    };
    let stubs = match stubs {
        Ok(stubs) => stubs,
        Err(e) => return fail(&name, e),
    };

    // Rooting is checked before the workflow runs: a path naming no root can
    // never match anything, and saying so after a full run — as an `<absent>`
    // diff — buries the one fact the author needs.
    let unrooted: Vec<String> = case
        .expect
        .keys()
        .filter(|path| !orion::engine::functions::stub::is_rooted(path))
        .map(|path| unrooted_message(path))
        .collect();
    if !unrooted.is_empty() {
        return CaseResult {
            name,
            failures: unrooted,
        };
    }

    let metadata = match orion::engine::utils::prepare_offline_metadata(case.metadata.clone()) {
        Ok(metadata) => metadata,
        Err(e) => return fail(&name, e),
    };

    let secrets = match case.secrets.as_ref() {
        Some(value) => match offline_secrets(value, &name) {
            Ok(secrets) => secrets,
            Err(e) => return fail(&name, e),
        },
        None => orion::engine::ResolvedSecrets::empty(),
    };

    let run = match build_dry_run_engine_with_stubs(&workflow_path, stubs, definitions, &secrets) {
        Ok(run) => run,
        Err(e) => return fail(&name, e.to_string()),
    };

    let mut message = dataflow_rs::Message::builder()
        .payload_json(&case.input)
        .metadata_json(&metadata)
        .build();
    let mut trace = dataflow_rs::ExecutionTrace::new();
    let run_error = run
        .engine
        .process_message_tracing(&mut message, &mut trace)
        .await
        .err();

    let mut failures = Vec::new();
    if let Some(e) = run_error {
        failures.push(format!("workflow failed: {e}"));
    }

    let roots = serde_json::Value::Object(orion::engine::functions::stub::run_documents(
        &message, &run.log,
    ));
    for (path, expected) in &case.expect {
        let actual = lookup_path(&roots, path);
        // An expected `null` matches an absent path as well as an explicit
        // null. JSONLogic resolves a missing `var` to null, so that is already
        // what the workflow sees; making a case distinguish the two would be a
        // distinction the runtime does not draw. (`expect_calls` is strict
        // instead — see its doc comment for why the two differ.)
        let matched = match actual {
            None => expected.is_null(),
            Some(ref actual) => actual == expected,
        };
        if !matched {
            // The diff is the whole value of the runner: "expected X, got Y at
            // this path" is what a bare pass/fail makes you go and find.
            failures.push(format!(
                "{path}: expected {expected}, got {}",
                actual.map_or("<absent>".to_string(), |v| v.to_string())
            ));
        }
    }

    failures.extend(check_expected_calls(&case.expect_calls, &run.log));

    if let Some(ref expected) = case.expect_tasks {
        let actual = executed_task_ids(&trace);
        if &actual != expected {
            failures.push(format!("tasks: expected {expected:?}, ran {actual:?}"));
        }
    }

    let actual_errors: Vec<String> = message
        .errors()
        .iter()
        .map(|e| e.code.to_string())
        .collect();
    if actual_errors != case.expect_errors {
        failures.push(format!(
            "task errors: expected {:?}, got {:?}",
            case.expect_errors, actual_errors
        ));
    }

    CaseResult { name, failures }
}

/// The ids of the tasks that executed, in step order.
///
/// Read from the execution trace rather than the audit trail: a
/// `TaskOutcome::Skip` returns before the audit entry is pushed, so the trail
/// cannot tell "skipped by condition" from "not in the workflow" — and which
/// branch ran is the whole question `expect_tasks` exists to answer.
fn executed_task_ids(trace: &dataflow_rs::ExecutionTrace) -> Vec<String> {
    trace
        .steps
        .iter()
        .filter(|step| matches!(step.result, dataflow_rs::StepResult::Executed))
        .filter_map(|step| step.task_id.clone())
        .collect()
}

/// The failure text for an `expect` path with no root, suggesting the fix.
///
/// `data.` is suggested because it is what an unrooted path used to mean, and
/// so is what almost every one of them intends.
fn unrooted_message(path: &str) -> String {
    format!(
        "expect path '{path}' has no root — did you mean 'data.{path}'? \
         roots: {}",
        orion::engine::functions::stub::RUN_DOCUMENTS.join(", ")
    )
}

/// One segment of an `expect` path.
enum Segment<'a> {
    Key(&'a str),
    Index(usize),
}

/// Split a dotted path into segments, accepting both `calls.http_call[0].input`
/// and `calls.http_call.0.input` for an array position.
fn path_segments(path: &str) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    for part in path.split('.') {
        let (head, mut rest) = match part.find('[') {
            Some(i) => (&part[..i], &part[i..]),
            None => (part, ""),
        };
        if let Ok(index) = head.parse::<usize>() {
            out.push(Segment::Index(index));
        } else if !head.is_empty() {
            out.push(Segment::Key(head));
        }
        // Trailing `[n]` groups, however many.
        while let Some(close) = rest.find(']') {
            if let Ok(index) = rest[1..close].parse::<usize>() {
                out.push(Segment::Index(index));
            }
            rest = &rest[close + 1..];
        }
    }
    out
}

/// Read a rooted, optionally array-indexed path out of the run's documents.
fn lookup_path(roots: &serde_json::Value, path: &str) -> Option<serde_json::Value> {
    path_segments(path)
        .into_iter()
        .try_fold(roots, |acc, segment| match segment {
            Segment::Key(key) => acc.get(key),
            Segment::Index(i) => acc.get(i),
        })
        .cloned()
}

/// Check a case's `expect_calls` against what the run recorded.
fn check_expected_calls(
    expected: &std::collections::BTreeMap<String, Vec<serde_json::Value>>,
    log: &orion::engine::functions::stub::CallLog,
) -> Vec<String> {
    if expected.is_empty() {
        return Vec::new();
    }
    let recorded = log.calls();
    let mut failures = Vec::new();
    for (function, expected_calls) in expected {
        let actual: Vec<&orion::engine::functions::stub::RecordedCall> = recorded
            .iter()
            .filter(|call| call.function == function.as_str())
            .collect();
        if actual.len() != expected_calls.len() {
            failures.push(format!(
                "calls.{function}: expected {} call(s), recorded {}",
                expected_calls.len(),
                actual.len()
            ));
            continue;
        }
        for (i, want) in expected_calls.iter().enumerate() {
            failures.extend(subset_mismatch(
                want,
                &actual[i].input,
                &format!("calls.{function}[{i}].input"),
            ));
        }
    }
    failures
}

/// Every way `actual` fails to contain `expected`, as diff lines rooted at
/// `path`.
///
/// Objects match as subsets — a key `expected` does not mention is not
/// checked — but a key it *does* mention must be **present**, which is what
/// makes `"revokedAt": null` assert that the field was written. Arrays match
/// element for element, including length: "these two documents were inserted"
/// is an assertion a subset rule could not express.
fn subset_mismatch(
    expected: &serde_json::Value,
    actual: &serde_json::Value,
    path: &str,
) -> Vec<String> {
    match (expected, actual) {
        (serde_json::Value::Object(want), serde_json::Value::Object(got)) => want
            .iter()
            .flat_map(|(key, want_value)| match got.get(key) {
                Some(got_value) => subset_mismatch(want_value, got_value, &format!("{path}.{key}")),
                None => vec![format!("{path}.{key}: expected {want_value}, not written")],
            })
            .collect(),
        (serde_json::Value::Array(want), serde_json::Value::Array(got))
            if want.len() == got.len() =>
        {
            want.iter()
                .zip(got)
                .enumerate()
                .flat_map(|(i, (want_value, got_value))| {
                    subset_mismatch(want_value, got_value, &format!("{path}[{i}]"))
                })
                .collect()
        }
        _ if expected == actual => Vec::new(),
        _ => vec![format!("{path}: expected {expected}, got {actual}")],
    }
}
