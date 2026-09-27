//! The rate-limiter prune task: the sweep that keeps governor's in-process
//! keyed stores from growing without bound.
//!
//! A keyed GCRA limiter keeps one entry per client identity it has seen and
//! only shrinks when asked. The identity is the caller's address — or, behind
//! a trusted proxy, whatever a forwarded header resolves to, and per-channel
//! `rate_limit.key_logic` can key on a header value outright — so the key
//! space is effectively caller-chosen. Per-channel limiters are carried across
//! reloads by `Arc`, and the platform limiters live for the whole process, so
//! nothing else ever prompts the store to shrink. Left unpruned, an attacker
//! rotating source identities grows the maps until the process is restarted.
//!
//! This runs `retain_recent` + `shrink_to_fit` on every live limiter — the
//! per-channel ones on the current generation's estate and the platform ones —
//! on a fixed cadence. It is `Optional`: a node that has stopped pruning still
//! serves correctly, it only stops reclaiming idle limiter entries.

use std::time::Duration;

use crate::runtime::{Criticality, TaskRegistry};
use crate::server::state::AppState;

pub const TASK_NAME: &str = "rate_limiter_prune";

/// How often to sweep. Limiter entries are tiny, so a minute is frequent
/// enough to keep the maps proportional to *recently* active identities
/// without the sweep itself costing anything measurable.
const PRUNE_INTERVAL: Duration = Duration::from_secs(60);

/// Start the periodic limiter prune under the supervisor.
pub fn start(tasks: &TaskRegistry, state: AppState) {
    tasks.supervise(TASK_NAME, Criticality::Optional, move |mut shutdown| {
        let state = state.clone();
        async move {
            loop {
                if !shutdown.sleep(PRUNE_INTERVAL).await {
                    return;
                }
                prune_once(&state);
            }
        }
    });
}

/// One sweep over every live limiter: the current generation's per-channel
/// stores and the platform-level stores.
fn prune_once(state: &AppState) {
    state.runtime.load().channels.prune_rate_limiters();
    if let Some(rate_limit_state) = &state.rate_limit_state {
        rate_limit_state.prune();
    }
}
