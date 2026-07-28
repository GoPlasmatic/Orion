//! Epoch watcher: the receive side of the config-change bus (A2/A11).
//!
//! Every admin mutation bumps the `config_epoch` row in the shared DB (the
//! send side, `bump_config_epoch`). Each node polls that row; when the epoch
//! is ahead of what this node has applied it resyncs everything from the DB
//! (connector registry + pool eviction + engine reload). Breaker resets ride
//! the same row via `breaker_epoch`/`breaker_key`.

use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::server::state::AppState;

/// Spawn cluster background tasks. Returns no handles when cluster mode is
/// disabled — the tasks simply don't exist on a single node.
pub fn start_cluster_tasks(state: &AppState) -> Vec<tokio::task::JoinHandle<()>> {
    if !state.cluster.enabled {
        return Vec::new();
    }
    vec![start_epoch_watcher(state.clone())]
}

fn start_epoch_watcher(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(
            state.config.cluster.epoch_poll_interval_ms,
        ));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Skip the immediate first tick.
        interval.tick().await;
        tracing::info!(
            poll_interval_ms = state.config.cluster.epoch_poll_interval_ms,
            "Epoch watcher started"
        );

        loop {
            interval.tick().await;
            let row = match state.cluster.repo.get_epoch().await {
                Ok(row) => row,
                Err(e) => {
                    crate::metrics::record_error("epoch_watcher");
                    tracing::warn!(error = %e, "Epoch watcher: failed to read config epoch");
                    continue;
                }
            };

            // A tick succeeds when the epoch was read and, if it had
            // advanced, the resync applied. A failed resync leaves the
            // gauge stale alongside the retry warning (O3).
            let mut tick_ok = true;
            let last = state.cluster.last_seen_epoch.load(Ordering::Acquire);
            if row.epoch > last {
                tracing::info!(
                    from = last,
                    to = row.epoch,
                    "Config epoch advanced on another node; resyncing from DB"
                );
                match crate::engine::resync_from_db(&state).await {
                    Ok(()) => {
                        state
                            .cluster
                            .last_seen_epoch
                            .fetch_max(row.epoch, Ordering::AcqRel);
                    }
                    Err(e) => {
                        // Do not advance last_seen — retry on the next tick.
                        tick_ok = false;
                        crate::metrics::record_error("epoch_watcher");
                        tracing::warn!(error = %e, "Epoch watcher: resync failed; will retry");
                    }
                }
            }

            let last_breaker = state
                .cluster
                .last_seen_breaker_epoch
                .load(Ordering::Acquire);
            if row.breaker_epoch > last_breaker {
                if !row.breaker_key.is_empty() {
                    // A missing key on this node is fine — breakers are
                    // created lazily per node (D3).
                    let found = state
                        .connector_registry
                        .reset_circuit_breaker(&row.breaker_key)
                        .await;
                    tracing::info!(
                        key = %row.breaker_key,
                        found,
                        "Applied cluster-wide circuit breaker reset"
                    );
                }
                state
                    .cluster
                    .last_seen_breaker_epoch
                    .fetch_max(row.breaker_epoch, Ordering::AcqRel);
            }

            if tick_ok {
                crate::metrics::record_job_success("epoch_watcher");
            }
        }
    })
}
