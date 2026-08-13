//! Rolling-deploy drain gate (multi-instance-ha A9).
//!
//! Before this existed the two serve paths disagreed: TLS stopped accepting
//! the instant a signal arrived (LB still routing here → connection refused),
//! while plain HTTP kept accepting for the whole drain window but never told
//! the LB anything — `/readyz` stayed 200 throughout. Both now share one
//! sequence: withdraw readiness first, keep serving while the LB reacts,
//! then stop accepting and drain in-flight work.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Resolves when the server should stop accepting new connections.
///
/// On `shutdown` firing:
/// 1. `ready` flips to `false` immediately — `/readyz` starts returning 503
///    and the load balancer pulls this node from rotation.
/// 2. Accepting and serving continues for `accept_grace` — requests the LB
///    still routes here during its own poll interval must succeed.
/// 3. The future resolves — the caller stops accepting and drains in-flight
///    requests (bounded by `server.shutdown_force_timeout_secs`).
///
/// The shutdown trigger is a generic future (not hardwired to process
/// signals) so tests can drive the sequence with a channel.
pub async fn drain_gate(
    shutdown: impl Future<Output = ()>,
    ready: Arc<AtomicBool>,
    accept_grace: Duration,
) {
    shutdown.await;
    ready.store(false, Ordering::Release);
    tracing::info!(
        grace_secs = accept_grace.as_secs(),
        "Readiness withdrawn (/readyz -> 503); still accepting during LB drain grace"
    );
    tokio::time::sleep(accept_grace).await;
    tracing::info!("Drain grace elapsed; no longer accepting new connections");
}
