//! V2-08 Owned native work scheduler, permits, single-flight loader, live
//! capacity ceiling updates across revisions, and bounded graceful shutdown.

use std::collections::BTreeMap;
use std::future::Future;
use std::hash::Hash;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{Notify, watch};

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
        self.scheduler.in_flight.fetch_sub(1, Ordering::AcqRel);
        self.scheduler
            .completed_total
            .fetch_add(1, Ordering::AcqRel);
        self.scheduler.notify.notify_waiters();
    }
}

/// Unified scheduler for native blocking/inference/probe/load work (`V2-08.1`..`V2-08.5`).
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

    /// V2-08.4: Dynamically update the concurrency ceiling across revisions.
    /// Reducing the ceiling never steals permits from running jobs; new work
    /// simply waits until `in_flight < new_max_in_flight`.
    pub fn update_max_in_flight(&self, new_max: usize) {
        self.inner
            .max_in_flight
            .store(new_max.max(1), Ordering::Release);
        self.inner.notify.notify_waiters();
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
        loop {
            if Instant::now() >= deadline {
                return Err(NativeWorkError {
                    code: "NATIVE_QUEUE_DEADLINE_EXCEEDED",
                    message: "deadline expired while waiting in native work queue".to_string(),
                });
            }
            let notified = self.inner.notify.notified();
            match self.try_acquire(label_str.clone()) {
                Ok(permit) => return Ok(permit),
                Err(e) if e.code == "SCHEDULER_SHUTTING_DOWN" => return Err(e),
                Err(_) => {
                    let rem = deadline.saturating_duration_since(Instant::now());
                    if rem.is_zero() || tokio::time::timeout(rem, notified).await.is_err() {
                        return Err(NativeWorkError {
                            code: "NATIVE_QUEUE_DEADLINE_EXCEEDED",
                            message: "deadline expired while waiting in native work queue"
                                .to_string(),
                        });
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

type InFlightLoadReceiver<V> = watch::Receiver<Option<Result<V, NativeWorkError>>>;

/// V2-08.3: Single-flight loader where the load job is independently owned on a
/// background task and holds its [`OwnedPermit`] until completion. Waiter
/// cancellation removes only that waiter without aborting the running load.
pub struct SingleFlightLoader<K, V> {
    scheduler: NativeWorkScheduler,
    cache: Arc<Mutex<BTreeMap<K, V>>>,
    in_flight_loads: Arc<Mutex<BTreeMap<K, InFlightLoadReceiver<V>>>>,
}

impl<K, V> Clone for SingleFlightLoader<K, V> {
    fn clone(&self) -> Self {
        Self {
            scheduler: self.scheduler.clone(),
            cache: Arc::clone(&self.cache),
            in_flight_loads: Arc::clone(&self.in_flight_loads),
        }
    }
}

impl<K, V> SingleFlightLoader<K, V>
where
    K: Ord + Clone + Hash + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    pub fn new(scheduler: NativeWorkScheduler) -> Self {
        Self {
            scheduler,
            cache: Arc::new(Mutex::new(BTreeMap::new())),
            in_flight_loads: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn get_cached(&self, key: &K) -> Option<V> {
        self.cache.lock().expect("cache lock").get(key).cloned()
    }

    pub async fn get_or_load<F, Fut>(
        &self,
        key: K,
        waiter_deadline: Instant,
        loader: F,
    ) -> Result<V, NativeWorkError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<V, NativeWorkError>> + Send + 'static,
    {
        if let Some(cached) = self.get_cached(&key) {
            return Ok(cached);
        }

        let existing_rx = self
            .in_flight_loads
            .lock()
            .expect("in_flight_loads lock")
            .get(&key)
            .cloned();

        let mut rx = if let Some(existing_rx) = existing_rx {
            existing_rx
        } else {
            let permit = self
                .scheduler
                .acquire_until("single-flight-load", waiter_deadline)
                .await?;
            let mut g = self.in_flight_loads.lock().expect("in_flight_loads lock");
            if let Some(existing_rx) = g.get(&key) {
                existing_rx.clone()
            } else {
                let (tx, rx) = watch::channel(None);
                g.insert(key.clone(), rx.clone());

                let cache = Arc::clone(&self.cache);
                let in_flight = Arc::clone(&self.in_flight_loads);
                let key_clone = key.clone();
                tokio::spawn(async move {
                    let _owned_permit = permit;
                    _owned_permit.mark_running();
                    let res = loader().await;
                    if let Ok(ref val) = res {
                        cache
                            .lock()
                            .expect("cache lock")
                            .insert(key_clone.clone(), val.clone());
                    }
                    in_flight
                        .lock()
                        .expect("in_flight_loads lock")
                        .remove(&key_clone);
                    let _ = tx.send(Some(res));
                });
                rx
            }
        };

        loop {
            if let Some(res) = rx.borrow().clone() {
                return res;
            }
            let rem = waiter_deadline.saturating_duration_since(Instant::now());
            if rem.is_zero() || tokio::time::timeout(rem, rx.changed()).await.is_err() {
                return Err(NativeWorkError {
                    code: "SINGLE_FLIGHT_WAITER_TIMEOUT",
                    message: "waiter timed out while owned single-flight load continues"
                        .to_string(),
                });
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
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

    #[tokio::test]
    async fn single_flight_load_continues_and_populates_cache_when_creator_times_out() {
        let scheduler = NativeWorkScheduler::new(2);
        let loader = SingleFlightLoader::<String, String>::new(scheduler.clone());

        // First caller (creator) has a 10ms deadline while load takes 50ms
        let loader1 = loader.clone();
        let err = loader1
            .get_or_load(
                "model-v1".to_string(),
                Instant::now() + Duration::from_millis(10),
                || async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Ok("compiled-weights".to_string())
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, "SINGLE_FLIGHT_WAITER_TIMEOUT");

        // Underlying load is still running and holding its permit
        assert_eq!(scheduler.in_flight(), 1);

        // Wait for the background load to finish and verify cache is populated
        tokio::time::sleep(Duration::from_millis(70)).await;
        assert_eq!(scheduler.in_flight(), 0);
        assert_eq!(
            loader.get_cached(&"model-v1".to_string()),
            Some("compiled-weights".to_string())
        );
    }
}
