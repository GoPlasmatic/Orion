use orion::config;

use super::config_cmd::{open_state_db, redacted};

/// Probe the state database: open the pool and run the migrations check.
/// Avoids the "wrong credentials surface only at first request" footgun.
pub(crate) async fn run_test_connectivity(
    config: &config::AppConfig,
    wait: Option<std::time::Duration>,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Probing storage at {} ...", redacted(&config.storage.url));
    let pool = open_state_db(config, wait)
        .await
        .map_err(|e| format!("storage: connection failed: {e}"))?;
    let pending = orion::storage::pending_migrations(&pool)
        .await
        .map_err(|e| format!("storage: pending_migrations query failed: {e}"))?;
    println!(
        "  storage:         OK ({} pending migrations)",
        pending.len()
    );
    Ok(())
}
