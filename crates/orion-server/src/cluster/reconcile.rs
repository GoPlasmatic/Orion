//! The cluster's one reconcile loop, and its sweeper.
//!
//! Every node runs the v2 reconciler: it re-reads the committed deployment
//! state when the head moved, builds the revision it should serve, and
//! exchanges convergence heartbeats. A circuit-breaker reset requested on one
//! node rides the `config_epoch` row's `breaker_epoch`/`breaker_key`, and the
//! same loop applies it here.

use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::server::state::AppState;

/// Register the cluster background tasks with the supervisor. Registers
/// the reconciler on single nodes too: a committed revision whose inline build
/// failed still needs to reach the serving generation without another apply.
pub fn start_cluster_tasks(state: &AppState) {
    if let Some(store) = state.cluster.shared.clone()
        && !state.tasks.is_registered("shared_state_sweeper")
    {
        // Optional: an unswept table still answers every read correctly (an
        // expired row reads as absent); it only grows.
        state.tasks.supervise(
            "shared_state_sweeper",
            crate::runtime::Criticality::Optional,
            move |shutdown| run_shared_state_sweeper(store.clone(), shutdown),
        );
    }
    start_v2_reconciler(state);
}

/// How often each node deletes the shared store's expired keys and finished
/// rate-limit windows.
const SHARED_STATE_SWEEP_SECS: u64 = 30;

/// Delete what the shared store no longer serves. Every node runs it; each
/// statement removes only rows already dead to every reader, so concurrent
/// sweeps are harmless.
async fn run_shared_state_sweeper(
    store: crate::storage::repositories::shared_state::SqlSharedState,
    mut shutdown: crate::runtime::Shutdown,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(SHARED_STATE_SWEEP_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = shutdown.signalled() => return,
        }
        if let Err(e) = store.sweep().await {
            crate::metrics::record_error("shared_state_sweep");
            tracing::warn!(error = %e, "Shared state sweep failed");
        }
    }
}

/// Register the supervised v2 state reconciliation loop (`V2-06`, `TD-18`).
///
/// Continuously hydrates `ImmutableDeploymentStore` from the state database,
/// refreshes provider/package revocations into the active `AdmissionPolicy`,
/// reconciles desired `deployment_head` revisions with retry/backoff, renews
/// the security lease, applies peers' circuit-breaker resets, and exchanges
/// `runtime_nodes` status heartbeats.
///
/// Required: a node whose reconciler is dead never learns about another
/// node's apply, so it keeps serving the revision it last built while every
/// probe stays green — the cluster silently splits.
pub fn start_v2_reconciler(state: &AppState) {
    if state.tasks.is_registered("v2_reconciler") {
        return;
    }
    let reconciler_state = state.clone();
    state.tasks.supervise(
        "v2_reconciler",
        crate::runtime::Criticality::Required,
        move |shutdown| run_v2_reconciler(reconciler_state.clone(), shutdown),
    );
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Synchronize revoked provider packages from the deployment store into the
/// runtime `AdmissionPolicy` deny-set (`CTX-09`).
pub fn sync_v2_revocations(state: &AppState) {
    let packages = state.composition.deployments.list_packages();
    let connections = state.composition.deployments.list_connections();
    for adm in state.composition.deployments.list_admissions() {
        if adm.revoked {
            state
                .composition
                .admission
                .revoke_digest(&adm.package_digest);
            for pkg in &packages {
                if pkg.package_digest == adm.package_digest {
                    state
                        .composition
                        .admission
                        .revoke_provider(&pkg.provider_id);
                }
            }
            for conn in &connections {
                if conn.provider_digest == adm.package_digest {
                    state
                        .composition
                        .admission
                        .revoke_connection(&conn.connection_id);
                }
            }
        }
    }
}

/// Execute one pass of the v2 state reconciliation loop (`V2-06`, `TD-18`).
pub async fn reconcile_v2_state_once(state: &AppState) -> bool {
    let now_ms = unix_now_ms();
    // One row says whether anything committed has changed: every apply,
    // rollback and revocation moves the head or the epoch. The whole store
    // is re-read only when it did.
    let hydrated =
        match crate::extensions::ImmutableDeploymentStore::persisted_head(&state.db_pool).await {
            Ok(persisted)
                if persisted.unwrap_or((0, 0)) == state.composition.deployments.current_head() =>
            {
                Ok(())
            }
            Ok(_) => {
                state
                    .composition
                    .deployments
                    .load_from_db_pool(&state.db_pool)
                    .await
            }
            Err(e) => Err(e),
        };
    if let Err(e) = hydrated {
        if !state.composition.reconciler.is_security_lease_valid(now_ms) {
            state.composition.admission.set_lease_valid(false);
        }
        crate::metrics::record_error("v2_reconciler");
        tracing::warn!(error = %e, "V2 reconciler: failed to hydrate deployment store from DB");
        return false;
    }

    sync_v2_revocations(state);
    crate::runtime::workflow_providers::sync(state);

    state.composition.reconciler.renew_security_lease(
        now_ms,
        crate::extensions::reconciler::DEFAULT_SECURITY_LEASE_MS,
    );
    state.composition.admission.set_lease_valid(true);

    let desired_rev = state.composition.reconciler.observe_desired_head();
    let local = state.composition.reconciler.local_status();
    let mut tick_ok = true;

    if desired_rev > 0
        && local.applied_revision != desired_rev
        && let Err(e) = crate::runtime::reconcile_deployment_revision(state, desired_rev).await
    {
        crate::metrics::record_error("v2_reconciler");
        tracing::warn!(
            error = %e,
            desired_rev,
            applied_rev = local.applied_revision,
            "V2 reconciler: failed to reconcile deployment revision; will retry"
        );
        tick_ok = false;
    }

    if state.cluster.enabled && !apply_breaker_resets(state).await {
        tick_ok = false;
    }

    if let Err(e) = state
        .composition
        .reconciler
        .persist_local_status_to_db_pool(&state.db_pool)
        .await
    {
        tracing::warn!(error = %e, "V2 reconciler: failed to persist local status heartbeat");
        tick_ok = false;
    }

    if let Err(e) = state
        .composition
        .reconciler
        .refresh_observed_nodes_from_db_pool(&state.db_pool)
        .await
    {
        tracing::warn!(error = %e, "V2 reconciler: failed to refresh observed cluster nodes");
        tick_ok = false;
    }

    tick_ok
}

async fn run_v2_reconciler(state: AppState, mut shutdown: crate::runtime::Shutdown) {
    let poll_ms = state.config.cluster.epoch_poll_interval_ms.max(10);
    let mut interval = tokio::time::interval(Duration::from_millis(poll_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await;

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = shutdown.signalled() => return,
        }
        if reconcile_v2_state_once(&state).await {
            crate::metrics::record_job_success("v2_reconciler");
        }
    }
}

/// Apply a circuit-breaker reset another node requested: when the breaker
/// epoch moved past what this node applied, reset the key it names here.
/// Answers whether the row was read.
async fn apply_breaker_resets(state: &AppState) -> bool {
    let row = match state.cluster.repo.get_epoch().await {
        Ok(row) => row,
        Err(e) => {
            crate::metrics::record_error("v2_reconciler");
            tracing::warn!(error = %e, "V2 reconciler: failed to read the breaker epoch");
            return false;
        }
    };
    let last = state
        .cluster
        .last_seen_breaker_epoch
        .load(Ordering::Acquire);
    if row.breaker_epoch > last {
        if !row.breaker_key.is_empty() {
            // A missing key on this node is fine — breakers are created
            // lazily per node (D3).
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
    true
}
