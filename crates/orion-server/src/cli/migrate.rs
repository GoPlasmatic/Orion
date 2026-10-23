use orion::config;

use super::config_cmd::open_state_db;

/// `migrate [--dry-run] [--wait]` subcommand: list or apply pending DB
/// migrations.
pub(crate) async fn handle_migrate(
    config: &config::AppConfig,
    dry_run: bool,
    wait: Option<std::time::Duration>,
) -> Result<(), Box<dyn std::error::Error>> {
    let pool = open_state_db(config, wait).await?;
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

/// Print the OpenAPI spec to stdout. Backs the checked-in `docs/openapi.json`
/// (regenerate with `orion-server dump-openapi > docs/openapi.json`) and needs
/// neither config, a database, nor a running server.
pub(crate) fn run_dump_openapi() -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", orion::server::routes::openapi::pretty_json());
    Ok(())
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
