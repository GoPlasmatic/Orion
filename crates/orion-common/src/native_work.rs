//! The node's owned native work scheduler: a bound on blocking native jobs,
//! permits owned by the job rather than its caller, and a bounded drain at
//! shutdown.
//!
//! A native job is work that occupies an OS thread for its whole length:
//! compiling a plugin component with Cranelift on this node, and whatever an
//! extension built on `orion_server::extensions::CompositionRoot` schedules here.
//! `engine.native_work_max_in_flight` bounds how many run at once, and
//! `engine.native_work_timeout_ms` how long a caller waits for one. A caller
//! that gives up does not release the slot: the job holds its permit until
//! its thread finishes or unwinds, so the bound counts the threads actually
//! busy. `orion_native_work_in_flight` reports it, and
//! `GET /api/v2/admin/runtime-capabilities` reports both numbers under
//! `native_scheduler`.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeJobState {
    Admitted,
    Running,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeJobInfo {
    pub job_id: u64,
    pub label: String,
    pub state: NativeJobState,
    pub caller_abandoned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownDrainReport {
    pub drained_cleanly: bool,
    pub residual_jobs: Vec<NativeJobInfo>,
    pub completed_jobs_total: u64,
    pub abandoned_waiters_total: u64,
    pub unwinds_total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct NativeWorkError {
    pub code: &'static str,
    pub message: String,
}

struct SchedulerInner {
    max_in_flight: AtomicUsize,
    in_flight: AtomicUsize,
    shutting_down: AtomicBool,
    next_job_id: AtomicU64,
    completed_total: AtomicU64,
    abandoned_waiters_total: AtomicU64,
    unwinds_total: AtomicU64,
    jobs: Mutex<BTreeMap<u64, NativeJobInfo>>,
    notify: Notify,
}

/// Permit moved into the actual native work closure (`V2-08.1`).
///
/// Capacity is released only when this permit is dropped at the end of the
/// native closure (including on panic/unwind), never when a waiting caller
/// times out or cancels its future.
pub struct OwnedPermit {
    scheduler: Arc<SchedulerInner>,
    job_id: u64,
}

impl OwnedPermit {
    pub fn job_id(&self) -> u64 {
        self.job_id
    }

    pub fn mark_running(&self) {
        let mut g = self.scheduler.jobs.lock().expect("jobs lock");
        if let Some(info) = g.get_mut(&self.job_id) {
            info.state = NativeJobState::Running;
        }
    }
}

impl Drop for OwnedPermit {
    fn drop(&mut self) {
        {
            let mut g = self.scheduler.jobs.lock().expect("jobs lock");
            g.remove(&self.job_id);
        }
        let now = self.scheduler.in_flight.fetch_sub(1, Ordering::AcqRel) - 1;
        crate::metrics::set_native_work_in_flight(now);
        self.scheduler
            .completed_total
            .fetch_add(1, Ordering::AcqRel);
        self.scheduler.notify.notify_waiters();
    }
}

/// The scheduler every blocking native job on this node runs under.
#[derive(Clone)]
pub struct NativeWorkScheduler {
    inner: Arc<SchedulerInner>,
}

impl NativeWorkScheduler {
    pub fn new(max_in_flight: usize) -> Self {
        Self {
            inner: Arc::new(SchedulerInner {
                max_in_flight: AtomicUsize::new(max_in_flight.max(1)),
                in_flight: AtomicUsize::new(0),
                shutting_down: AtomicBool::new(false),
                next_job_id: AtomicU64::new(1),
                completed_total: AtomicU64::new(0),
                abandoned_waiters_total: AtomicU64::new(0),
                unwinds_total: AtomicU64::new(0),
                jobs: Mutex::new(BTreeMap::new()),
                notify: Notify::new(),
            }),
        }
    }

    pub fn in_flight(&self) -> usize {
        self.inner.in_flight.load(Ordering::Acquire)
    }

    pub fn max_in_flight(&self) -> usize {
        self.inner.max_in_flight.load(Ordering::Acquire)
    }

    /// Try to acquire an [`OwnedPermit`] immediately without waiting.
    pub fn try_acquire(&self, label: impl Into<String>) -> Result<OwnedPermit, NativeWorkError> {
        if self.inner.shutting_down.load(Ordering::Acquire) {
            return Err(NativeWorkError {
                code: "SCHEDULER_SHUTTING_DOWN",
                message: "scheduler is shutting down".to_string(),
            });
        }
        loop {
            let cur = self.inner.in_flight.load(Ordering::Acquire);
            let max = self.inner.max_in_flight.load(Ordering::Acquire);
            if cur >= max {
                return Err(NativeWorkError {
                    code: "NATIVE_CAPACITY_EXHAUSTED",
                    message: format!("in_flight {cur} >= ceiling {max}"),
                });
            }
            if self
                .inner
                .in_flight
                .compare_exchange(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                crate::metrics::set_native_work_in_flight(cur + 1);
                let job_id = self.inner.next_job_id.fetch_add(1, Ordering::Relaxed);
                let mut g = self.inner.jobs.lock().expect("jobs lock");
                g.insert(
                    job_id,
                    NativeJobInfo {
                        job_id,
                        label: label.into(),
                        state: NativeJobState::Admitted,
                        caller_abandoned: false,
                    },
                );
                return Ok(OwnedPermit {
                    scheduler: Arc::clone(&self.inner),
                    job_id,
                });
            }
        }
    }

    /// Acquire an [`OwnedPermit`] before `deadline`, consuming queue wait time
    /// from the caller's original deadline (`V2-08.2`).
    pub async fn acquire_until(
        &self,
        label: impl Into<String>,
        deadline: Instant,
    ) -> Result<OwnedPermit, NativeWorkError> {
        let label_str = label.into();
        let queue_deadline = || {
            crate::metrics::record_native_work_refusal("queue_deadline");
            NativeWorkError {
                code: "NATIVE_QUEUE_DEADLINE_EXCEEDED",
                message: "deadline expired while waiting in native work queue".to_string(),
            }
        };
        loop {
            if Instant::now() >= deadline {
                return Err(queue_deadline());
            }
            let notified = self.inner.notify.notified();
            match self.try_acquire(label_str.clone()) {
                Ok(permit) => return Ok(permit),
                Err(e) if e.code == "SCHEDULER_SHUTTING_DOWN" => {
                    crate::metrics::record_native_work_refusal("shutting_down");
                    return Err(e);
                }
                Err(_) => {
                    let rem = deadline.saturating_duration_since(Instant::now());
                    if rem.is_zero() || tokio::time::timeout(rem, notified).await.is_err() {
                        return Err(queue_deadline());
                    }
                }
            }
        }
    }

    /// V2-08.1 & V2-08.5: Spawn a blocking native job that owns its [`OwnedPermit`]
    /// inside the thread closure. If the caller times out at `caller_deadline`,
    /// the caller receives `NATIVE_CALLER_DEADLINE_EXCEEDED` while the blocking
    /// job keeps its permit until the native thread actually finishes or unwinds.
    pub async fn run_owned_blocking<F, T>(
        &self,
        label: impl Into<String>,
        caller_deadline: Instant,
        work: F,
    ) -> Result<T, NativeWorkError>
    where
        F: FnOnce() -> Result<T, NativeWorkError> + Send + 'static,
        T: Send + 'static,
    {
        let permit = self.acquire_until(label, caller_deadline).await?;
        let job_id = permit.job_id();
        let sched = Arc::clone(&self.inner);

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _owned_permit = permit;
            _owned_permit.mark_running();
            let outcome = catch_unwind(AssertUnwindSafe(work));
            match outcome {
                Ok(res) => {
                    let _ = tx.send(res);
                }
                Err(_) => {
                    sched.unwinds_total.fetch_add(1, Ordering::AcqRel);
                    crate::metrics::record_native_work_refusal("panicked");
                    let _ = tx.send(Err(NativeWorkError {
                        code: "NATIVE_WORK_PANICKED",
                        message: "native job panicked and unwound".to_string(),
                    }));
                }
            }
        });

        let rem = caller_deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(rem, &mut rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(NativeWorkError {
                code: "NATIVE_WORK_DROPPED",
                message: "native worker channel closed".to_string(),
            }),
            Err(_) => {
                crate::metrics::record_native_work_refusal("caller_deadline");
                self.inner
                    .abandoned_waiters_total
                    .fetch_add(1, Ordering::AcqRel);
                if let Some(info) = self.inner.jobs.lock().expect("jobs lock").get_mut(&job_id) {
                    info.caller_abandoned = true;
                }
                Err(NativeWorkError {
                    code: "NATIVE_CALLER_DEADLINE_EXCEEDED",
                    message: "caller deadline expired while native job remains running".to_string(),
                })
            }
        }
    }

    /// V2-08.5: Initiate graceful shutdown and wait up to `drain_timeout` for
    /// owned native jobs to complete, reporting any residual jobs still running.
    pub async fn graceful_shutdown(&self, drain_timeout: Duration) -> ShutdownDrainReport {
        self.inner.shutting_down.store(true, Ordering::Release);
        let deadline = Instant::now() + drain_timeout;
        loop {
            if self.in_flight() == 0 || Instant::now() >= deadline {
                break;
            }
            let notified = self.inner.notify.notified();
            if self.in_flight() == 0 {
                break;
            }
            let rem = deadline.saturating_duration_since(Instant::now());
            if tokio::time::timeout(rem, notified).await.is_err() {
                break;
            }
        }
        let residual_jobs: Vec<NativeJobInfo> = self
            .inner
            .jobs
            .lock()
            .expect("jobs lock")
            .values()
            .cloned()
            .collect();
        ShutdownDrainReport {
            drained_cleanly: residual_jobs.is_empty(),
            residual_jobs,
            completed_jobs_total: self.inner.completed_total.load(Ordering::Acquire),
            abandoned_waiters_total: self.inner.abandoned_waiters_total.load(Ordering::Acquire),
            unwinds_total: self.inner.unwinds_total.load(Ordering::Acquire),
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[tokio::test]
    async fn blocked_native_job_retains_permit_after_caller_timeout_and_releases_on_completion() {
        let scheduler = NativeWorkScheduler::new(1);
        let gate = Arc::new(Barrier::new(2));
        let gate_worker = Arc::clone(&gate);

        // Caller times out in 15ms while native thread blocks on `gate`
        let res = scheduler
            .run_owned_blocking(
                "onnx-infer",
                Instant::now() + Duration::from_millis(15),
                move || {
                    gate_worker.wait();
                    Ok(42)
                },
            )
            .await;
        assert_eq!(res.unwrap_err().code, "NATIVE_CALLER_DEADLINE_EXCEEDED");

        // Capacity MUST still be 1 because the native thread has not finished yet!
        assert_eq!(scheduler.in_flight(), 1);
        assert!(scheduler.try_acquire("second-job").is_err());

        // Release the native thread and verify capacity returns to 0
        gate.wait();
        let report = scheduler.graceful_shutdown(Duration::from_secs(2)).await;
        assert!(report.drained_cleanly);
        assert_eq!(scheduler.in_flight(), 0);
        assert_eq!(report.abandoned_waiters_total, 1);
    }
}
