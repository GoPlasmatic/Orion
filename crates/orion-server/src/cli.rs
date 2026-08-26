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
            format!("enabled (endpoint={})", config.tracing.otlp_endpoint)
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
    let backend = orion::storage::get_backend();
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
) -> Result<(), Box<dyn std::error::Error>> {
    use orion::storage::repositories::workflows::CreateWorkflowRequest;

    // A directory is a definition set, and the checks that matter there are
    // the ones *between* files — which a per-file lint cannot see by
    // construction (#286).
    if std::path::Path::new(workflow_path).is_dir() {
        return run_lint_set(workflow_path, deny_warnings, boundary);
    }

    let catalog = Catalog::load_opt(definitions)?;
    let doc = read_expanded_workflow(workflow_path, catalog.as_ref())?;
    let req: CreateWorkflowRequest = serde_json::from_value(doc)
        .map_err(|e| format!("'{workflow_path}' is not a valid workflow JSON: {e}"))?;

    // No config here: `lint` reads a file, not a server. The default ceiling
    // is used, which can only make lint *stricter* than an instance that has
    // raised `engine.max_loop_iterations` — the safe direction for a
    // pre-flight tool (R20).
    let loop_cap = orion::config::EngineConfig::default().max_loop_iterations;
    if let Err(err) = orion::validation::validate_create_workflow(&req, loop_cap) {
        return Err(format_lint_error(workflow_path, err).into());
    }

    // Advisory findings the create path does not refuse. On stderr so stdout
    // stays the one-line verdict a script greps.
    //
    // Through `Finding`, in the same shape `check_workflows` gives the same
    // advisory in set mode: the `check` id is what lets a pipeline grandfather
    // one rule instead of reaching for `--deny-warnings` and silencing every
    // rule. Printing it as a bare string here would leave the most-used entry
    // point — one file — as the one that cannot be selected against.
    let warnings: Vec<orion::definitions::Finding> =
        orion::validation::unresolvable_logic_warnings(&req.tasks)
            .into_iter()
            .map(|(path, message)| {
                orion::definitions::Finding::warning(
                    "logic.unresolvable",
                    format!("workflow '{}' {path}", req.name),
                    message,
                )
            })
            .collect();
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

    let Some(catalog) = definitions else {
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
            catalog.dir
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
    /// The directory it came from, for the message when a reference does not
    /// resolve against it.
    dir: String,
    shared: orion::definitions::SharedDefinitions,
}

impl Catalog {
    /// Load the catalog under `dir`, reporting what it found once.
    pub(crate) fn load(dir: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let (shared, findings) =
            orion::definitions::SharedDefinitions::from_directory(std::path::Path::new(dir))?;
        let errors = findings.iter().filter(|f| f.is_error()).count();
        for finding in &findings {
            eprintln!("{finding}");
        }
        if errors > 0 {
            return Err(format!("{errors} error(s) in the definitions under '{dir}'").into());
        }
        Ok(Self {
            dir: dir.to_string(),
            shared,
        })
    }

    /// [`Self::load`] for an optional `--definitions` argument.
    pub(crate) fn load_opt(dir: Option<&str>) -> Result<Option<Self>, Box<dyn std::error::Error>> {
        dir.map(Self::load).transpose()
    }
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
) -> Result<(), Box<dyn std::error::Error>> {
    let (set, report) =
        orion::definitions::DefinitionSet::from_directory(std::path::Path::new(dir))?;

    // Say what was not read. A set lint that silently ignores a file reports
    // green over a set it did not finish reading, which is the failure this
    // command exists to remove rather than relocate.
    for (path, error) in &report.unparseable {
        eprintln!("warning: {} is not readable JSON: {error}", path.display());
    }
    for path in &report.skipped {
        eprintln!(
            "note: {} is not a channel, workflow or connector — skipped",
            path.display()
        );
    }

    if set.is_empty() {
        return Err(format!(
            "no definitions found under '{dir}'. A definition is a JSON object with \
             'tasks' (workflow), 'channel_type' (channel) or 'connector_type' (connector)."
        )
        .into());
    }

    // `false`: a directory being authored may hold a workflow with no id yet.
    // A package must carry explicit ids because channels reference them across
    // the artifact; a directory has no such contract, and refusing an id-less
    // draft would make the gate unusable exactly when it is most wanted.
    // The loader's findings — an unresolvable `$from`, a missing fragment, a
    // name defined twice — are the same class as the check pass's and share
    // its exit rules, which is the whole reason #286 came first.
    let mut findings = report.findings;
    findings.extend(orion::definitions::check(&set, &boundary, false));

    let errors = findings.iter().filter(|f| f.is_error()).count();
    // Counted by severity rather than as "everything that is not an error":
    // the report also carries inventory notes, and gating on those made
    // `--deny-warnings` fail on any set that references an environment
    // variable.
    let warnings = findings.iter().filter(|f| f.is_warning()).count();
    for finding in &findings {
        eprintln!("{finding}");
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
        set.count(Entity::Connector),
        set.count(Entity::Workflow),
        set.count(Entity::Channel),
    );

    if errors > 0 {
        return Err(format!("{errors} error(s) in '{dir}'").into());
    }
    if deny_warnings && warnings > 0 {
        return Err(format!("{warnings} warning(s) in '{dir}' and --deny-warnings is set").into());
    }
    Ok(())
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
) -> Result<OfflineRun, Box<dyn std::error::Error>> {
    let stubs = match stubs_path {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| format!("Failed to read stubs '{path}': {e}"))?;
            orion::engine::functions::stub::parse_stubs(&raw, path)?
        }
        None => orion::engine::functions::stub::StubTable::new(),
    };
    build_dry_run_engine_with_stubs(workflow_path, stubs, definitions)
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
) -> Result<OfflineRun, Box<dyn std::error::Error>> {
    use orion::storage::repositories::workflows::{CreateWorkflowRequest, workflow_to_dataflow};

    let doc = read_expanded_workflow(workflow_path, definitions)?;
    let req: CreateWorkflowRequest = serde_json::from_value(doc)
        .map_err(|e| format!("'{workflow_path}' is not a valid workflow JSON: {e}"))?;
    orion::validation::validate_create_workflow(
        &req,
        orion::config::EngineConfig::default().max_loop_iterations,
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
    let functions =
        orion::engine::functions::stub::build_stub_functions_with_log(stubs, log.clone());
    // Custom operators are registered here too: a dry-run must speak the same
    // expression vocabulary as the serving engine.
    let engine =
        orion::engine::operators::with_orion_engine_defaults(dataflow_rs::Engine::builder())
            .with_workflow(df_workflow)
            .with_handlers(functions)
            .build()?;
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
    definitions: Option<&str>,
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

    let catalog = Catalog::load_opt(definitions)?;
    let run = build_dry_run_engine(workflow_path, stubs_path, catalog.as_ref())?;
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
    use orion::storage::repositories::workflows::SqlWorkflowRepository;

    let pool = orion::storage::init_pool_no_migrate(&config.storage)
        .await
        .map_err(|e| format!("storage: connection failed: {e}"))?;

    println!("Config and environment: OK (checked while loading).");
    eprintln!("Scanning stored channels and workflows ...");

    let channels = SqlChannelRepository::new(pool.clone());
    let workflows = SqlWorkflowRepository::new(pool);
    let findings = orion::preflight::scan(&channels, &workflows).await?;

    if findings.is_empty() {
        println!("Stored channels and workflows: OK — nothing to migrate.");
        return Ok(());
    }

    println!(
        "\n{} item(s) need attention before upgrading. Numbers in brackets are \
         checklist rows at https://docs.goplasmatic.io/operate/upgrading-to-1.0.html.\n",
        findings.len()
    );
    for finding in &findings {
        println!("{finding}\n");
    }

    // Non-zero so this can gate a deploy (`orion-server preflight || exit 1`),
    // the same way `validate-config` and `lint` already do.
    Err(format!("preflight found {} item(s) to fix", findings.len()).into())
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
    // run's work on any set of size.
    let catalog = Catalog::load_opt(definitions)?;

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

    let run = match build_dry_run_engine_with_stubs(&workflow_path, stubs, definitions) {
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
