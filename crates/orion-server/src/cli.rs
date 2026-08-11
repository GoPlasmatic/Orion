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
        "  cors:            {}",
        config.cors.allowed_origins.join(", ")
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
pub(crate) fn run_lint(workflow_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    use orion::storage::repositories::workflows::CreateWorkflowRequest;

    let raw = std::fs::read_to_string(workflow_path)
        .map_err(|e| format!("Failed to read '{workflow_path}': {e}"))?;
    let req: CreateWorkflowRequest = serde_json::from_str(&raw)
        .map_err(|e| format!("'{workflow_path}' is not a valid workflow JSON: {e}"))?;

    // No config here: `lint` reads a file, not a server. The default ceiling
    // is used, which can only make lint *stricter* than an instance that has
    // raised `engine.max_loop_iterations` — the safe direction for a
    // pre-flight tool (R20).
    let loop_cap = orion::config::EngineConfig::default().max_loop_iterations;
    if let Err(err) = orion::validation::validate_create_workflow(&req, loop_cap) {
        return Err(format_lint_error(workflow_path, err).into());
    }

    println!("'{workflow_path}' is valid.");
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
) -> Result<dataflow_rs::Engine, Box<dyn std::error::Error>> {
    let stubs = match stubs_path {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| format!("Failed to read stubs '{path}': {e}"))?;
            orion::engine::functions::stub::parse_stubs(&raw, path)?
        }
        None => orion::engine::functions::stub::StubTable::new(),
    };
    build_dry_run_engine_with_stubs(workflow_path, stubs)
}

/// [`build_dry_run_engine`] over an already-parsed stub table, for the `test`
/// runner — whose stubs may be inline in the case file rather than on disk.
pub(crate) fn build_dry_run_engine_with_stubs(
    workflow_path: &str,
    stubs: orion::engine::functions::stub::StubTable,
) -> Result<dataflow_rs::Engine, Box<dyn std::error::Error>> {
    use orion::storage::repositories::workflows::{CreateWorkflowRequest, workflow_to_dataflow};

    let raw = std::fs::read_to_string(workflow_path)
        .map_err(|e| format!("Failed to read '{workflow_path}': {e}"))?;
    let req: CreateWorkflowRequest = serde_json::from_str(&raw)
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
    let functions = orion::engine::functions::stub::build_stub_functions(stubs);
    Ok(dataflow_rs::Engine::new(vec![df_workflow], functions)?)
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
) -> Result<(), Box<dyn std::error::Error>> {
    let input_raw = std::fs::read_to_string(input_path)
        .map_err(|e| format!("Failed to read input '{input_path}': {e}"))?;
    let input: serde_json::Value = serde_json::from_str(&input_raw)
        .map_err(|e| format!("'{input_path}' is not valid JSON: {e}"))?;

    let engine = build_dry_run_engine(workflow_path, stubs_path)?;
    let mut message = dataflow_rs::Message::from_value(&input);

    // Own the trace so a hard failure still prints the steps that ran. A dry
    // run that dies on task three and reports nothing is the least useful
    // possible answer to "what does this workflow do?".
    let mut trace = dataflow_rs::ExecutionTrace::new();
    let run_error = engine
        .process_message_tracing(&mut message, &mut trace)
        .await
        .err();

    let mut output = serde_json::json!({
        "matched": !trace.steps.is_empty(),
        "trace": trace,
        "output": message.data(),
        "errors": message.errors().iter().filter_map(|e| serde_json::to_value(e).ok()).collect::<Vec<_>>(),
    });
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
         checklist rows in docs/src/getting-started/upgrading.md.\n",
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
    /// Inline connector stubs, in the same shape as `dry-run --stubs`.
    #[serde(default)]
    stubs: Option<serde_json::Value>,
    /// Path to a stub file, as an alternative to inline `stubs`.
    #[serde(default)]
    stubs_file: Option<String>,
    /// Dotted output paths to their expected values. `data.order.flagged`
    /// reads the `order.flagged` field of the workflow's data document; the
    /// leading `data.` is optional, matching `body_path` and the mapping paths
    /// authors already write.
    #[serde(default)]
    expect: std::collections::BTreeMap<String, serde_json::Value>,
    /// Expected task-error codes, in order. An empty vec (the default) asserts
    /// the run produced none — which is why it is checked even when the case
    /// does not mention it.
    #[serde(default)]
    expect_errors: Vec<String>,
}

/// What one case did.
struct CaseResult {
    name: String,
    failures: Vec<String>,
}

/// `test` subcommand: run every case under `path` and report.
pub(crate) async fn run_test(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let cases = collect_case_files(path)?;
    if cases.is_empty() {
        return Err(format!(
            "no test cases found under '{path}' (looking for *{CASE_SUFFIX}). \
             Name a case file explicitly to run one that does not follow the convention."
        )
        .into());
    }

    let mut results = Vec::new();
    for case_path in &cases {
        results.push(run_case(case_path).await);
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
async fn run_case(case_path: &std::path::Path) -> CaseResult {
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

    let engine = match build_dry_run_engine_with_stubs(&workflow_path, stubs) {
        Ok(engine) => engine,
        Err(e) => return fail(&name, e.to_string()),
    };

    let mut message = dataflow_rs::Message::from_value(&case.input);
    let mut trace = dataflow_rs::ExecutionTrace::new();
    let run_error = engine
        .process_message_tracing(&mut message, &mut trace)
        .await
        .err();

    let mut failures = Vec::new();
    if let Some(e) = run_error {
        failures.push(format!("workflow failed: {e}"));
    }

    let output: serde_json::Value = message.data().into();
    for (path, expected) in &case.expect {
        let actual = lookup_output(&output, path);
        // An expected `null` matches an absent path as well as an explicit
        // null. JSONLogic resolves a missing `var` to null, so that is already
        // what the workflow sees; making a case distinguish the two would be a
        // distinction the runtime does not draw.
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

/// Read a dotted path out of the workflow's data document.
///
/// A leading `data.` is optional: `message.data()` *is* the data document, so
/// `data.order.flagged` and `order.flagged` name the same field. Accepting both
/// matches the mapping paths authors already write.
fn lookup_output(output: &serde_json::Value, path: &str) -> Option<serde_json::Value> {
    let path = path.strip_prefix("data.").unwrap_or(path);
    path.split('.')
        .try_fold(output, |acc, segment| acc.get(segment))
        .cloned()
}
