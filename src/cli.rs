//! CLI subcommand implementations — everything `orion-server <subcommand>`
//! runs instead of starting the server. `main.rs` keeps the clap definitions
//! and dispatches here.

use orion::config;

/// `validate-config` subcommand: report parsed configuration and exit.
pub(crate) fn handle_validate_config(
    config: &config::AppConfig,
) -> Result<(), Box<dyn std::error::Error>> {
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
    println!("  storage:         {}", config.storage.url);
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
        config.queue.workers, config.queue.buffer_size
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
        "  kafka:           {}",
        if config.kafka.enabled {
            format!("enabled (brokers={})", config.kafka.brokers.join(","))
        } else {
            "disabled".to_string()
        }
    );
    Ok(())
}

/// `migrate [--dry-run]` subcommand: list or apply pending DB migrations.
pub(crate) async fn handle_migrate(
    config: &config::AppConfig,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let pool = orion::storage::init_pool_no_migrate(&config.storage).await?;
    let pending = orion::storage::pending_migrations(&pool).await?;

    if pending.is_empty() {
        println!("No pending migrations.");
        return Ok(());
    }

    if dry_run {
        println!("Pending migrations ({}):", pending.len());
        for (version, description) in &pending {
            println!("  {version} — {description}");
        }
    } else {
        println!("Applying {} migration(s)...", pending.len());
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
    eprintln!("Probing storage at {} ...", config.storage.url);
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
        eprintln!(
            "Probing Kafka brokers {} ...",
            config.kafka.brokers.join(",")
        );
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
