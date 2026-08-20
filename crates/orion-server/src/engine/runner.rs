//! Running the engine for one channel, and the instrumented lock accessors
//! every caller goes through to reach it.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;

use super::profile;

/// The live engine, swapped wholesale on reload.
///
/// Was `Arc<RwLock<Arc<Engine>>>`. The outer lock never protected a mutation —
/// reload builds the replacement engine entirely outside it and the critical
/// section was a single assignment — so all it ever did was serialise readers
/// against a writer that had nothing left to do. Every data-plane request paid
/// a futures-aware acquire, and a reload could still block readers in the
/// window between the timeout and the store.
///
/// `ArcSwap` is the shape the access pattern always had: many readers taking a
/// snapshot, one writer publishing a finished value. Readers never block and
/// never wait, so the reload no longer needs a timeout to bound how long it
/// might hold them off, and a reader that is mid-request keeps the engine it
/// started with until it drops the `Arc`. N17 made the same change to the
/// channel-registry snapshot for the same reason.
///
/// Reloads are still serialised, by the `reload_lock` in `AppState` — that is
/// a separate concern (two concurrent reloads would each build from a possibly
/// stale read) and is not what this type is for.
pub struct EngineHandle(ArcSwap<dataflow_rs::Engine>);

impl EngineHandle {
    pub fn new(engine: Arc<dataflow_rs::Engine>) -> Self {
        Self(ArcSwap::new(engine))
    }

    /// A snapshot of the current engine. Wait-free; the returned `Arc` stays
    /// valid across a concurrent [`Self::store`].
    pub fn load(&self) -> Arc<dataflow_rs::Engine> {
        self.0.load_full()
    }

    /// Publish a new engine. Readers already holding a snapshot finish against
    /// the old one; every load after this returns the new one.
    pub fn store(&self, engine: Arc<dataflow_rs::Engine>) {
        self.0.store(engine);
    }
}

/// Result of one engine invocation: the engine's own result plus the captured
/// per-task `ExecutionTrace` when the caller opted in (A2).
pub type EngineCallResult = (dataflow_rs::Result<()>, Option<dataflow_rs::ExecutionTrace>);

/// Opt in to per-task trace capture, bounded by the same byte budget the
/// persisted row is capped at.
///
/// A `bool` before: the engine took no capture policy, so the only defence
/// against an oversized trace was throwing the finished one away.
#[derive(Debug, Clone, Copy)]
pub struct TraceCapture {
    /// Approximate in-memory snapshot budget. Pass
    /// `queue.max_result_size_bytes`: it is the exact limit the serialized row
    /// is checked against afterwards, so using it here makes the post-hoc cap a
    /// backstop that should essentially never fire. `0` is unbounded, matching
    /// the result cap's own semantics.
    pub max_snapshot_bytes: usize,
}

/// Run the engine for `channel` with optional timeout, optional per-task
/// trace capture, and optional profiling scope. The sync HTTP, async trace
/// queue, Kafka ingress and in-process `channel_call` paths all go through
/// here so timeout and trace semantics cannot drift between them. `Err(ms)`
/// means the call timed out after `ms` milliseconds.
pub async fn run_for_channel(
    engine: &Arc<dataflow_rs::Engine>,
    channel: &str,
    message: &mut dataflow_rs::Message,
    timeout_ms: Option<u64>,
    profile: Option<&Arc<profile::ProfileCollector>>,
    capture: Option<TraceCapture>,
) -> Result<EngineCallResult, u64> {
    let run = run_for_channel_inner(engine, channel, message, timeout_ms, capture);
    if let Some(p) = profile {
        profile::ORION_PROFILE.scope(p.clone(), run).await
    } else {
        run.await
    }
}

/// Await `fut` under an optional deadline, reporting the elapsed budget as the
/// error so the caller can name it in a 504.
///
/// F46: the timeout arm used to be written out once per (capture_trace ×
/// timeout) combination — four branches for two independent choices, where the
/// timed and untimed halves of each pair could drift apart silently. It is one
/// decision, made here.
async fn with_deadline<F>(timeout_ms: Option<u64>, fut: F) -> Result<F::Output, u64>
where
    F: std::future::Future,
{
    match timeout_ms {
        Some(ms) => tokio::time::timeout(Duration::from_millis(ms), fut)
            .await
            .map_err(|_| ms),
        None => Ok(fut.await),
    }
}

/// What a `task_details` run records per executed step.
///
/// Under the default policy a step deep-clones the whole `Message` — context,
/// payload **and** the accumulated audit trail — so trace size is unbounded in
/// message size and quadratic in task count: a 6-task workflow over a ~1 MB
/// context serialized to ~12 MB. `serialize_task_trace_capped` is the exact
/// cap, but it runs strictly afterwards, by which point the clones and the
/// serialization are already paid. This bounds it at capture time, which is the
/// only place memory can be bounded.
///
/// - `snapshot_audit_trail: Own` — each snapshot carries only the entry its own
///   task produced. `Full` accumulates `N*(N+1)/2` entries across a trace and is
///   the term that makes the growth quadratic. Orion reads `Message::audit_trail`
///   nowhere.
/// - `changes` — the per-task diff, which is what `task_details` is *for*
///   ("inspect intermediate inputs/outputs for each task"). Correctly attributed
///   on a `Skip`, unlike reading `audit_trail.last()`.
/// - `redact_paths: ["metadata.headers", "metadata.cookies"]` — a pruning
///   clone, so neither map is cloned into a step in the first place.
///   `context.metadata` is stripped from `result_json` on read (S14) but
///   `task_trace_json` was returned verbatim, and every step inside it held a
///   full `Message` clone carrying the same headers. Only four header names are
///   masked at ingress, so everything else was readable through that hole.
///   Forward-only: rows already on disk still need the read-side strip.
///
///   This is a **path list, not a metadata-wide prune**, so a new metadata key
///   is covered only by being named here — which is why `metadata.cookies`
///   (#270) had to be added when the cookie allowlist landed, or an
///   allowlisted value would be cloned into every step snapshot and persisted
///   for any channel with `tracing.task_details = true`.
///
///   Note what this defends: the row **at rest**. The trace read already
///   strips `context.metadata` whole-message and per-step, so a missing entry
///   here does not surface through the API — which is exactly why it needs
///   stating rather than testing end-to-end.
fn trace_options(max_snapshot_bytes: usize) -> dataflow_rs::TraceOptions {
    dataflow_rs::TraceOptions {
        changes: true,
        snapshot_audit_trail: dataflow_rs::AuditTrailScope::Own,
        max_snapshot_bytes,
        redact_paths: vec![
            "metadata.headers".to_string(),
            "metadata.cookies".to_string(),
        ],
        ..Default::default()
    }
}

async fn run_for_channel_inner(
    engine: &Arc<dataflow_rs::Engine>,
    channel: &str,
    message: &mut dataflow_rs::Message,
    timeout_ms: Option<u64>,
    capture: Option<TraceCapture>,
) -> Result<EngineCallResult, u64> {
    if let Some(capture) = capture {
        // A trace-capturing run reports task failure through the trace itself,
        // so an `Err` here is the workflow's error, not the call's.
        let inner = with_deadline(
            timeout_ms,
            engine.process_message_for_channel_with_trace_options(
                channel,
                message,
                trace_options(capture.max_snapshot_bytes),
            ),
        )
        .await?;
        Ok(match inner {
            Ok(trace) => (Ok(()), Some(trace)),
            Err(e) => (Err(e), None),
        })
    } else {
        let inner = with_deadline(
            timeout_ms,
            engine.process_message_for_channel(channel, message),
        )
        .await?;
        Ok((inner, None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F46: both halves of the `capture_trace` choice must honour the deadline,
    /// and both must pass it through untimed. The four-branch original made
    /// that a coincidence; this pins it.
    #[tokio::test]
    async fn a_deadline_applies_regardless_of_trace_capture() {
        let slow = || async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            42
        };
        assert_eq!(with_deadline(Some(20), slow()).await, Err(20));
        assert_eq!(with_deadline(None, slow()).await, Ok(42));
        assert_eq!(with_deadline(Some(5_000), slow()).await, Ok(42));
    }
}
