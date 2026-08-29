//! Supervision and liveness for the node's long-lived background tasks.
//!
//! Every one of them used to be a bare `tokio::spawn` whose `JoinHandle` was
//! kept only so shutdown could `abort()` it. Nothing watched them. If the
//! trace dispatcher, a persistence worker, the audit writer, the DLQ retry
//! consumer or the cluster epoch watcher stopped — a panic in a task is not a
//! process abort, it resolves the join handle nobody was awaiting — the node
//! kept answering `/readyz` with `ready` and kept serving 200s. Submitters saw
//! the symptom and reported it as something else: the trace queue turned a
//! closed channel into a 503, the persistence queue counted it as an overflow
//! drop, the audit queue logged a warning per lost row. **A node with a dead
//! persistence worker silently lost traces while reporting ready.**
//!
//! What this module adds is one owner that knows every task's name and state,
//! so the probes can report it and shutdown can stop them cooperatively.
//!
//! # The two shapes, and why the distinction is real
//!
//! [`TaskRegistry::supervise`] takes a **factory**, owns the join handle, and
//! re-runs what the factory makes after a capped backoff.
//! [`TaskRegistry::guard`] hands back a [`TaskGuard`] the caller wraps its own
//! future in; the caller keeps the join handle and the registry only learns
//! the state.
//!
//! The split is not a policy choice, it is what the tasks allow.
//!
//! A retention job or the epoch watcher holds nothing but clones — running its
//! body again is exactly as valid as running it the first time, so the
//! registry can own it outright and repair it.
//!
//! The trace dispatcher, the persistence workers and the audit writer each own
//! the receiving end of an `mpsc` channel, moved into the task at spawn. Two
//! things follow. Restarting one is meaningless: when the task ends the
//! receiver drops, the channel closes, and a fresh body would consume nothing.
//! And their shutdown is a *drain* in a fixed order — the worker pool feeds
//! the persistence queue, so persistence must finish after the workers do —
//! triggered by dropping the last sender rather than by any signal. Taking
//! their join handles into a set the registry joins concurrently would break
//! that ordering. So they are guarded, not owned: the drain stays exactly
//! where it is, and the registry learns whether each one is alive.
//!
//! # Shutdown
//!
//! One `watch` channel, the same cooperative shape the Kafka consume loop
//! already uses. A supervised body takes a [`Shutdown`] and is expected to
//! return when it fires; [`TaskRegistry::shutdown`] signals, then joins under
//! one deadline for the set it owns and names anything that outlived it. That
//! replaces `JoinHandle::abort()`, which could cut a retention job between its
//! `DELETE` and its metric, or an epoch watcher mid-query.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

/// First retry delay after a supervised task fails.
pub const INITIAL_BACKOFF_MS: u64 = 1_000;
/// Ceiling the retry delay doubles up to.
pub const MAX_BACKOFF_MS: u64 = 60_000;

/// Double a retry delay, capped at [`MAX_BACKOFF_MS`].
///
/// The Kafka consume loop's own retry uses the identical curve and constants
/// (`kafka::consumer::next_backoff_ms`); they are separate because that one is
/// per-*message* and reads `max.poll.interval.ms`, while this one is per-task.
pub fn next_backoff_ms(current_ms: u64) -> u64 {
    current_ms.saturating_mul(2).min(MAX_BACKOFF_MS)
}

/// How the probes should read a task that is not running.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Criticality {
    /// The node cannot do its job without it: `/readyz` reports not-ready, so
    /// an orchestrator takes the node out of rotation instead of routing
    /// traffic whose side effects will be dropped.
    Required,
    /// Serving continues without it. Reported on `/health` as degraded and
    /// left out of readiness — a node that has stopped expiring old traces
    /// still answers requests correctly.
    Optional,
}

/// What a supervised task is doing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskState {
    Running,
    /// Failed and waiting out a backoff before its body runs again.
    Restarting,
    /// Gone, and not coming back on its own.
    Failed,
    /// Returned after the shutdown signal — the expected end.
    ShutDown,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Restarting => "restarting",
            Self::Failed => "failed",
            Self::ShutDown => "shut_down",
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Restarting,
            2 => Self::Failed,
            3 => Self::ShutDown,
            _ => Self::Running,
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            Self::Running => 0,
            Self::Restarting => 1,
            Self::Failed => 2,
            Self::ShutDown => 3,
        }
    }
}

/// One task's liveness, as the probes see it.
#[derive(Clone, Debug)]
pub struct TaskReport {
    pub name: &'static str,
    pub criticality: Criticality,
    pub state: TaskState,
    /// How many times the supervisor has re-run the body. Non-zero on a
    /// running task is the signal worth alerting on: it is up *now*, and it
    /// has been failing.
    pub restarts: u32,
}

impl TaskReport {
    /// Whether this task's state should make `/readyz` answer not-ready.
    ///
    /// `Restarting` deliberately does not: the supervisor is repairing it, and
    /// pulling a node out of rotation for a transient database blip that the
    /// next backoff will ride out is the more expensive failure. `Failed` does
    /// — nothing is coming to fix it.
    pub fn blocks_readiness(&self) -> bool {
        self.criticality == Criticality::Required && self.state == TaskState::Failed
    }

    /// Whether `/health` should call the node degraded because of this task.
    pub fn is_degraded(&self) -> bool {
        matches!(self.state, TaskState::Restarting | TaskState::Failed)
    }
}

struct Slot {
    name: &'static str,
    criticality: Criticality,
    state: AtomicU8,
    restarts: AtomicU32,
}

impl Slot {
    fn report(&self) -> TaskReport {
        TaskReport {
            name: self.name,
            criticality: self.criticality,
            state: TaskState::from_u8(self.state.load(Ordering::Relaxed)),
            restarts: self.restarts.load(Ordering::Relaxed),
        }
    }

    fn set(&self, state: TaskState) {
        self.state.store(state.as_u8(), Ordering::Relaxed);
    }
}

/// The shutdown signal handed to a supervised task body.
///
/// A loop should treat it the way the Kafka consume loop treats its own: check
/// it between units of work, and race it against any sleep, so shutdown is
/// bounded by one unit rather than by one poll interval.
#[derive(Clone)]
pub struct Shutdown(watch::Receiver<bool>);

impl Shutdown {
    /// Whether shutdown has been signalled.
    pub fn is_signalled(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolve when shutdown is signalled (immediately if it already was).
    pub async fn signalled(&mut self) {
        while !*self.0.borrow_and_update() {
            if self.0.changed().await.is_err() {
                // The registry is gone; treat that as shutdown rather than
                // parking a task forever on a dead channel.
                return;
            }
        }
    }

    /// Sleep for `duration`, cut short by shutdown. `false` means shutdown
    /// fired — the caller should return rather than do another pass.
    pub async fn sleep(&mut self, duration: Duration) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(duration) => true,
            _ = self.signalled() => false,
        }
    }
}

/// The node's supervised background tasks.
///
/// Held on `AppState` so the probes can read it, and by `main` so shutdown can
/// stop it. Registration happens at boot; nothing registers a task later.
pub struct TaskRegistry {
    shutdown_tx: watch::Sender<bool>,
    slots: std::sync::Mutex<Vec<Arc<Slot>>>,
    joins: std::sync::Mutex<Vec<(&'static str, JoinHandle<()>)>>,
}

impl Default for TaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskRegistry {
    pub fn new() -> Self {
        let (shutdown_tx, _) = watch::channel(false);
        Self {
            shutdown_tx,
            slots: std::sync::Mutex::new(Vec::new()),
            joins: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// A receiver for the shutdown signal, for a task that manages its own
    /// spawn (the Kafka consumer's restart supervisor) but still wants to stop
    /// when the node does.
    pub fn shutdown_signal(&self) -> Shutdown {
        Shutdown(self.shutdown_tx.subscribe())
    }

    fn register(&self, name: &'static str, criticality: Criticality) -> Arc<Slot> {
        let slot = Arc::new(Slot {
            name,
            criticality,
            state: AtomicU8::new(TaskState::Running.as_u8()),
            restarts: AtomicU32::new(0),
        });
        lock(&self.slots).push(slot.clone());
        slot
    }

    /// Spawn a task that can be re-run, and re-run it when it stops early.
    ///
    /// `body` is called once per attempt. A return *before* the shutdown
    /// signal is a failure however clean it looked — every caller here is an
    /// endless loop, so returning at all means something went wrong — and is
    /// retried after a capped backoff. A panic is caught by the join and
    /// treated the same way, which is the case that used to be invisible.
    pub fn supervise<F, Fut>(&self, name: &'static str, criticality: Criticality, body: F)
    where
        F: Fn(Shutdown) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let slot = self.register(name, criticality);
        let mut shutdown = self.shutdown_signal();
        let join = tokio::spawn(async move {
            let mut backoff_ms = INITIAL_BACKOFF_MS;
            loop {
                let attempt = tokio::spawn(body(shutdown.clone()));
                let outcome = attempt.await;

                if shutdown.is_signalled() {
                    slot.set(TaskState::ShutDown);
                    return;
                }

                match outcome {
                    Ok(()) => tracing::error!(
                        task = name,
                        backoff_ms,
                        "Background task returned before shutdown; restarting"
                    ),
                    Err(e) => tracing::error!(
                        task = name,
                        backoff_ms,
                        error = %e,
                        "Background task died; restarting"
                    ),
                }
                crate::metrics::record_error("background_task");
                slot.set(TaskState::Restarting);

                if !shutdown.sleep(Duration::from_millis(backoff_ms)).await {
                    slot.set(TaskState::ShutDown);
                    return;
                }
                backoff_ms = next_backoff_ms(backoff_ms);
                slot.restarts.fetch_add(1, Ordering::Relaxed);
                slot.set(TaskState::Running);
            }
        });
        lock(&self.joins).push((name, join));
    }

    /// Register a task the caller spawns and joins itself, for liveness only.
    ///
    /// The caller wraps its future in the returned guard
    /// ([`TaskGuard::run`]) and keeps the `JoinHandle`. Used by the three
    /// queue consumers, whose drain order the registry must not take over —
    /// see the module docs.
    pub fn guard(&self, name: &'static str, criticality: Criticality) -> TaskGuard {
        TaskGuard {
            slot: self.register(name, criticality),
            shutdown: self.shutdown_signal(),
        }
    }

    /// Every registered task's liveness, in registration order.
    pub fn report(&self) -> Vec<TaskReport> {
        lock(&self.slots).iter().map(|s| s.report()).collect()
    }

    /// The tasks whose state should make `/readyz` answer not-ready.
    pub fn blocking_readiness(&self) -> Vec<&'static str> {
        self.report()
            .into_iter()
            .filter(TaskReport::blocks_readiness)
            .map(|r| r.name)
            .collect()
    }

    /// Signal every task to stop, without waiting.
    ///
    /// For a caller that cannot `.await` — the cluster test harness's `Drop`,
    /// which needs its per-test epoch watchers to stop polling a database it
    /// is about to destroy. A supervised loop sees the signal at its next tick
    /// and returns.
    pub fn signal_shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// Signal every task to stop and wait, bounded by `deadline` for the whole
    /// set. Names anything still running when it expires; nothing is aborted,
    /// because a task cut between a write and its bookkeeping is worse than a
    /// task that outlives the process by a few milliseconds.
    pub async fn shutdown(&self, deadline: Duration) {
        self.signal_shutdown();
        let joins = std::mem::take(&mut *lock(&self.joins));
        if joins.is_empty() {
            return;
        }
        tracing::info!(tasks = joins.len(), "Stopping background tasks...");

        let names: Vec<&'static str> = joins.iter().map(|(name, _)| *name).collect();
        let all = futures::future::join_all(joins.into_iter().map(|(_, join)| join));
        if tokio::time::timeout(deadline, all).await.is_err() {
            let still_running: Vec<&'static str> = self
                .report()
                .into_iter()
                .filter(|r| r.state != TaskState::ShutDown)
                .map(|r| r.name)
                .collect();
            tracing::warn!(
                deadline_secs = deadline.as_secs(),
                registered = ?names,
                still_running = ?still_running,
                "Background tasks did not all stop within the shutdown deadline"
            );
        }
    }
}

/// Liveness reporting for a task the registry does not own.
///
/// Wrap the task's future in [`Self::run`] and spawn *that*. The state is
/// settled in `Drop`, which is what makes a panic visible: a panicking future
/// unwinds past any code after the `.await`, but its locals — this guard among
/// them — are still dropped, so the task is recorded as failed rather than
/// vanishing into a `JoinHandle` nobody inspects.
///
/// `Clone` covers a *pool*: the persistence queue runs N workers over one
/// channel each, and one dead worker means that worker's share of traces is
/// silently dropped. Every clone settles the same slot, so the pool is one
/// entry in the report and any member's death fails it — which is the state
/// the probes should act on.
#[derive(Clone)]
pub struct TaskGuard {
    slot: Arc<Slot>,
    shutdown: Shutdown,
}

impl TaskGuard {
    /// Run `body` under this guard. The returned future is what the caller
    /// spawns; the caller keeps its `JoinHandle`.
    pub async fn run<Fut: Future<Output = ()>>(self, body: Fut) {
        body.await;
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        if self.shutdown.is_signalled() {
            self.slot.set(TaskState::ShutDown);
            return;
        }
        self.slot.set(TaskState::Failed);
        crate::metrics::record_error("background_task");
        tracing::error!(
            task = self.slot.name,
            "Background task stopped before shutdown and cannot be restarted (it owns \
             its queue's receiver, so the queue behind it is now closed)"
        );
    }
}

/// A slot list holds plain data, so a panic mid-update cannot corrupt it and a
/// poisoned lock is safe to keep using — the same argument the SSRF pin cache
/// makes.
fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    // The crate warns on `panic!` because production code should not have
    // any. These tests need a task that panics — that is the failure the
    // supervisor exists to catch, and the only way to produce it is to write
    // one.
    #![allow(clippy::panic)]

    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn state_of(registry: &TaskRegistry, name: &str) -> TaskState {
        registry
            .report()
            .into_iter()
            .find(|r| r.name == name)
            .expect("the task is registered")
            .state
    }

    /// The failure the supervisor exists for: a background task that panics
    /// used to resolve a `JoinHandle` nobody awaited, so the node kept
    /// reporting ready with the work silently stopped. A guarded task records
    /// it, and a `Required` one blocks readiness.
    #[tokio::test]
    async fn a_panicking_guarded_task_is_recorded_and_blocks_readiness() {
        let registry = TaskRegistry::new();
        let guard = registry.guard("persistence", Criticality::Required);

        let join = tokio::spawn(guard.run(async {
            panic!("worker exploded");
        }));
        let _ = join.await;

        assert_eq!(state_of(&registry, "persistence"), TaskState::Failed);
        assert_eq!(registry.blocking_readiness(), vec!["persistence"]);
    }

    /// An `Optional` task's death is reported but must not take the node out
    /// of rotation: retention stopping does not make it unfit to serve.
    #[tokio::test]
    async fn an_optional_task_degrades_health_without_blocking_readiness() {
        let registry = TaskRegistry::new();
        let guard = registry.guard("retention", Criticality::Optional);
        let join = tokio::spawn(guard.run(async {}));
        join.await.expect("clean return");

        let report = registry.report();
        assert_eq!(report[0].state, TaskState::Failed);
        assert!(report[0].is_degraded());
        assert!(registry.blocking_readiness().is_empty());
    }

    /// Returning *after* the shutdown signal is the expected end, not a fault.
    #[tokio::test]
    async fn a_task_that_stops_on_the_signal_is_not_a_failure() {
        let registry = TaskRegistry::new();
        registry.supervise("looper", Criticality::Required, |mut shutdown| async move {
            shutdown.signalled().await;
        });

        registry.shutdown(Duration::from_secs(5)).await;

        assert_eq!(state_of(&registry, "looper"), TaskState::ShutDown);
        assert!(registry.blocking_readiness().is_empty());
    }

    /// A supervised body that fails is re-run. The restart count is what
    /// distinguishes "up" from "up, and has been failing all night".
    #[tokio::test(start_paused = true)]
    async fn a_supervised_task_is_restarted_after_a_failure() {
        let registry = TaskRegistry::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = attempts.clone();

        registry.supervise("flaky", Criticality::Optional, move |mut shutdown| {
            let attempts = seen.clone();
            async move {
                let n = attempts.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    panic!("attempt {n} fails");
                }
                // Third attempt survives until shutdown.
                shutdown.signalled().await;
            }
        });

        // Two backoffs: 1s, then 2s.
        for _ in 0..40 {
            tokio::time::advance(Duration::from_millis(200)).await;
            tokio::task::yield_now().await;
        }

        assert_eq!(
            attempts.load(Ordering::SeqCst),
            3,
            "two restarts, then it held"
        );
        assert_eq!(state_of(&registry, "flaky"), TaskState::Running);
        assert_eq!(registry.report()[0].restarts, 2);
        registry.shutdown(Duration::from_secs(5)).await;
    }

    /// A task that ignores the signal must not hold shutdown open forever.
    /// It is left running rather than aborted — see [`TaskRegistry::shutdown`].
    #[tokio::test(start_paused = true)]
    async fn shutdown_is_bounded_by_its_deadline() {
        let registry = TaskRegistry::new();
        registry.supervise("stuck", Criticality::Optional, |_shutdown| async move {
            std::future::pending::<()>().await;
        });

        let started = tokio::time::Instant::now();
        registry.shutdown(Duration::from_secs(2)).await;
        assert!(started.elapsed() >= Duration::from_secs(2));
    }

    #[test]
    fn the_backoff_doubles_and_is_capped() {
        assert_eq!(next_backoff_ms(INITIAL_BACKOFF_MS), 2_000);
        assert_eq!(next_backoff_ms(40_000), 60_000);
        assert_eq!(next_backoff_ms(MAX_BACKOFF_MS), MAX_BACKOFF_MS);
        assert_eq!(next_backoff_ms(u64::MAX), MAX_BACKOFF_MS);
    }
}
