//! The bounded producer/consumer primitive behind the background queues.
//!
//! Three queues in this module tree accept work from the request path, hand it
//! to background workers, and have to be drained at shutdown: the async trace
//! queue ([`super::TraceQueue`]), the trace persistence queue
//! ([`super::TracePersistenceQueue`]) and the audit writer
//! ([`super::audit_queue::AuditQueue`]). Each had its own bounded mpsc, its own
//! depth counter, and its own drain — and the three had drifted on the one
//! thing that has a right answer.
//!
//! **The depth counter's ordering.** A queue publishes its depth as a gauge, so
//! producer and consumer both touch one `AtomicUsize`. `AuditQueue` increments
//! *before* the send and releases on failure; the other two incremented after a
//! successful send. Incrementing after is unsound: the item is visible to the
//! consumer the instant `try_send` returns, so the consumer can dequeue and run
//! `fetch_sub` while the producer's `fetch_add` is still in flight. `fetch_sub`
//! at zero wraps to `usize::MAX`, and that value is what gets published as the
//! depth gauge and interpolated into the "queue is full (N pending)" refusal.
//! `saturating_sub` at the decrement site does not help — it clamps the value
//! the consumer reads back, not the value left in the atomic.
//!
//! Incrementing first can only ever over-count, for the instant a send takes,
//! and over-counting merely makes a drain wait. So that is the ordering here,
//! and [`BoundedWorker::try_submit`] and [`WorkerReceiver::recv`] are a matched
//! pair: a consumer cannot forget the decrement and cannot get its order wrong,
//! because it never touches the counter itself.
//!
//! **What stays with the callers.** Policy, not mechanism. The memory-byte
//! reservation and its 503, the overflow log rate-limiter, the batch flush
//! schedule, the semaphore worker pool and the three different shutdown
//! timeouts are all deliberate per-queue answers and none of them moves here.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Subtract from a counter that must never go below zero, returning the value
/// left in it.
///
/// `fetch_sub` at zero wraps to `usize::MAX`, and the module doc above is about
/// why that matters: the wrapped value is what gets published as a gauge and
/// interpolated into a refusal message, and — for the trace queue's byte
/// counter — what gets compared against the configured ceiling, so one wrap
/// turns a capacity limit into a permanent 503. `saturating_sub` on the value
/// `fetch_sub` *returns* does not help: it clamps the copy the caller reads
/// back, not the value left in the atomic. Only a compare-and-swap can clamp
/// the atomic itself, which is what this does.
///
/// Every counter here is balanced by construction, so the clamp should never
/// fire; the `debug_assert` says so out loud, and release builds degrade to a
/// stuck-at-zero gauge rather than a counter reading 18 quintillion.
///
/// `Release` on success: a drain's `Acquire` load of zero has to mean every
/// write ordered before this release actually happened.
pub(crate) fn release_counter(counter: &AtomicUsize, by: usize) -> usize {
    let previous = counter
        .fetch_update(Ordering::Release, Ordering::Relaxed, |current| {
            Some(current.saturating_sub(by))
        })
        // The closure never returns `None`, so this cannot fail.
        .unwrap_or(0);
    debug_assert!(
        previous >= by,
        "counter underflow: released {by} from {previous}"
    );
    previous.saturating_sub(by)
}

/// The `metrics.rs` setter this queue publishes its depth through.
///
/// A function pointer rather than a metric name: every `gauge!` call in the
/// process lives in `metrics.rs`, and passing one of its setters keeps it that
/// way while letting the primitive publish on every counter change.
pub type DepthGauge = fn(f64);

/// Why a non-blocking submission was refused, handing the item back.
#[derive(Debug)]
pub enum Rejected<T> {
    /// Every shard was at capacity. The caller decides whether that is a 503
    /// or a counted drop.
    Full(T),
    /// Every shard's receiver is gone — the workers have stopped. No amount of
    /// waiting fixes this, which is why it is distinct from [`Self::Full`].
    Closed(T),
}

/// Why a blocking submission was refused.
///
/// Deliberately not [`Rejected`]: waiting *is* the answer to a full queue, so
/// [`BoundedWorker::submit_blocking`] cannot report `Full` — it either gets in
/// or runs out of time. Two error sets rather than one carrying an arm every
/// caller has to swear is unreachable.
#[derive(Debug)]
pub enum Blocked<T> {
    /// Every shard's receiver is gone.
    Closed(T),
    /// The timeout elapsed with no capacity.
    ///
    /// Carries no item: the abandoned `send` future owns it, and dropping that
    /// future is what gives up. The same loss the blocking policy has always
    /// had, now named rather than implied.
    TimedOut,
}

/// Producer handle. Cheap to clone — every clone shares the shards and the
/// counter, and the queue stays open until the last one is dropped.
pub struct BoundedWorker<T> {
    senders: Vec<mpsc::Sender<T>>,
    /// Round-robin cursor. Shared, so concurrent submitters spread across
    /// shards instead of all starting at the same one.
    next: Arc<AtomicUsize>,
    pending: Arc<AtomicUsize>,
    gauge: DepthGauge,
}

impl<T> Clone for BoundedWorker<T> {
    /// Hand-written: the derive would demand `T: Clone`, and the items on these
    /// queues are rows and events that are moved, never copied.
    fn clone(&self) -> Self {
        Self {
            senders: self.senders.clone(),
            next: self.next.clone(),
            pending: self.pending.clone(),
            gauge: self.gauge,
        }
    }
}

impl<T> BoundedWorker<T> {
    /// Build a queue of `shards` channels, each holding `capacity_per_shard`
    /// items, and the receivers to hand to that many worker tasks.
    ///
    /// One shard per worker rather than one channel shared behind a mutex: a
    /// shared receiver serialises every worker behind one lock, which is what
    /// made `async_workers > 1` deliver no parallelism before Q7.
    pub fn new(
        shards: usize,
        capacity_per_shard: usize,
        gauge: DepthGauge,
    ) -> (Self, Vec<WorkerReceiver<T>>) {
        let shards = shards.max(1);
        let capacity_per_shard = capacity_per_shard.max(1);
        let pending = Arc::new(AtomicUsize::new(0));

        let mut senders = Vec::with_capacity(shards);
        let mut receivers = Vec::with_capacity(shards);
        for _ in 0..shards {
            let (tx, rx) = mpsc::channel::<T>(capacity_per_shard);
            senders.push(tx);
            receivers.push(WorkerReceiver {
                rx,
                pending: pending.clone(),
                gauge,
            });
        }

        let worker = Self {
            senders,
            next: Arc::new(AtomicUsize::new(0)),
            pending,
            gauge,
        };
        (worker, receivers)
    }

    /// A queue with no shards, for the modes that route nothing through it.
    /// Every [`Self::try_submit`] answers [`Rejected::Closed`] without
    /// touching the counter, so call sites stay shape-uniform.
    pub fn disabled(gauge: DepthGauge) -> Self {
        Self {
            senders: Vec::new(),
            next: Arc::new(AtomicUsize::new(0)),
            pending: Arc::new(AtomicUsize::new(0)),
            gauge,
        }
    }

    /// Whether this queue has any workers behind it.
    pub fn is_disabled(&self) -> bool {
        self.senders.is_empty()
    }

    /// Accepted but not yet dequeued — the value published as the depth gauge.
    pub fn depth(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }

    /// Offer an item without waiting.
    ///
    /// Sheds rather than parks: capacity is a shed threshold, not a waiting
    /// room, because awaiting it here holds the calling request open for as
    /// long as the workers stay behind.
    ///
    /// With more than one shard this falls through to the siblings when the
    /// preferred one is full — a single stalled worker must not drop work while
    /// the others sit idle. [`Rejected::Closed`] is reported only when *every*
    /// shard is gone; one dead worker among several is a `Full`, because the
    /// others can still make progress.
    pub fn try_submit(&self, item: T) -> Result<(), Rejected<T>> {
        if self.senders.is_empty() {
            return Err(Rejected::Closed(item));
        }

        // Reserve before the send makes the item visible. See the module docs:
        // the reverse order lets a consumer's `fetch_sub` land at zero and wrap.
        self.pending.fetch_add(1, Ordering::AcqRel);

        let start = self.next.fetch_add(1, Ordering::Relaxed);
        let mut item = item;
        let mut any_open = false;
        for i in 0..self.senders.len() {
            let shard = &self.senders[(start.wrapping_add(i)) % self.senders.len()];
            match shard.try_send(item) {
                Ok(()) => {
                    (self.gauge)(self.depth() as f64);
                    return Ok(());
                }
                Err(mpsc::error::TrySendError::Full(returned)) => {
                    any_open = true;
                    item = returned;
                }
                Err(mpsc::error::TrySendError::Closed(returned)) => {
                    item = returned;
                }
            }
        }

        // Nothing entered the queue, so nothing downstream will subtract it.
        release_counter(&self.pending, 1);
        Err(if any_open {
            Rejected::Full(item)
        } else {
            Rejected::Closed(item)
        })
    }

    /// Offer an item, waiting up to `timeout` for capacity.
    ///
    /// One shard, no fallthrough: this is the "slow the producer down" policy,
    /// and hunting for a free sibling is the opposite of what it is for.
    pub async fn submit_blocking(&self, item: T, timeout: Duration) -> Result<(), Blocked<T>> {
        if self.senders.is_empty() {
            return Err(Blocked::Closed(item));
        }

        self.pending.fetch_add(1, Ordering::AcqRel);
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        let shard = &self.senders[start % self.senders.len()];

        match tokio::time::timeout(timeout, shard.send(item)).await {
            Ok(Ok(())) => {
                (self.gauge)(self.depth() as f64);
                Ok(())
            }
            Ok(Err(mpsc::error::SendError(returned))) => {
                release_counter(&self.pending, 1);
                Err(Blocked::Closed(returned))
            }
            Err(_elapsed) => {
                release_counter(&self.pending, 1);
                Err(Blocked::TimedOut)
            }
        }
    }

    /// The producer side to release at shutdown, paired with the counter a
    /// drain witnesses.
    ///
    /// Holding a clone here is what lets shutdown close the queue on its own
    /// schedule: dropping it releases only this reference, so the queue closes
    /// once the runtime has dropped its own — which is why a drain must run
    /// after the server has stopped accepting requests, not before.
    pub fn drain_handle(&self) -> DrainHandle<T> {
        DrainHandle {
            senders: self.senders.clone(),
            pending: self.pending.clone(),
        }
    }
}

/// One worker's end of the queue. Owning it is what keeps the queue open, so a
/// worker that exits closes its shard.
pub struct WorkerReceiver<T> {
    rx: mpsc::Receiver<T>,
    pending: Arc<AtomicUsize>,
    gauge: DepthGauge,
}

/// The outcome of a bounded receive.
#[derive(Debug)]
pub enum Recv<T> {
    Item(T),
    /// Every producer handle is gone and the buffer is empty. For a worker
    /// whose loop is its own drain, this is the exit condition.
    Closed,
    /// The deadline passed with nothing to take.
    Elapsed,
}

impl<T> WorkerReceiver<T> {
    /// Take the next item, releasing its reservation at once.
    ///
    /// Depth then means "sitting in the channel", which is what a worker that
    /// hands the item on — to a spawned task, or to a batch buffer — should
    /// publish. Pair with [`DrainWitness::TasksExit`].
    ///
    /// Cannot underflow: an item is only visible here after the `fetch_add`
    /// that preceded its send, so the counter is at least 1 whenever this runs.
    pub async fn recv(&mut self) -> Option<T> {
        let item = self.rx.recv().await?;
        self.release();
        Some(item)
    }

    /// Take the next item, holding its reservation until the returned lease is
    /// dropped.
    ///
    /// Depth then means "accepted and not yet finished", which is the stronger
    /// claim: a worker that holds the lease across its write makes a depth of
    /// zero mean the writes have landed, and that is exactly what
    /// [`DrainWitness::QueueEmpty`] needs to be sound. Use it whenever losing
    /// an item after dequeue would be a silent hole rather than a shed load.
    pub async fn recv_leased(&mut self) -> Option<Leased<T>> {
        let item = self.rx.recv().await?;
        Some(Leased {
            item,
            pending: self.pending.clone(),
            gauge: self.gauge,
        })
    }

    /// [`Self::recv`] bounded by `within` — what a batch worker flushes on.
    pub async fn recv_timeout(&mut self, within: Duration) -> Recv<T> {
        match tokio::time::timeout(within, self.rx.recv()).await {
            Ok(Some(item)) => {
                self.release();
                Recv::Item(item)
            }
            Ok(None) => Recv::Closed,
            Err(_elapsed) => Recv::Elapsed,
        }
    }

    /// Take an item if one is already buffered, releasing its reservation.
    ///
    /// Test-only: production consumers are loops that await. Kept here rather
    /// than reaching into the channel from a test so the release stays paired
    /// with the take.
    #[cfg(test)]
    pub(crate) fn try_recv(&mut self) -> Option<T> {
        let item = self.rx.try_recv().ok()?;
        self.release();
        Some(item)
    }

    fn release(&self) {
        (self.gauge)(release_counter(&self.pending, 1) as f64);
    }
}

/// An item still holding its slot in the queue's depth.
///
/// Released on drop, so a worker that panics mid-write cannot strand the
/// reservation and leave a drain waiting on a depth that will never reach zero.
pub struct Leased<T> {
    item: T,
    pending: Arc<AtomicUsize>,
    gauge: DepthGauge,
}

impl<T> std::ops::Deref for Leased<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.item
    }
}

impl<T> Drop for Leased<T> {
    fn drop(&mut self) {
        (self.gauge)(release_counter(&self.pending, 1) as f64);
    }
}

/// What a drain is allowed to treat as "finished".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainWitness {
    /// Wait for the worker tasks to exit, and nothing else.
    ///
    /// The correct witness whenever a worker decrements the counter *before*
    /// doing the item's work — the trace dispatcher (which then hands the item
    /// to a spawned task) and both persistence workers. For those, a depth of
    /// zero says the channel is empty, not that the writes have landed.
    TasksExit,
    /// Also finish as soon as the depth reaches zero.
    ///
    /// Sound only when the worker takes items with
    /// [`WorkerReceiver::recv_leased`] and holds the lease across its work,
    /// which makes zero mean "everything submitted has been written" — true of
    /// the audit writer and nothing else here. It is a liveness escape,
    /// not a shortcut: a background task still holding a producer clone that
    /// the runtime has not finished dropping would otherwise stall shutdown for
    /// the whole timeout over a queue that is already empty.
    QueueEmpty,
}

/// How a drain ended. Every variant is worth reporting; which of them is an
/// error, and what it says, is the caller's to decide.
#[derive(Debug, PartialEq, Eq)]
pub enum DrainOutcome {
    Drained,
    /// A worker task panicked. The queue is drained as far as it will ever be.
    WorkerPanicked,
    /// The deadline passed first. `lost` is what was still queued — for an
    /// audit trail, the number of actions an investigator will not find.
    TimedOut {
        lost: usize,
    },
}

/// The shutdown half of a [`BoundedWorker`]: the producer clone to release and
/// the counter to witness.
pub struct DrainHandle<T> {
    senders: Vec<mpsc::Sender<T>>,
    pending: Arc<AtomicUsize>,
}

/// How often [`DrainWitness::QueueEmpty`] re-checks the depth. Short, because
/// it runs once per process and only at shutdown.
const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(2);

impl<T> DrainHandle<T> {
    /// The depth at this instant — what a caller logs before draining.
    pub fn depth(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }

    /// Release the producer side and wait for `joins` to finish, bounded by
    /// `deadline`.
    ///
    /// Any worker still running when this returns is aborted. On the success
    /// paths that is a no-op; on the timeout path it is what stops a stuck
    /// worker from holding the process open past its last statement.
    pub async fn drain(
        self,
        mut joins: Vec<JoinHandle<()>>,
        witness: DrainWitness,
        deadline: Duration,
    ) -> DrainOutcome {
        drop(self.senders);
        if joins.is_empty() {
            return DrainOutcome::Drained;
        }

        let pending = self.pending.clone();
        let finished = tokio::time::timeout(deadline, async {
            let all_joined = async {
                let mut panicked = false;
                for join in joins.iter_mut() {
                    if join.await.is_err() {
                        panicked = true;
                    }
                }
                panicked
            };
            match witness {
                DrainWitness::TasksExit => all_joined.await,
                DrainWitness::QueueEmpty => {
                    tokio::select! {
                        panicked = all_joined => panicked,
                        () = async {
                            while pending.load(Ordering::Acquire) > 0 {
                                tokio::time::sleep(DRAIN_POLL_INTERVAL).await;
                            }
                        } => false,
                    }
                }
            }
        })
        .await;

        for join in &joins {
            join.abort();
        }

        match finished {
            Ok(false) => DrainOutcome::Drained,
            Ok(true) => DrainOutcome::WorkerPanicked,
            Err(_elapsed) => DrainOutcome::TimedOut {
                lost: self.pending.load(Ordering::Acquire),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    // The crate warns on `panic!` because production code should not have any.
    // These tests assert on the *variant* a rejection carries, and a wrong
    // variant has to fail loudly with the one it got — `expect` cannot say that.
    #![allow(clippy::panic)]

    use super::*;

    fn no_gauge(_: f64) {}

    #[tokio::test]
    async fn a_full_queue_sheds_immediately_and_hands_the_item_back() {
        let (worker, _rx) = BoundedWorker::<u32>::new(1, 1, no_gauge);
        worker.try_submit(1).expect("first fits");

        match worker.try_submit(2) {
            Err(Rejected::Full(item)) => assert_eq!(item, 2),
            other => panic!("expected Full(2), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_closed_queue_is_distinct_from_a_full_one() {
        let (worker, rx) = BoundedWorker::<u32>::new(1, 1, no_gauge);
        drop(rx);
        match worker.try_submit(1) {
            Err(Rejected::Closed(item)) => assert_eq!(item, 1),
            other => panic!("expected Closed(1), got {other:?}"),
        }
    }

    /// A rejection must leave nothing reserved, or the depth gauge drifts up by
    /// one per shed submission and never comes back down.
    #[tokio::test]
    async fn a_rejection_releases_its_reservation() {
        let (worker, _rx) = BoundedWorker::<u32>::new(1, 1, no_gauge);
        worker.try_submit(1).expect("first fits");
        assert_eq!(worker.depth(), 1);

        for _ in 0..10 {
            assert!(worker.try_submit(2).is_err());
        }
        assert_eq!(worker.depth(), 1, "shed submissions must not accumulate");
    }

    /// One stalled shard must not shed work the siblings could take.
    #[tokio::test]
    async fn a_full_shard_falls_through_to_its_siblings() {
        let (worker, mut receivers) = BoundedWorker::<u32>::new(2, 1, no_gauge);
        // Fill both shards, then free exactly one.
        worker.try_submit(1).expect("shard a");
        worker.try_submit(2).expect("shard b");
        assert!(worker.try_submit(3).is_err(), "both shards are full");

        receivers[0].recv().await.expect("an item");
        worker
            .try_submit(4)
            .expect("the freed shard must be found by fallthrough");
    }

    /// One dead worker among several is still a `Full`: the others can make
    /// progress, so the caller should shed, not give up on the queue.
    #[tokio::test]
    async fn one_dead_shard_among_several_reports_full_not_closed() {
        let (worker, mut receivers) = BoundedWorker::<u32>::new(2, 1, no_gauge);
        let dead = receivers.remove(0);
        drop(dead);
        // Fill the one live shard.
        worker.try_submit(1).expect("the live shard");
        match worker.try_submit(2) {
            Err(Rejected::Full(_)) => {}
            other => panic!("expected Full, got {other:?}"),
        }
    }

    /// The regression this primitive exists for. Under the previous ordering —
    /// increment *after* a successful send — a consumer could dequeue and run
    /// `fetch_sub` at zero, wrapping the counter to `usize::MAX` and publishing
    /// that as the depth gauge.
    ///
    /// The bound includes one slot per producer on top of what the shards hold:
    /// reserving before the send means a submission in flight is already
    /// counted, which is the deliberate over-count the module documents. A
    /// wrapped counter reads `usize::MAX`, so it clears any such bound by 18
    /// orders of magnitude and this still catches it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_depth_counter_never_exceeds_capacity_under_interleaving() {
        const SHARDS: usize = 2;
        const CAPACITY: usize = 4;
        const PRODUCERS: usize = 4;
        const PER_PRODUCER: usize = 500;

        let (worker, receivers) = BoundedWorker::<u32>::new(SHARDS, CAPACITY, no_gauge);
        let ceiling = SHARDS * CAPACITY + PRODUCERS;

        let consumers: Vec<_> = receivers
            .into_iter()
            .map(|mut rx| {
                tokio::spawn(async move {
                    let mut taken = 0usize;
                    while rx.recv().await.is_some() {
                        taken += 1;
                        tokio::task::yield_now().await;
                    }
                    taken
                })
            })
            .collect();

        let observer = {
            let worker = worker.clone();
            tokio::spawn(async move {
                let mut worst = 0usize;
                for _ in 0..20_000 {
                    worst = worst.max(worker.depth());
                    tokio::task::yield_now().await;
                }
                worst
            })
        };

        let producers: Vec<_> = (0..PRODUCERS)
            .map(|_| {
                let worker = worker.clone();
                tokio::spawn(async move {
                    let mut accepted = 0usize;
                    for i in 0..PER_PRODUCER {
                        if worker.try_submit(i as u32).is_ok() {
                            accepted += 1;
                        }
                        tokio::task::yield_now().await;
                    }
                    accepted
                })
            })
            .collect();

        let mut accepted = 0usize;
        for p in producers {
            accepted += p.await.expect("producer");
        }
        let worst = observer.await.expect("observer");

        drop(worker);
        let mut taken = 0usize;
        for c in consumers {
            taken += c.await.expect("consumer");
        }

        assert!(
            worst <= ceiling,
            "depth reached {worst}, above the {ceiling} the shards hold plus one \
             reservation per producer — the counter wrapped"
        );
        assert_eq!(
            accepted, taken,
            "every accepted item must be delivered once"
        );
    }

    #[tokio::test]
    async fn a_drain_waits_for_its_workers_to_exit() {
        let (worker, mut receivers) = BoundedWorker::<u32>::new(1, 4, no_gauge);
        let mut rx = receivers.pop().expect("one receiver");
        let drained = Arc::new(AtomicUsize::new(0));

        let join = {
            let drained = drained.clone();
            tokio::spawn(async move {
                while rx.recv().await.is_some() {
                    drained.fetch_add(1, Ordering::Release);
                }
            })
        };

        for i in 0..4 {
            worker.try_submit(i).expect("fits");
        }
        let handle = worker.drain_handle();
        drop(worker);

        let outcome = handle
            .drain(vec![join], DrainWitness::TasksExit, Duration::from_secs(5))
            .await;
        assert_eq!(outcome, DrainOutcome::Drained);
        assert_eq!(drained.load(Ordering::Acquire), 4);
    }

    /// `QueueEmpty` is the liveness escape: a producer clone the runtime has
    /// not dropped keeps the worker alive, so waiting only on the join would
    /// burn the whole timeout over a queue that is already empty.
    #[tokio::test]
    async fn queue_empty_finishes_while_a_producer_clone_is_still_held() {
        let (worker, mut receivers) = BoundedWorker::<u32>::new(1, 4, no_gauge);
        let mut rx = receivers.pop().expect("one receiver");
        let join = tokio::spawn(async move { while rx.recv().await.is_some() {} });

        worker.try_submit(1).expect("fits");
        let handle = worker.drain_handle();
        // `worker` stays alive on purpose — this is the lingering-sender case.

        let outcome = handle
            .drain(vec![join], DrainWitness::QueueEmpty, Duration::from_secs(5))
            .await;
        assert_eq!(outcome, DrainOutcome::Drained);
        assert_eq!(worker.depth(), 0);
    }

    #[tokio::test]
    async fn a_drain_that_times_out_reports_what_it_abandoned() {
        let (worker, receivers) = BoundedWorker::<u32>::new(1, 4, no_gauge);
        // Hold the receiver without consuming, so nothing ever drains.
        let _receivers = receivers;
        let join = tokio::spawn(async { std::future::pending::<()>().await });

        for i in 0..3 {
            worker.try_submit(i).expect("fits");
        }
        let handle = worker.drain_handle();
        drop(worker);

        let outcome = handle
            .drain(
                vec![join],
                DrainWitness::TasksExit,
                Duration::from_millis(50),
            )
            .await;
        assert_eq!(outcome, DrainOutcome::TimedOut { lost: 3 });
    }

    #[tokio::test]
    async fn a_panicking_worker_is_reported_rather_than_read_as_a_clean_drain() {
        let (worker, receivers) = BoundedWorker::<u32>::new(1, 4, no_gauge);
        let _receivers = receivers;
        let join = tokio::spawn(async { panic!("worker exploded") });

        let handle = worker.drain_handle();
        drop(worker);

        let outcome = handle
            .drain(vec![join], DrainWitness::TasksExit, Duration::from_secs(5))
            .await;
        assert_eq!(outcome, DrainOutcome::WorkerPanicked);
    }

    #[tokio::test]
    async fn blocking_submission_gives_up_after_its_timeout() {
        let (worker, _receivers) = BoundedWorker::<u32>::new(1, 1, no_gauge);
        worker.try_submit(1).expect("first fits");

        match worker.submit_blocking(2, Duration::from_millis(25)).await {
            Err(Blocked::TimedOut) => {}
            other => panic!("expected TimedOut, got {other:?}"),
        }
        assert_eq!(
            worker.depth(),
            1,
            "the abandoned send must release its slot"
        );
    }

    #[tokio::test]
    async fn a_disabled_queue_accepts_nothing_and_counts_nothing() {
        let worker = BoundedWorker::<u32>::disabled(no_gauge);
        assert!(worker.is_disabled());
        assert!(matches!(worker.try_submit(1), Err(Rejected::Closed(1))));
        assert_eq!(worker.depth(), 0);
    }
}
