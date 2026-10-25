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

    // D13: name the backend. Version numbers are per-backend — each
    // directory numbers its own changes after the shared `001` baseline — so
    // a bare number never says which change is pending. The description always did; printing the backend
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
