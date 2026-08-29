//! Epoch watcher: the receive side of the config-change bus (A2/A11).
//!
//! Every admin mutation bumps the `config_epoch` row in the shared DB (the
//! send side, `bump_config_epoch`). Each node polls that row; when the epoch
//! is ahead of what this node has applied it resyncs from the DB — the engine
//! and channel registry always, the connector registry and the cached pools
//! when the advance does not prove otherwise (`EpochScope`). Breaker resets
//! ride the same row via `breaker_epoch`/`breaker_key`.

use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::server::state::AppState;

/// Register the cluster background tasks with the supervisor. Registers
/// nothing when cluster mode is disabled — the tasks simply don't exist on a
/// single node.
pub fn start_cluster_tasks(state: &AppState) {
    if !state.cluster.enabled {
        return;
    }
    let watcher_state = state.clone();
    // Required: a node whose watcher is dead never learns about another
    // node's mutation, so it keeps serving the configuration it booted with
    // while every probe stays green — the cluster silently splits.
    state.tasks.supervise(
        "epoch_watcher",
        crate::runtime::Criticality::Required,
        move |shutdown| run_epoch_watcher(watcher_state.clone(), shutdown),
    );
}

async fn run_epoch_watcher(state: AppState, mut shutdown: crate::runtime::Shutdown) {
    {
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
            tokio::select! {
                _ = interval.tick() => {}
                _ = shutdown.signalled() => return,
            }
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
            //
            // The scope the resync is sized to is decided in there rather than
            // here, because it is a property of the *advance* — this node's
            // watermark and the row together — not of the row alone.
            let tick_ok = advance_config_epoch(&state.cluster.last_seen_epoch, &row, |scope| {
                crate::runtime::resync_from_db(&state, scope)
            })
            .await;

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
    }
}

/// Apply a freshly-read config epoch: run `resync` if the epoch is ahead of
/// this node's watermark, and advance the watermark **only on success**.
/// Returns whether the tick counts as successful.
///
/// The scope handed to `resync` is decided here, from the watermark and the
/// row together ([`EpochScope::for_advance`](crate::cluster::EpochScope::for_advance)): the row carries one
/// scope, so it describes the whole advance only when the advance is the one
/// bump that wrote it. A node several epochs behind — every node, whenever an
/// operator makes two changes inside one poll interval — is applying bumps
/// whose scopes overwrote each other, and must resync wide.
///
/// Extracted from the polling loop (T15) so the advance/retry contract is
/// testable without a database: the watermark refusing to advance past a
/// failed resync is the property that makes the watcher self-healing — the
/// next tick sees the same gap and retries, instead of recording a config it
/// never applied.
async fn advance_config_epoch<F, Fut>(
    last_seen: &std::sync::atomic::AtomicI64,
    row: &crate::storage::repositories::cluster::EpochRow,
    resync: F,
) -> bool
where
    F: FnOnce(crate::cluster::EpochScope) -> Fut,
    Fut: std::future::Future<Output = Result<(), crate::errors::OrionError>>,
{
    let last = last_seen.load(Ordering::Acquire);
    if row.epoch <= last {
        return true;
    }
    let scope = crate::cluster::EpochScope::for_advance(
        last,
        row.epoch,
        row.epoch_scope_at,
        &row.epoch_scope,
    );
    tracing::info!(
        from = last,
        to = row.epoch,
        scope = scope.as_str(),
        "Config epoch advanced on another node; resyncing from DB"
    );
    match resync(scope).await {
        Ok(()) => {
            last_seen.fetch_max(row.epoch, Ordering::AcqRel);
            true
        }
        Err(e) => {
            // Do not advance last_seen — retry on the next tick.
            crate::metrics::record_error("epoch_watcher");
            tracing::warn!(error = %e, "Epoch watcher: resync failed; will retry");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::advance_config_epoch;
    use crate::cluster::EpochScope;
    use crate::storage::repositories::cluster::EpochRow;
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

    /// An epoch row as a scope-aware bump leaves it: the scope stamped with
    /// the epoch it was written for.
    fn row(epoch: i64, scope: &str) -> EpochRow {
        EpochRow {
            epoch,
            breaker_epoch: 0,
            breaker_key: String::new(),
            epoch_scope: scope.to_string(),
            epoch_scope_at: epoch,
        }
    }

    /// The self-healing property: a failed resync must leave the watermark
    /// where it was, so the next tick sees the same gap and retries.
    #[tokio::test]
    async fn a_failed_resync_does_not_advance_the_watermark() {
        let last_seen = AtomicI64::new(5);
        let ok = advance_config_epoch(&last_seen, &row(7, "definitions"), |_| async {
            Err(crate::errors::OrionError::internal("resync exploded"))
        })
        .await;
        assert!(!ok, "a failed resync is a failed tick");
        assert_eq!(
            last_seen.load(Ordering::Acquire),
            5,
            "the watermark must not record a config that was never applied"
        );

        // The retry succeeding is what advances it.
        let ok =
            advance_config_epoch(&last_seen, &row(7, "definitions"), |_| async { Ok(()) }).await;
        assert!(ok);
        assert_eq!(last_seen.load(Ordering::Acquire), 7);
    }

    /// An epoch at or behind the watermark must not trigger a resync at all —
    /// resync rebuilds the engine and evicts pools, so a spurious one is not
    /// harmless.
    #[tokio::test]
    async fn an_unchanged_epoch_never_resyncs() {
        let last_seen = AtomicI64::new(7);
        let ran = AtomicBool::new(false);
        let ok = advance_config_epoch(&last_seen, &row(7, "definitions"), |_| {
            ran.store(true, Ordering::SeqCst);
            async { Ok(()) }
        })
        .await;
        assert!(ok, "nothing to do is a successful tick");
        assert!(!ran.load(Ordering::SeqCst), "resync must not have run");
        assert_eq!(last_seen.load(Ordering::Acquire), 7);
    }

    /// One bump behind: the row's scope describes exactly the change being
    /// applied, so the resync is the size of it. This is the whole point of
    /// the scope — without this case every tick costs a reconnect storm.
    #[tokio::test]
    async fn a_single_step_advance_uses_the_row_scope() {
        let last_seen = AtomicI64::new(6);
        let mut applied = None;
        advance_config_epoch(&last_seen, &row(7, "definitions"), |scope| {
            applied = Some(scope);
            async { Ok(()) }
        })
        .await;
        assert_eq!(applied, Some(EpochScope::Definitions));
    }

    /// More than one bump behind: the scopes of the bumps in between were
    /// overwritten — the row keeps only the last — so the advance must be the
    /// widest resync.
    ///
    /// This is not a rare race. Creating a connector and activating the
    /// workflow that uses it is three bumps, and an operator issues them back
    /// to back: every peer sees one advance carrying the last bump's
    /// `definitions`. Sized to that, a peer rebuilds its engine and never
    /// reloads its connector registry, and every request through the new
    /// channel fails on a connector the node has never heard of.
    #[tokio::test]
    async fn an_advance_over_several_bumps_resyncs_everything() {
        let last_seen = AtomicI64::new(0);
        let mut applied = None;
        advance_config_epoch(&last_seen, &row(3, "definitions"), |scope| {
            applied = Some(scope);
            async { Ok(()) }
        })
        .await;
        assert_eq!(
            applied,
            Some(EpochScope::All),
            "a scope that describes one of three bumps cannot size the resync"
        );
        assert_eq!(last_seen.load(Ordering::Acquire), 3);
    }
}
