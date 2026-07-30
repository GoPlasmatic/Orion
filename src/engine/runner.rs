//! Running the engine for one channel, and the instrumented lock accessors
//! every caller goes through to reach it.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;

use super::profile;

/// Acquire the engine read lock with timing instrumentation.
///
/// Records lock wait time as a histogram metric and returns the cloned inner `Arc<Engine>`,
/// releasing the lock immediately.
pub async fn acquire_engine_read(
    lock: &RwLock<Arc<dataflow_rs::Engine>>,
) -> Arc<dataflow_rs::Engine> {
    let start = std::time::Instant::now();
    let guard = lock.read().await;
    let elapsed = start.elapsed();
    crate::metrics::record_engine_lock_wait("read", elapsed.as_secs_f64());
    profile::record_engine_lock_wait(elapsed);
    guard.clone()
}

/// Acquire the engine write lock with timing instrumentation.
///
/// Records lock wait time as a histogram metric.
pub async fn acquire_engine_write(
    lock: &RwLock<Arc<dataflow_rs::Engine>>,
) -> tokio::sync::RwLockWriteGuard<'_, Arc<dataflow_rs::Engine>> {
    let start = std::time::Instant::now();
    let guard = lock.write().await;
    crate::metrics::record_engine_lock_wait("write", start.elapsed().as_secs_f64());
    guard
}

/// Result of one engine invocation: the engine's own result plus the captured
/// per-task `ExecutionTrace` when the caller opted in (A2).
pub type EngineCallResult = (dataflow_rs::Result<()>, Option<dataflow_rs::ExecutionTrace>);

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
    capture_trace: bool,
) -> Result<EngineCallResult, u64> {
    let run = run_for_channel_inner(engine, channel, message, timeout_ms, capture_trace);
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

async fn run_for_channel_inner(
    engine: &Arc<dataflow_rs::Engine>,
    channel: &str,
    message: &mut dataflow_rs::Message,
    timeout_ms: Option<u64>,
    capture_trace: bool,
) -> Result<EngineCallResult, u64> {
    if capture_trace {
        // A trace-capturing run reports task failure through the trace itself,
        // so an `Err` here is the workflow's error, not the call's.
        let inner = with_deadline(
            timeout_ms,
            engine.process_message_for_channel_with_trace(channel, message),
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
