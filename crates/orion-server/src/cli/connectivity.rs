use orion::config;

use super::config_cmd::{open_state_db, redacted};

/// Probe the configured backends: open the database pool and run the
/// migrations check, and (if Kafka is enabled) fetch cluster metadata from
/// the configured brokers with the configured auth. Avoids the "wrong
/// credentials surface only at first request" footgun.
pub(crate) async fn run_test_connectivity(
    config: &config::AppConfig,
    wait: Option<std::time::Duration>,
) -> Result<(), Box<dyn std::error::Error>> {
    // One deadline for the whole command, not one per dependency: the flag
    // answers "block until Orion's dependencies are up".
    let started = std::time::Instant::now();
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
    if config.kafka.enabled {
        let broker_list: Vec<String> = config.kafka.brokers.iter().map(|b| redacted(b)).collect();
        eprintln!("Probing Kafka brokers {} ...", broker_list.join(","));
        let deadline = wait.map(|w| started + w);
        let mut failures = 0u32;
        let brokers = loop {
            let kafka_config = config.kafka.clone();
            let probe = tokio::task::spawn_blocking(move || {
                orion::kafka::probe_brokers(&kafka_config, std::time::Duration::from_secs(5))
            })
            .await
            .map_err(|e| format!("kafka: probe task failed: {e}"))?;
            match probe {
                Ok(brokers) => break brokers,
                Err(e) => {
                    failures += 1;
                    let backoff = std::time::Duration::from_millis(250u64 << failures.min(5))
                        .min(std::time::Duration::from_secs(5));
                    match deadline {
                        Some(deadline) if std::time::Instant::now() + backoff <= deadline => {
                            eprintln!(
                                "waiting for Kafka ({e}) … {}s",
                                (started.elapsed() + backoff).as_secs()
                            );
                            tokio::time::sleep(backoff).await;
                        }
                        Some(_) => {
                            return Err(format!(
                                "kafka: not reachable after {}s: {e}",
                                started.elapsed().as_secs()
                            )
                            .into());
                        }
                        None => return Err(format!("kafka: {e}").into()),
                    }
                }
            }
        };
        println!("  kafka:           OK ({brokers} brokers visible)");
    } else {
        println!("  kafka:           disabled");
    }
    Ok(())
}
