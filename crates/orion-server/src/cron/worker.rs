//! The worker: claimed occurrences in, finished runs out.
//!
//! One attempt is nine steps, and the order of the first four is the whole
//! safety argument:
//!
//! 1. **Load one generation** and hold it for the attempt. The channel resolved
//!    from it, the guards applied from it and the engine that runs it are one
//!    build — the rule every other ingress follows.
//! 2. **Resolve the channel by stable id.** An occurrence outlives the name it
//!    was materialised under, and may outlive the channel entirely.
//! 3. **Take the singleton and go `running` in one transaction.** There is no
//!    instant at which an occurrence is executing without holding its key.
//! 4. **Write the trace before executing**, so a run that dies mid-flight is
//!    still visible as one that started.
//! 5. Guards, 6. execute against a heartbeat, 7. settle the trace,
//! 8. settle the occurrence, 9. release the key — conditionally, so a
//!    superseded holder cannot free a key it no longer owns.
//!
//! **Failure is not retried here.** A cron failure does not enter the trace DLQ
//! and is not re-attempted automatically: the next scheduled occurrence is the
//! natural retry, and a deterministically-failing job that retried itself would
//! spin. What *is* automatic is recovery from a crash — an expired claim is
//! re-claimed by the ordinary path, which is a different thing entirely.

use std::sync::Arc;

use chrono::Utc;
use tokio::sync::Semaphore;
use tracing::Instrument;

use crate::channel::registry::ChannelRuntimeConfig;
use crate::cron::metadata::{TriggerFacts, occurrence_metadata};
use crate::cron::status::CronStatus;
use crate::runtime::{RuntimeHandle, Shutdown};
use crate::storage::models::CronOccurrence;
use crate::storage::repositories::cron::{
    AttemptStart, ClaimRequest, CronRepository, Settlement, SingletonRequest, status,
};
use crate::storage::repositories::traces::TraceSink;

/// Everything the worker pool needs.
pub struct WorkerDeps {
    pub runtime: Arc<RuntimeHandle>,
    pub repo: Arc<dyn CronRepository>,
    pub trace_repo: Arc<dyn TraceSink>,
    pub persistence_queue: crate::queue::TracePersistenceQueue,
    pub global_trace_storage: crate::config::TraceStorageConfig,
    pub datalogic: Arc<dataflow_rs::datalogic_rs::Engine>,
    pub vars: Option<Arc<serde_json::Value>>,
    /// This node's identity: the claimant and the singleton holder.
    pub instance_id: String,
    pub status: Arc<CronStatus>,
    pub config: crate::config::CronConfig,
    /// Cap on the serialized result, shared with the trace queue.
    pub max_result_size_bytes: usize,
}

/// Claim, dispatch, repeat.
pub async fn run_worker(deps: Arc<WorkerDeps>, mut shutdown: Shutdown) {
    // Both the concurrency bound and the drain accounting. An attempt holds a
    // permit for its whole life, so "every permit is free" is exactly "no
    // attempt is in flight" — which is what shutdown waits on.
    let permits = Arc::new(Semaphore::new(deps.config.workers));
    let poll = deps.config.poll_interval();

    loop {
        if !shutdown.sleep(poll).await {
            break;
        }
        // Claim only what this node can actually start. Claiming more would
        // lease rows this node cannot run, and a peer with free capacity would
        // have to wait out the lease to get them.
        let free = permits.available_permits();
        if free == 0 {
            continue;
        }
        let limit = Ord::min(deps.config.claim_batch_size, free as i64);
        let claimed = match deps
            .repo
            .claim_due(ClaimRequest {
                claimant: &deps.instance_id,
                limit,
                lease_secs: deps.config.claim_lease_secs,
            })
            .await
        {
            Ok(claimed) => claimed,
            Err(e) => {
                // Fail closed: claim nothing, and say so. Guessing here is how
                // two nodes end up running one occurrence.
                deps.status.record_db_unavailable();
                crate::metrics::record_error("cron_claim");
                tracing::warn!(error = %e, "Cron claim failed; nothing claimed this tick");
                continue;
            }
        };
        deps.status.record_claim_ok();

        for occurrence in claimed {
            let Ok(permit) = permits.clone().try_acquire_owned() else {
                // Cannot happen — the batch was sized to the free permits — but
                // dropping the claim is better than blocking the loop, and the
                // lease expiry recovers it.
                tracing::warn!(
                    occurrence_id = %occurrence.id,
                    "No worker permit for a claimed occurrence; leaving its claim to expire"
                );
                break;
            };
            let deps = deps.clone();
            // Detached rather than held in a JoinSet: a supervisor restart of
            // this loop must not abort attempts that are mid-flight. The
            // semaphore is what shutdown drains against.
            tokio::spawn(async move {
                let _permit = permit;
                run_attempt(&deps, occurrence).await;
            });
        }
    }

    drain(&deps, permits).await;
}

/// Stop claiming, then wait for in-flight attempts.
///
/// Attempts still running at the deadline are dropped — cancelled at their next
/// await point — and their claims and singleton rows are **left to expire**.
/// Never released eagerly: the cancellation has not been observed to complete,
/// so freeing the key now would let a peer start alongside work that may still
/// be finishing a connector call. A peer recovers them after the lease, which
/// is the safety window's whole purpose.
async fn drain(deps: &WorkerDeps, permits: Arc<Semaphore>) {
    let total = deps.config.workers as u32;
    let deadline = std::time::Duration::from_secs(deps.config.shutdown_timeout_secs);
    match tokio::time::timeout(deadline, permits.acquire_many_owned(total)).await {
        Ok(_) => tracing::info!("Cron workers drained"),
        Err(_) => tracing::warn!(
            deadline_secs = deps.config.shutdown_timeout_secs,
            "Cron attempts did not finish within the shutdown deadline; their claims \
             will expire and a peer will retry them"
        ),
    }
}

/// Why an attempt ended before the engine ran, when it did.
enum Abandoned {
    /// Another node owns this occurrence now. Write nothing.
    Lost,
    /// The occurrence is settled and there is nothing more to do.
    Settled,
}

async fn run_attempt(deps: &WorkerDeps, occurrence: CronOccurrence) {
    // One span per attempt, carrying the identity every line inside it needs.
    //
    // The fields are the design's observability contract, and they are set
    // *here* rather than repeated at each call site so a log line added later
    // cannot forget them. `fencing_token` is recorded rather than declared
    // because it is not known until the singleton is acquired — `Empty` is what
    // lets a later `Span::record` fill it in.
    let span = tracing::info_span!(
        "cron_attempt",
        occurrence_id = %occurrence.id,
        channel_id = %occurrence.channel_id,
        scheduled_for = %occurrence.scheduled_for,
        attempt = occurrence.attempt,
        instance_id = %deps.instance_id,
        fencing_token = tracing::field::Empty,
        singleton_slot = tracing::field::Empty,
    );
    run_attempt_inner(deps, occurrence).instrument(span).await
}

async fn run_attempt_inner(deps: &WorkerDeps, occurrence: CronOccurrence) {
    match attempt(deps, &occurrence).await {
        Ok(()) | Err(Abandoned::Settled) => {}
        Err(Abandoned::Lost) => tracing::info!(
            occurrence_id = %occurrence.id,
            "Cron occurrence is owned by another node; abandoning this attempt"
        ),
    }
}

async fn attempt(deps: &WorkerDeps, occurrence: &CronOccurrence) -> Result<(), Abandoned> {
    // 1. One generation, held for the whole attempt.
    let generation = deps.runtime.load();

    // 2. By stable id: a channel renamed between materialisation and execution
    // is the same channel, and a name lookup would fail as though it had been
    // deleted.
    let Some(runtime) = generation
        .channels
        .cron_by_channel_id(&occurrence.channel_id)
    else {
        // Archived, deleted, or quarantined since this was materialised. A
        // visible failure, not a silent drop — and deliberately not a retry:
        // the definition, not the run, is what needs fixing.
        settle_failed(
            deps,
            occurrence,
            None,
            "channel_unavailable: the cron channel is no longer active on this node \
             (archived, deleted, or quarantined since this occurrence was scheduled)",
        )
        .await;
        return Err(Abandoned::Settled);
    };
    let descriptor = runtime
        .cron
        .as_ref()
        .expect("cron_by_channel_id only returns cron channels")
        .clone();

    // 3. The singleton and the status change, in one transaction.
    let singleton = matches!(
        descriptor.concurrency,
        crate::channel::ConcurrencyPolicy::Forbid
    )
    .then(|| SingletonRequest {
        key: descriptor.singleton_key.as_str(),
        holder: deps.instance_id.as_str(),
        lease_secs: 0, // replaced below, with the claim's lease
        slots: descriptor.singleton_slots,
    });

    let timeout_ms = crate::channel::guards::effective_timeout_ms(
        &Some(runtime.clone()),
        Some(deps.config.default_timeout_ms),
        // A default, not a ceiling: a cron worker occupies only its own slot,
        // unlike a Kafka consumer blocking a poll loop or an async worker
        // holding one of a fixed pool. A channel that genuinely needs six hours
        // may say so.
        None,
    )
    .unwrap_or(deps.config.default_timeout_ms);

    // The claim and the singleton are held for one lease at a time, renewed
    // every heartbeat, however long the channel's timeout. The lease is what
    // a dead node leaves behind: sized to the timeout, a crashed or drained
    // node held its slots for the whole of it (#352, forty minutes for a
    // 2400-second channel). A live attempt that cannot renew stops itself
    // before its lease can end (`with_heartbeat`), so a peer never starts
    // beside it.
    let lease_secs = deps.config.claim_lease_secs;
    let singleton = singleton.map(|s| SingletonRequest { lease_secs, ..s });

    // Before the call, so it is no later than the database's own `now` the
    // lease is written from: every deadline measured from it is early.
    let acquired = tokio::time::Instant::now();
    let start = match deps
        .repo
        .start_attempt(
            occurrence,
            &deps.instance_id,
            runtime.channel.version,
            singleton.clone(),
            lease_secs,
        )
        .await
    {
        Ok(start) => start,
        Err(e) => {
            // The claim stands and its lease will expire, so this occurrence is
            // retried rather than lost.
            deps.status.record_db_unavailable();
            crate::metrics::record_error("cron_start");
            tracing::warn!(
                occurrence_id = %occurrence.id,
                error = %e,
                "Could not start a cron attempt; its claim will expire and be retried"
            );
            return Err(Abandoned::Lost);
        }
    };

    let held = match start {
        AttemptStart::Started { held } => {
            // Now that the key is held, the span can name the generation it is
            // held under — which is what makes two nodes' logs about one key
            // orderable after the fact.
            if let Some(held) = held {
                tracing::Span::current().record("fencing_token", held.fencing_token);
                tracing::Span::current().record("singleton_slot", held.slot);
            }
            held
        }
        AttemptStart::Lost => return Err(Abandoned::Lost),
        AttemptStart::SingletonBusy => {
            // `forbid` working as documented: recorded as a visible skip, not
            // dropped and not deferred. The next scheduled occurrence is the
            // next chance.
            crate::metrics::record_cron_singleton_contention();
            crate::metrics::record_cron_occurrence(status::SKIPPED_SINGLETON);
            tracing::info!(
                occurrence_id = %occurrence.id,
                channel_id = %occurrence.channel_id,
                singleton_key = %descriptor.singleton_key,
                "Cron occurrence skipped: its singleton key is held by a running attempt"
            );
            let _ = deps
                .repo
                .settle_skipped(
                    &occurrence.id,
                    &deps.instance_id,
                    &if descriptor.singleton_slots > 1 {
                        format!(
                            "all {} slots of singleton key '{}' were held by running \
                             occurrences (concurrency.policy = \"forbid\")",
                            descriptor.singleton_slots, descriptor.singleton_key
                        )
                    } else {
                        format!(
                            "singleton key '{}' was held by another running occurrence \
                             (concurrency.policy = \"forbid\")",
                            descriptor.singleton_key
                        )
                    },
                )
                .await;
            return Err(Abandoned::Settled);
        }
    };

    let outcome = execute(
        deps,
        &generation,
        occurrence,
        &runtime,
        &descriptor,
        timeout_ms,
        held,
        Lease {
            secs: lease_secs,
            acquired,
        },
    )
    .await;

    // 9. Release the key, conditionally. A superseded holder matches nothing
    // and leaves the new owner's row alone.
    if let (Some(held), Some(singleton)) = (held, singleton.as_ref()) {
        match deps
            .repo
            .release_singleton(singleton.key, &occurrence.id, held)
            .await
        {
            Ok(true) => {}
            Ok(false) => tracing::info!(
                occurrence_id = %occurrence.id,
                singleton_key = %singleton.key,
                "Singleton was already taken over; leaving the new holder's row alone"
            ),
            Err(e) => tracing::warn!(
                occurrence_id = %occurrence.id,
                error = %e,
                "Could not release a cron singleton; it will expire with its lease"
            ),
        }
    }
    outcome
}

/// The lease an attempt holds its claim and singleton under.
#[derive(Clone, Copy)]
struct Lease {
    secs: u64,
    /// No later than the moment the database wrote it.
    acquired: tokio::time::Instant,
}

/// Steps 4 through 8: trace, guards, execute, settle.
///
/// `generation` is the one step 1 loaded, passed rather than re-loaded: one
/// attempt, one generation, from the channel resolution through to the engine
/// call. That is the rule every ingress follows, and stating it in the
/// signature is what stops a future edit quietly re-reading the `ArcSwap` here.
#[allow(clippy::too_many_arguments)]
async fn execute(
    deps: &WorkerDeps,
    generation: &Arc<crate::runtime::RuntimeGeneration>,
    occurrence: &CronOccurrence,
    runtime: &Arc<ChannelRuntimeConfig>,
    descriptor: &Arc<crate::channel::CronDescriptor>,
    timeout_ms: u64,
    held: Option<crate::storage::repositories::cron::HeldSlot>,
    lease: Lease,
) -> Result<(), Abandoned> {
    let channel = runtime.channel.name.as_str();
    let started_at = Utc::now().naive_utc();

    // The lag: how late this occurrence started. The scheduler's core service
    // signal, recorded whatever the run then does.
    let lag = (started_at - occurrence.scheduled_for).num_milliseconds() as f64 / 1000.0;
    crate::metrics::record_cron_schedule_lag(lag.max(0.0));

    let payload = descriptor.payload.clone();
    let metadata = occurrence_metadata(
        TriggerFacts {
            channel_name: channel,
            trigger_type: &occurrence.trigger,
            occurrence_id: &occurrence.id,
            scheduled_for: occurrence.scheduled_for,
            started_at,
            timezone: descriptor.timezone.name(),
            attempt: occurrence.attempt,
            singleton_key: held.map(|_| descriptor.singleton_key.as_str()),
            singleton_slot: held.map(|h| h.slot),
        },
        deps.vars.as_deref(),
    );

    // The `/async` trace contract: `off` is upgraded to `sync` because the
    // occurrence is the only thing that observes this run, and a run nobody can
    // debug is worth less than the storage it saves. Sampling is likewise
    // forced — a scheduled job runs once, so sampling it is all-or-nothing
    // rather than statistical.
    let effective_trace = runtime.trace_storage.for_async_submission();

    // 4. The trace row exists before the workflow starts, so a run that dies
    // mid-flight is visible as one that started rather than one that never did.
    let input_json = serde_json::to_string(&payload).ok();
    let trace = match deps
        .trace_repo
        .create_pending(
            channel,
            Some(&occurrence.channel_id),
            crate::storage::models::TRACE_MODE_CRON,
            input_json.as_deref(),
            None,
        )
        .await
    {
        Ok(trace) => Some(trace),
        Err(e) => {
            // Defer rather than run: without the row there is nothing to read
            // the outcome out of, and the occurrence is better retried when the
            // database recovers.
            tracing::warn!(
                occurrence_id = %occurrence.id,
                error = %e,
                "Could not create a cron trace; returning the occurrence to pending"
            );
            let _ = deps
                .repo
                .release_claim(&occurrence.id, &deps.instance_id)
                .await;
            return Err(Abandoned::Settled);
        }
    };
    let trace_id = trace.as_ref().map(|t| t.id.clone());
    if let Some(ref id) = trace_id {
        let _ = deps.repo.set_trace_id(&occurrence.id, id).await;
        let _ = deps
            .trace_repo
            .update_status(id, crate::storage::models::TRACE_STATUS_RUNNING, None)
            .await;
    }

    // 5. `Transport::Cron`'s row of the guard matrix — validation and
    // backpressure, and nothing caller-shaped.
    let header_lookup = |_: &str| None;
    let admission = crate::channel::guards::admit(crate::channel::guards::GuardRequest {
        transport: crate::channel::guards::Transport::Cron,
        channel,
        runtime: &Some(runtime.clone()),
        data: &payload,
        metadata: &metadata,
        datalogic: &deps.datalogic,
        origin: None,
        // The bucket key if a channel somehow declared a rate limit — it
        // cannot, validation refuses it — so this is the stable identity rather
        // than a caller.
        caller_identity: &occurrence.channel_id,
        header: &header_lookup,
        auth_backoff: None,
        raw_body: None,
        dedup_key_fallback: None,
        dedup_owner: None,
        default_timeout_ms: Some(timeout_ms),
        max_timeout_ms: None,
        oauth: None,
    })
    .await;

    let admission = match admission {
        Ok(admission) => admission,
        Err(e) => {
            // Two dispositions, drawn the way the Kafka consumer draws them.
            // Backpressure is transient — the node is busy — so the occurrence
            // goes back to `pending` and is claimed again. Everything else is a
            // property of the definition and the payload, which are fixed, so
            // it would fail identically on every retry.
            if matches!(e, crate::errors::OrionError::ServiceUnavailable { .. }) {
                tracing::debug!(
                    occurrence_id = %occurrence.id,
                    "Cron occurrence deferred by backpressure; returning it to pending"
                );
                let _ = deps
                    .repo
                    .release_claim(&occurrence.id, &deps.instance_id)
                    .await;
                if let Some(ref id) = trace_id {
                    let _ = deps
                        .trace_repo
                        .update_status(
                            id,
                            crate::storage::models::TRACE_STATUS_PENDING,
                            Some("deferred by backpressure"),
                        )
                        .await;
                }
                return Err(Abandoned::Settled);
            }
            let reason = format!("guard_refused: {e}");
            if let Some(ref id) = trace_id {
                let _ = deps
                    .trace_repo
                    .update_status(
                        id,
                        crate::storage::models::TRACE_STATUS_FAILED,
                        Some(&reason),
                    )
                    .await;
            }
            settle_failed(deps, occurrence, trace_id.as_deref(), &reason).await;
            return Err(Abandoned::Settled);
        }
    };
    let _backpressure_permit = admission.backpressure_permit;

    let capture = effective_trace
        .task_details
        .then_some(crate::engine::TraceCapture {
            max_snapshot_bytes: deps.max_result_size_bytes,
        });

    // 6. The engine, raced against the heartbeat.
    //
    // The rollout bucket is derived from `(channel_id, scheduled_for)` rather
    // than left `None`, and that is deliberate: `None` admits *every* rollout
    // version, so a schedule under a 90/10 canary would run both versions on
    // every occurrence. Hashing the occurrence's own identity gives one version
    // per occurrence, and the same one on every retry of it.
    let bucket = crate::engine::utils::rollout_bucket_for_identity(Some(&format!(
        "{}:{}",
        occurrence.channel_id, occurrence.scheduled_for
    )));
    let engine_start = std::time::Instant::now();
    let run = crate::engine::execute_admitted(
        &generation.engine,
        channel,
        &payload,
        &metadata,
        crate::engine::ExecOpts {
            timeout_ms: Some(timeout_ms),
            capture,
            routing_bucket: Some(bucket),
            profile: None,
        },
    );

    let execution = match with_heartbeat(deps, occurrence, held, lease, run).await {
        Some(execution) => execution,
        None => {
            // The claim was lost: another node owns this occurrence now, or
            // an operator cancelled it, and the engine future was dropped.
            // Write nothing — the record belongs to whoever took it.
            crate::metrics::record_cron_lease_renewal_failure();
            deps.status.record_renewal_failure();
            tracing::warn!(
                occurrence_id = %occurrence.id,
                channel_id = %occurrence.channel_id,
                "Cron attempt lost its claim and was cancelled mid-run: a peer took it over, \
                 or an operator cancelled it"
            );
            return Err(Abandoned::Lost);
        }
    };

    let duration = engine_start.elapsed();
    crate::metrics::record_cron_execution_duration(channel, duration.as_secs_f64());
    crate::metrics::record_message(channel, execution.outcome.status_label());
    crate::metrics::record_message_duration(channel, execution.duration.as_secs_f64());

    // 7. The trace, then 8. the occurrence.
    let task_trace_json = crate::engine::utils::serialize_task_trace_capped(
        execution.task_trace.as_ref(),
        deps.max_result_size_bytes,
        &occurrence.id,
    );
    let has_errors = !execution.outcome.is_ok();
    let result_json =
        serde_json::to_string(execution.message.data()).unwrap_or_else(|_| "{}".to_string());

    if let Some(ref id) = trace_id
        && crate::queue::trace_record::TracePlan::decide(&effective_trace, has_errors).persists()
    {
        let _ = deps
            .trace_repo
            .set_result(
                id,
                &result_json,
                duration.as_secs_f64() * 1000.0,
                task_trace_json.as_deref(),
            )
            .await;
    }

    let (occurrence_status, error_message) = match execution.outcome {
        crate::engine::RunOutcome::Ok => (status::COMPLETED, None),
        crate::engine::RunOutcome::WorkflowErrors(summary) => (status::FAILED, Some(summary)),
        crate::engine::RunOutcome::Timeout(ms) => {
            (status::FAILED, Some(format!("timed out after {ms}ms")))
        }
        crate::engine::RunOutcome::EngineError(e) => (status::FAILED, Some(e.to_string())),
    };
    if let Some(ref id) = trace_id {
        let _ = deps
            .trace_repo
            .update_status(
                id,
                if occurrence_status == status::COMPLETED {
                    crate::storage::models::TRACE_STATUS_COMPLETED
                } else {
                    crate::storage::models::TRACE_STATUS_FAILED
                },
                error_message.as_deref(),
            )
            .await;
    }

    crate::metrics::record_cron_occurrence(occurrence_status);
    let settled = deps
        .repo
        .settle(Settlement {
            occurrence_id: &occurrence.id,
            claimant: &deps.instance_id,
            status: occurrence_status,
            error_message: error_message.as_deref(),
            trace_id: trace_id.as_deref(),
        })
        .await;
    match settled {
        Ok(true) => Ok(()),
        // Lost the occurrence between the run and the write. The work happened;
        // the record belongs to whoever owns it now.
        Ok(false) => Err(Abandoned::Lost),
        Err(e) => {
            crate::metrics::record_error("cron_settle");
            tracing::error!(
                occurrence_id = %occurrence.id,
                error = %e,
                "Cron occurrence ran but could not be settled; its lease will expire and \
                 it may be attempted again — scheduled side effects must be idempotent"
            );
            Err(Abandoned::Settled)
        }
    }
}

/// Run `work` while renewing the lease, cancelling it if ownership is lost.
///
/// `None` means `work` was dropped: ownership was lost, or the lease could not
/// be renewed in time to be sure of it. Dropping the future cancels it at its
/// next await point, which does *not* unwind a connector call that has
/// already been sent, and is why the exactly-once caveat exists.
async fn with_heartbeat<F, T>(
    deps: &WorkerDeps,
    occurrence: &CronOccurrence,
    held: Option<crate::storage::repositories::cron::HeldSlot>,
    lease: Lease,
    work: F,
) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    let beat = std::time::Duration::from_secs(deps.config.heartbeat_interval_secs);
    let outcome = heartbeat(
        beat,
        fence_after(lease.secs, deps.config.heartbeat_interval_secs),
        lease.acquired,
        || {
            deps.repo
                .renew(&occurrence.id, &deps.instance_id, held, lease.secs)
        },
        |e| {
            crate::metrics::record_error("cron_renew");
            tracing::warn!(
                occurrence_id = %occurrence.id,
                error = %e,
                "Cron lease renewal failed; retrying on the next beat"
            );
        },
        work,
    )
    .await;
    match outcome {
        Beat::Done(result) => Some(result),
        Beat::Lost => None,
        Beat::Fenced => {
            tracing::warn!(
                occurrence_id = %occurrence.id,
                lease_secs = lease.secs,
                "Cron attempt could not renew its lease in time and stopped itself \
                 before the lease could run out"
            );
            None
        }
    }
}

/// How an attempt under [`heartbeat`] ended.
#[derive(Debug, PartialEq, Eq)]
enum Beat<T> {
    Done(T),
    /// A renewal matched nothing: a peer took the occurrence over, or an
    /// operator cancelled it.
    Lost,
    /// No renewal succeeded for [`fence_after`]. The lease may still be held,
    /// but there is no longer room to be sure of it.
    Fenced,
}

/// How long an attempt may go without a successful renewal before it stops.
///
/// Short of the lease by a margin, so the work is dropped while the lease is
/// still certainly held. The margin is one heartbeat, or half the gap between
/// beat and lease when that is smaller, so every configuration validation
/// accepts (`heartbeat < lease`) still renews before its deadline.
fn fence_after(lease_secs: u64, heartbeat_secs: u64) -> std::time::Duration {
    let lease = std::time::Duration::from_secs(lease_secs);
    let beat = std::time::Duration::from_secs(heartbeat_secs);
    let margin = Ord::min(beat, lease.saturating_sub(beat) / 2);
    lease.saturating_sub(margin)
}

/// Race `work` against the lease: renew every `beat`, stop on a renewal that
/// matches nothing, and stop once `fence_after` has passed since the last
/// renewal known to have landed.
///
/// A failed renewal *call* is not lost ownership, and one blip must not
/// abandon healthy work, so it is retried on the next beat. What bounds the
/// retries is the lease: the lease is short (#352), so a node whose database
/// is unreachable must stop its own work before a peer is entitled to start
/// it. Each renewal is timed from when it was sent, which is no later than
/// when the database wrote it, so every deadline here is early, never late.
async fn heartbeat<F, T, R, RF>(
    beat: std::time::Duration,
    fence_after: std::time::Duration,
    acquired: tokio::time::Instant,
    mut renew: R,
    on_error: impl Fn(&crate::errors::OrionError),
    work: F,
) -> Beat<T>
where
    F: std::future::Future<Output = T>,
    R: FnMut() -> RF,
    RF: std::future::Future<Output = Result<bool, crate::errors::OrionError>>,
{
    // The first tick is immediate: the steps between acquiring the lease and
    // starting the work (the trace row, the guards) renewed nothing.
    let mut ticker = tokio::time::interval(beat);
    let mut renewed = acquired;
    tokio::pin!(work);
    loop {
        let deadline = renewed + fence_after;
        // In this order, so a race is decided the same way every time: work
        // that has finished is done, and past the deadline nothing more runs,
        // even a renewal that might have landed.
        tokio::select! {
            biased;
            result = &mut work => return Beat::Done(result),
            _ = tokio::time::sleep_until(deadline) => return Beat::Fenced,
            _ = ticker.tick() => {
                let sent = tokio::time::Instant::now();
                // Bounded by the deadline: a renewal hung on an unreachable
                // database must not hold the work past it.
                match tokio::time::timeout_at(deadline, renew()).await {
                    Ok(Ok(true)) => renewed = sent,
                    Ok(Ok(false)) => return Beat::Lost,
                    Ok(Err(e)) => on_error(&e),
                    Err(_) => return Beat::Fenced,
                }
            }
        }
    }
}

async fn settle_failed(
    deps: &WorkerDeps,
    occurrence: &CronOccurrence,
    trace_id: Option<&str>,
    reason: &str,
) {
    crate::metrics::record_cron_occurrence(status::FAILED);
    if let Err(e) = deps
        .repo
        .settle(Settlement {
            occurrence_id: &occurrence.id,
            claimant: &deps.instance_id,
            status: status::FAILED,
            error_message: Some(reason),
            trace_id,
        })
        .await
    {
        tracing::warn!(
            occurrence_id = %occurrence.id,
            error = %e,
            "Could not record a failed cron occurrence"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const BEAT: Duration = Duration::from_secs(15);
    const LEASE_SECS: u64 = 60;

    fn fence() -> Duration {
        fence_after(LEASE_SECS, BEAT.as_secs())
    }

    /// Ten minutes of work: far longer than one lease.
    async fn long_work() -> &'static str {
        tokio::time::sleep(Duration::from_secs(600)).await;
        "done"
    }

    #[test]
    fn the_fence_leaves_a_margin_and_room_for_a_renewal() {
        assert_eq!(fence_after(60, 15), Duration::from_secs(45));
        assert_eq!(fence_after(30, 1), Duration::from_secs(29));
        // Every configuration validation accepts renews before its deadline,
        // and the deadline is before the lease ends.
        for lease in 2..=120 {
            for beat in 1..lease {
                let fence = fence_after(lease, beat);
                assert!(fence > Duration::from_secs(beat), "{lease}/{beat}");
                assert!(fence < Duration::from_secs(lease), "{lease}/{beat}");
            }
        }
    }

    /// Renewal keeps work alive for as long as it runs, far past one lease.
    #[tokio::test(start_paused = true)]
    async fn renewals_hold_the_lease_for_as_long_as_the_work_runs() {
        let outcome = heartbeat(
            BEAT,
            fence(),
            tokio::time::Instant::now(),
            || async { Ok(true) },
            |_| {},
            long_work(),
        )
        .await;
        assert_eq!(outcome, Beat::Done("done"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_renewal_that_matches_nothing_stops_the_work() {
        let outcome = heartbeat(
            BEAT,
            fence(),
            tokio::time::Instant::now(),
            || async { Ok(false) },
            |_| {},
            long_work(),
        )
        .await;
        assert_eq!(outcome, Beat::Lost);
    }

    /// A node that cannot reach its database stops its own work before its
    /// lease ends, so a peer that takes the occurrence over after the lease
    /// never runs beside it.
    #[tokio::test(start_paused = true)]
    async fn failing_renewals_stop_the_work_before_the_lease_ends() {
        let started = tokio::time::Instant::now();
        let errors = std::sync::atomic::AtomicUsize::new(0);
        let outcome = heartbeat(
            BEAT,
            fence(),
            started,
            || async { Err(crate::errors::OrionError::Conflict("db down".into())) },
            |_| {
                errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            },
            long_work(),
        )
        .await;
        assert_eq!(outcome, Beat::Fenced);
        assert!(started.elapsed() < Duration::from_secs(LEASE_SECS));
        assert!(
            errors.load(std::sync::atomic::Ordering::Relaxed) >= 2,
            "a failed call is retried, not taken as lost ownership"
        );
    }

    /// A renewal that never answers cannot hold the work past the deadline.
    #[tokio::test(start_paused = true)]
    async fn a_hung_renewal_is_bounded_by_the_deadline() {
        let started = tokio::time::Instant::now();
        let outcome = heartbeat(
            BEAT,
            fence(),
            started,
            std::future::pending::<Result<bool, crate::errors::OrionError>>,
            |_| {},
            long_work(),
        )
        .await;
        assert_eq!(outcome, Beat::Fenced);
        assert!(started.elapsed() <= fence());
    }

    /// The steps before the work (the trace row, the guards) are not renewed
    /// across. If they took the whole margin, the work does not run, however
    /// a renewal would have gone: past the deadline the lease may be gone.
    #[tokio::test(start_paused = true)]
    async fn a_lease_already_past_its_deadline_never_runs_the_work() {
        let acquired = tokio::time::Instant::now();
        tokio::time::advance(Duration::from_secs(50)).await;
        let renewals = std::sync::atomic::AtomicUsize::new(0);
        let outcome = heartbeat(
            BEAT,
            fence(),
            acquired,
            || {
                renewals.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                async { Ok(true) }
            },
            |_| {},
            long_work(),
        )
        .await;
        assert_eq!(outcome, Beat::Fenced);
        assert_eq!(renewals.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(
            acquired.elapsed(),
            Duration::from_secs(50),
            "no further wait"
        );
    }
}
