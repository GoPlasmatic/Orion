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
    orion::connector::redact_url_secrets(value).unwrap_or_else(|| value.to_string())
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

    if let Err(err) = orion::validation::validate_create_workflow(&req) {
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

/// Dry-run a workflow against an input JSON file. Builds a minimal
/// in-process dataflow engine with the supplied workflow and no
/// custom functions — i.e. only built-in `map`/`log`/`filter`/etc.
/// work. Connector-backed tasks will fail with a clear error.
pub(crate) async fn run_dry_run(
    workflow_path: &str,
    input_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use orion::storage::repositories::workflows::{CreateWorkflowRequest, workflow_to_dataflow};

    let raw = std::fs::read_to_string(workflow_path)
        .map_err(|e| format!("Failed to read '{workflow_path}': {e}"))?;
    let req: CreateWorkflowRequest = serde_json::from_str(&raw)
        .map_err(|e| format!("'{workflow_path}' is not a valid workflow JSON: {e}"))?;
    orion::validation::validate_create_workflow(&req)
        .map_err(|e| format_lint_error(workflow_path, e))?;

    let input_raw = std::fs::read_to_string(input_path)
        .map_err(|e| format!("Failed to read input '{input_path}': {e}"))?;
    let input: serde_json::Value = serde_json::from_str(&input_raw)
        .map_err(|e| format!("'{input_path}' is not valid JSON: {e}"))?;

    // Build a synthetic Workflow row from the request to reuse the
    // existing dataflow conversion. Version + timestamps are placeholders.
    let synthetic = orion::storage::repositories::workflows::synthetic_workflow(
        &req,
        req.workflow_id.as_deref().unwrap_or("dry-run"),
    )?;
    let df_workflow = workflow_to_dataflow(&synthetic, "__dry_run__")?;
    let engine = dataflow_rs::Engine::new(vec![df_workflow], std::collections::HashMap::new())?;
    let mut message = dataflow_rs::Message::from_value(&input);

    let trace = engine
        .process_message_with_trace(&mut message)
        .await
        .map_err(orion::errors::OrionError::Engine)?;

    let output = serde_json::json!({
        "matched": !trace.steps.is_empty(),
        "trace": trace,
        "output": message.data(),
        "errors": message.errors().iter().filter_map(|e| serde_json::to_value(e).ok()).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
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
