//! The reconciler: stored schedules in, durable occurrences out.
//!
//! One pass does three things per active cron channel — decide where the cursor
//! should be, materialise what is due, and move the cursor — and every one of
//! them is idempotent, because the pass can die between any two of them:
//!
//! | Dies… | What happens next pass |
//! |---|---|
//! | before the insert | the cursor is unmoved, so the same instants are planned and inserted |
//! | after the insert, before the cursor moves | the insert loses to the unique key, then the cursor moves |
//! | with a peer running the same pass | both insert the same rows, one wins the cursor compare-and-set, the loser re-plans and finds nothing |
//!
//! That is why there is no lease around any of this. `JobLeaseGate` is used, but
//! only to stop every node in a cluster doing identical work every second — if
//! it were removed tomorrow the ledger would be unchanged.
//!
//! The pass is also *pure* wherever it can be:
//! [`CronDescriptor::plan_pass`](crate::channel::CronDescriptor::plan_pass)
//! decides what to materialise with no database and no clock of its own, so the
//! misfire policies are exhaustively testable and this file is left with the
//! writes.

use std::sync::Arc;

use chrono::NaiveDateTime;

use crate::channel::CronDescriptor;
use crate::cron::status::CronStatus;
use crate::runtime::{RuntimeHandle, Shutdown};
use crate::storage::repositories::cron::{
    CronRepository, NewOccurrence, ScheduleCursor, status, trigger,
};

/// Everything one reconciler needs.
pub struct ReconcileDeps {
    pub runtime: Arc<RuntimeHandle>,
    pub repo: Arc<dyn CronRepository>,
    pub status: Arc<CronStatus>,
    pub config: crate::config::CronConfig,
    /// Cluster single-flight. `None` on a single node — and never load-bearing:
    /// see the module docs.
    pub lease_gate: Option<Arc<crate::cluster::JobLeaseGate>>,
}

/// The job name the cluster lease and the health gauge use.
pub const JOB: &str = "cron_reconcile";

/// Poll, plan, write, repeat.
pub async fn run_reconcile(deps: Arc<ReconcileDeps>, mut shutdown: Shutdown) {
    let poll = deps.config.poll_interval();
    // TTL comfortably past one tick, matching the retention jobs: the incumbent
    // renews long before it expires, and a dead holder's lease frees up one TTL
    // later.
    let lease_ttl = deps.config.poll_interval_ms / 1000 + 30;

    loop {
        // The sleep races shutdown, so stopping a node costs at most one tick
        // rather than one whole interval.
        if !shutdown.sleep(poll).await {
            return;
        }
        if let Some(ref gate) = deps.lease_gate
            && !gate.try_acquire(JOB, lease_ttl).await
        {
            continue;
        }
        if let Err(e) = reconcile_once(&deps).await {
            // Swallowed and retried, like every other periodic job: a database
            // blip must not kill the loop. What makes a *sustained* failure
            // visible is the status struct, not this log line.
            deps.status.record_db_unavailable();
            crate::metrics::record_error("cron_reconcile");
            tracing::warn!(error = %e, "Cron reconciliation pass failed; retrying next tick");
            continue;
        }
        deps.status.record_reconcile_ok();
        crate::metrics::record_job_success(JOB);
    }
}

/// One pass. Split out so tests can drive it directly instead of racing a loop.
pub async fn reconcile_once(deps: &ReconcileDeps) -> Result<(), crate::errors::OrionError> {
    let started = std::time::Instant::now();

    // One generation for the whole pass: the schedules planned against and the
    // engine that will run what they produce come from the same build.
    let generation = deps.runtime.load();
    let descriptors = generation.channels.cron_descriptors();
    let states = deps.repo.schedule_states().await?;

    // The idle path. A node with no cron channels and no cursors still runs
    // this loop, so it must cost one indexed read and nothing else.
    if descriptors.is_empty() && states.iter().all(|s| s.paused_at.is_some()) {
        deps.status.set_oldest_pending_secs(None);
        return Ok(());
    }

    // The database's clock, read once. Every instant in this pass is compared
    // against this one value, so two channels planned in the same pass agree on
    // what "now" was — and a node with a skewed clock plans the same pass as
    // its peers.
    let now = deps.repo.db_now().await?;

    // Cursors whose channel is no longer active: archived, deleted, or
    // quarantined. Stop materialising; keep the history.
    let active_ids: std::collections::HashSet<&str> =
        descriptors.iter().map(|d| d.channel_id.as_str()).collect();
    for state in &states {
        if !active_ids.contains(state.channel_id.as_str()) && state.paused_at.is_none() {
            tracing::info!(
                channel_id = %state.channel_id,
                "Cron channel is no longer active; pausing its schedule"
            );
            deps.repo.pause_cursor(&state.channel_id).await?;
        }
    }

    for descriptor in &descriptors {
        if let Err(e) =
            reconcile_channel(deps.repo.as_ref(), &deps.config, descriptor, &states, now).await
        {
            // One channel's failure must not stop the others: a schedule whose
            // cursor row is wedged should not stop every other schedule on the
            // instance from firing.
            crate::metrics::record_error("cron_reconcile_channel");
            tracing::warn!(
                channel_id = %descriptor.channel_id,
                error = %e,
                "Reconciling one cron channel failed; other schedules are unaffected"
            );
        }
    }

    deps.status
        .set_oldest_pending_secs(deps.repo.oldest_pending_age_secs().await?);
    crate::metrics::set_cron_pending_occurrences(deps.repo.pending_count(None).await? as f64);
    crate::metrics::record_cron_reconcile_duration(started.elapsed().as_secs_f64());
    Ok(())
}

async fn reconcile_channel(
    repo: &dyn CronRepository,
    config: &crate::config::CronConfig,
    descriptor: &CronDescriptor,
    states: &[crate::storage::models::CronScheduleState],
    now: NaiveDateTime,
) -> Result<(), crate::errors::OrionError> {
    let state = states
        .iter()
        .find(|s| s.channel_id == descriptor.channel_id);

    // A cursor is (re)initialised in three cases, and all three mean the same
    // thing: there is no position in time this schedule can meaningfully resume
    // from, so it starts *now*.
    //
    // Never from the channel's creation date. A schedule activated today, or
    // whose expression changed today, has not been "missing" runs since
    // whenever the channel was written — inventing them would flood the engine
    // with work nobody asked for, which is the failure `max_catch_up` exists to
    // bound and this avoids entirely.
    let reset_reason = match state {
        None => Some("new"),
        Some(state) if state.config_hash != descriptor.config_hash => Some("schedule changed"),
        Some(state) if state.paused_at.is_some() => Some("reactivated"),
        Some(_) => None,
    };
    if let Some(reason) = reset_reason {
        let Some(next) = descriptor.next_after(now) else {
            tracing::warn!(
                channel_id = %descriptor.channel_id,
                schedule = %descriptor.expression,
                "Cron schedule has no future occurrence; leaving its cursor unset"
            );
            return Ok(());
        };
        tracing::info!(
            channel_id = %descriptor.channel_id,
            reason,
            next_fire_at = %next,
            "Initialising cron cursor"
        );
        repo.upsert_cursor(ScheduleCursor {
            channel_id: &descriptor.channel_id,
            channel_version: descriptor.version,
            config_hash: &descriptor.config_hash,
            next_fire_at: next,
        })
        .await?;
        return Ok(());
    }

    let state = state.expect("a `None` state took the reset branch");
    if state.next_fire_at > now {
        return Ok(());
    }

    let plan = descriptor.plan_pass(
        state.next_fire_at,
        now,
        config.misfire_grace(),
        config.max_catch_up,
    );

    // `forbid` is decided when an occurrence comes due, not when a worker
    // gets round to it. With every slot held, a `pending` row could only ever
    // be claimed to be skipped, and only a worker with a free permit claims —
    // on a node whose slots are all busy, that is the spare workers alone, one
    // claim per poll each. A schedule faster than they drain would grow a
    // backlog of rows that nothing can run and the cleanup (rightly) never
    // removes, until the node went idle (#377). So the row is written already
    // settled. A slot freed between this check and a claim changes nothing
    // that matters: that tick found the key busy when it was due, which is
    // what `forbid` skips on.
    let slots_full = if descriptor.concurrency == crate::channel::ConcurrencyPolicy::Forbid
        && !plan.materialise.is_empty()
    {
        let key = descriptor.singleton_key.as_str();
        let holds = repo.singleton_holds(&[key]).await?;
        holds.get(key).copied().unwrap_or(0) >= descriptor.singleton_slots
    } else {
        false
    };
    let busy_reason = slots_full.then(|| descriptor.singleton_busy_reason());

    for scheduled_for in &plan.materialise {
        let (occurrence_status, error_message) = match busy_reason.as_deref() {
            Some(reason) => (status::SKIPPED_SINGLETON, Some(reason)),
            None => (status::PENDING, None),
        };
        let created = repo
            .insert_occurrence(NewOccurrence {
                id: &new_occurrence_id(),
                channel_id: &descriptor.channel_id,
                channel_name: &descriptor.channel_name,
                channel_version: descriptor.version,
                workflow_id: descriptor.workflow_id.as_deref(),
                trigger: trigger::CRON,
                scheduled_for: *scheduled_for,
                status: occurrence_status,
                error_message,
            })
            .await?;
        if created {
            crate::metrics::record_cron_occurrence(occurrence_status);
            if slots_full {
                crate::metrics::record_cron_singleton_contention();
                tracing::info!(
                    channel_id = %descriptor.channel_id,
                    scheduled_for = %scheduled_for,
                    singleton_key = %descriptor.singleton_key,
                    "Cron occurrence skipped when due: every slot of its singleton key \
                     is held by a running attempt"
                );
            } else {
                tracing::debug!(
                    channel_id = %descriptor.channel_id,
                    scheduled_for = %scheduled_for,
                    "Materialised cron occurrence"
                );
            }
        }
    }

    // The misses, as one row rather than one per instant. A per-second schedule
    // down for a day missed 86 400 occurrences, and writing 86 400 rows to say
    // so turns an outage into a second outage.
    if let Some(summary) = plan.skipped.as_ref() {
        let created = repo
            .insert_occurrence(NewOccurrence {
                id: &new_occurrence_id(),
                channel_id: &descriptor.channel_id,
                channel_name: &descriptor.channel_name,
                channel_version: descriptor.version,
                workflow_id: descriptor.workflow_id.as_deref(),
                trigger: trigger::CRON,
                // The newest missed instant carries the row, so it keeps a
                // stable, unique identity under the same key everything else
                // uses.
                scheduled_for: summary.newest,
                status: status::SKIPPED_MISFIRE,
                error_message: Some(&summary.reason),
            })
            .await?;
        if created {
            tracing::warn!(
                channel_id = %descriptor.channel_id,
                skipped = summary.count,
                oldest = %summary.oldest,
                newest = %summary.newest,
                policy = descriptor.misfire.as_str(),
                "Cron occurrences were missed while nothing was scheduling them"
            );
            // Counted per missed occurrence, not per summary row: the row is a
            // storage decision and the metric is a fact about the work.
            crate::metrics::record_cron_occurrences(status::SKIPPED_MISFIRE, summary.count);
        }
    }

    let Some(next_cursor) = plan.next_cursor else {
        tracing::warn!(
            channel_id = %descriptor.channel_id,
            schedule = %descriptor.expression,
            "Cron schedule has no further occurrence; parking its cursor"
        );
        return Ok(());
    };
    // Compare-and-set. Losing is normal in a cluster and means a peer ran the
    // same pass: it inserted the same rows (idempotently) and moved the cursor,
    // so there is nothing left to do.
    if !repo
        .advance_cursor(&descriptor.channel_id, state.next_fire_at, next_cursor)
        .await?
    {
        tracing::debug!(
            channel_id = %descriptor.channel_id,
            "Another node advanced this cursor first"
        );
    }
    Ok(())
}

/// UUID v7: time-ordered, so the ledger's primary key and its listing order
/// agree and a page of recent occurrences is a range scan.
fn new_occurrence_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::repositories::cron::{CronOccurrenceFilter, SqlCronRepository};
    use chrono::Duration;
    use chrono::Timelike;

    fn descriptor_at(channel_id: &str, schedule: &str, version: i64) -> CronDescriptor {
        serde_json::from_value::<crate::channel::CronTransportConfig>(serde_json::json!({
            "schedule": schedule,
        }))
        .expect("config")
        .compile(crate::channel::CronIdentity {
            channel_id: channel_id.to_string(),
            channel_name: channel_id.to_string(),
            version,
            workflow_id: Some("wf".to_string()),
        })
        .expect("compiles")
    }

    fn descriptor(channel_id: &str, schedule: &str) -> CronDescriptor {
        descriptor_at(channel_id, schedule, 1)
    }

    async fn repo() -> Arc<SqlCronRepository> {
        Arc::new(SqlCronRepository::new(
            crate::storage::test_sqlite_pool().await,
        ))
    }

    /// Drive `reconcile_channel` directly: the loop and the generation are the
    /// integration test's business, and what needs pinning here is the cursor
    /// arithmetic.
    async fn pass(repo: &Arc<SqlCronRepository>, descriptor: &CronDescriptor, now: NaiveDateTime) {
        let config = crate::config::CronConfig::default();
        let states = repo.schedule_states().await.expect("states");
        reconcile_channel(repo.as_ref(), &config, descriptor, &states, now)
            .await
            .expect("pass");
    }

    /// A start instant `hours_back` hours ago, half past the hour.
    ///
    /// Both halves matter for a test that plans hourly instants. Mid-hour keeps
    /// a pass out of the misfire grace around a boundary — run at 02:00:01 it
    /// would materialise the 02:00 occurrence itself, correctly, and a count or
    /// a cursor asserted against it would be off by one — and backdating keeps
    /// every instant the passes plan in the past whatever the clock says.
    async fn half_past(repo: &Arc<SqlCronRepository>, hours_back: i64) -> NaiveDateTime {
        let now = repo.db_now().await.expect("db now");
        now.date()
            .and_hms_opt(now.hour(), 30, 0)
            .expect("a valid wall-clock instant")
            - Duration::hours(hours_back)
    }

    async fn occurrence_count(repo: &Arc<SqlCronRepository>) -> i64 {
        repo.list_paginated(&CronOccurrenceFilter::default())
            .await
            .expect("list")
            .total
    }

    /// A new channel starts from now, never from whenever it was written. The
    /// first pass materialises nothing — it establishes where "now" is.
    #[tokio::test]
    async fn a_new_channel_starts_from_now() {
        let repo = repo().await;
        let d = descriptor("ch", "0 0 * * * *"); // hourly
        let now = repo.db_now().await.expect("db now");

        pass(&repo, &d, now).await;
        assert_eq!(occurrence_count(&repo).await, 0, "no back-fill");
        let state = repo.schedule_states().await.expect("states").remove(0);
        assert!(state.next_fire_at > now);
        assert_eq!(state.config_hash, d.config_hash);
    }

    #[tokio::test]
    async fn a_due_instant_is_materialised_once_however_many_passes_run() {
        let repo = repo().await;
        let d = descriptor("ch", "* * * * * *"); // every second
        let now = repo.db_now().await.expect("db now");

        pass(&repo, &d, now).await;
        let cursor = repo.schedule_states().await.expect("states")[0].next_fire_at;

        // A pass at the cursor materialises exactly it…
        pass(&repo, &d, cursor).await;
        assert_eq!(occurrence_count(&repo).await, 1);

        // …and running the same pass again — a retried tick, or a second node —
        // adds nothing, because the cursor already moved and the identity index
        // would refuse it anyway.
        pass(&repo, &d, cursor).await;
        assert_eq!(occurrence_count(&repo).await, 1);
    }

    async fn statuses(repo: &Arc<SqlCronRepository>) -> Vec<String> {
        let mut rows = repo
            .list_paginated(&CronOccurrenceFilter::default())
            .await
            .expect("list")
            .data;
        rows.sort_by_key(|o| o.scheduled_for);
        rows.into_iter().map(|o| o.status).collect()
    }

    /// A `forbid` occurrence due while every slot of its key is held is
    /// written already settled `skipped_singleton`, not queued `pending` for a
    /// worker to claim only to skip it (#377). Once a slot is free, the next
    /// due occurrence is queued as usual.
    #[tokio::test]
    async fn a_forbid_occurrence_due_with_every_slot_held_is_skipped_when_due() {
        use crate::storage::repositories::cron::{AttemptStart, ClaimRequest, SingletonRequest};

        let repo = repo().await;
        let d = serde_json::from_value::<crate::channel::CronTransportConfig>(serde_json::json!({
            "schedule": "* * * * * *",
            "concurrency": { "policy": "forbid" },
        }))
        .expect("config")
        .compile(crate::channel::CronIdentity {
            channel_id: "ch".to_string(),
            channel_name: "ch".to_string(),
            version: 1,
            workflow_id: Some("wf".to_string()),
        })
        .expect("compiles");
        let now = repo.db_now().await.expect("db now");
        pass(&repo, &d, now).await;

        // One occurrence runs and holds the only slot: a manual one, already
        // due, so it can be claimed now.
        repo.insert_occurrence(NewOccurrence {
            id: "holder",
            channel_id: "ch",
            channel_name: "ch",
            channel_version: 1,
            workflow_id: Some("wf"),
            trigger: trigger::MANUAL,
            scheduled_for: now - Duration::seconds(5),
            status: status::PENDING,
            error_message: None,
        })
        .await
        .expect("insert");
        let claimed = repo
            .claim_due(ClaimRequest {
                claimant: "node-a",
                limit: 1,
                lease_secs: 60,
            })
            .await
            .expect("claim");
        let singleton = SingletonRequest {
            key: &d.singleton_key,
            holder: "node-a",
            lease_secs: 60,
            slots: d.singleton_slots,
        };
        let started = repo
            .start_attempt(&claimed[0], "node-a", 1, Some(singleton), 60)
            .await
            .expect("start");
        let held = match started {
            AttemptStart::Started { held } => {
                held.expect("the first occurrence takes the free slot")
            }
            _ => unreachable!("the only slot is free"),
        };

        // The next scheduled tick comes due while it runs.
        let second = repo.schedule_states().await.expect("states")[0].next_fire_at;
        pass(&repo, &d, second).await;
        assert_eq!(statuses(&repo).await, ["running", "skipped_singleton"]);
        let skipped = repo
            .list_paginated(&CronOccurrenceFilter {
                status: Some("skipped_singleton".to_string()),
                ..Default::default()
            })
            .await
            .expect("list")
            .data
            .remove(0);
        assert!(
            skipped.completed_at.is_some(),
            "written settled, so it carries when"
        );
        assert!(
            skipped
                .error_message
                .as_deref()
                .is_some_and(|m| m.contains("singleton key 'ch'")),
            "{:?}",
            skipped.error_message
        );
        assert_eq!(repo.pending_count(None).await.expect("pending"), 0);

        // The slot is released: the tick after is queued to run.
        repo.release_singleton(&d.singleton_key, &claimed[0].id, held)
            .await
            .expect("release");
        let third = repo.schedule_states().await.expect("states")[0].next_fire_at;
        pass(&repo, &d, third).await;
        assert_eq!(
            statuses(&repo).await,
            ["running", "skipped_singleton", "pending"]
        );
    }

    /// `allow` takes no slot, so nothing is ever skipped when due.
    #[tokio::test]
    async fn an_allow_occurrence_is_always_queued() {
        let repo = repo().await;
        let d = descriptor("ch", "* * * * * *");
        let now = repo.db_now().await.expect("db now");
        pass(&repo, &d, now).await;
        let cursor = repo.schedule_states().await.expect("states")[0].next_fire_at;
        pass(&repo, &d, cursor).await;
        assert_eq!(statuses(&repo).await, ["pending"]);
    }

    /// The crash window the ledger is designed around: the rows are written and
    /// the cursor is not. The next pass must converge, not duplicate.
    #[tokio::test]
    async fn a_crash_between_the_insert_and_the_cursor_converges() {
        let repo = repo().await;
        let d = descriptor("ch", "* * * * * *");
        let now = repo.db_now().await.expect("db now");
        pass(&repo, &d, now).await;
        let cursor = repo.schedule_states().await.expect("states")[0].next_fire_at;

        // Simulate the crash: insert what the pass would have, and leave the
        // cursor where it was.
        repo.insert_occurrence(NewOccurrence {
            id: "manual-id",
            channel_id: "ch",
            channel_name: "ch",
            channel_version: 1,
            workflow_id: Some("wf"),
            trigger: trigger::CRON,
            scheduled_for: cursor,
            status: status::PENDING,
            error_message: None,
        })
        .await
        .expect("insert");

        pass(&repo, &d, cursor).await;
        assert_eq!(
            occurrence_count(&repo).await,
            1,
            "the replayed insert must lose to the unique key"
        );
        assert!(repo.schedule_states().await.expect("states")[0].next_fire_at > cursor);
    }

    /// Downtime under the default policy: one run brings the world up to date,
    /// and the misses are one visible row rather than hundreds.
    #[tokio::test]
    async fn downtime_runs_the_newest_and_summarises_the_rest() {
        let repo = repo().await;
        let d = descriptor("ch", "0 0 * * * *"); // hourly
        let start = half_past(&repo, 7).await;
        pass(&repo, &d, start).await;

        // Six hours later, having scheduled nothing in between.
        pass(&repo, &d, start + Duration::hours(6)).await;

        let page = repo
            .list_paginated(&CronOccurrenceFilter::default())
            .await
            .expect("list");
        let pending: Vec<_> = page
            .data
            .iter()
            .filter(|o| o.status == status::PENDING)
            .collect();
        let skipped: Vec<_> = page
            .data
            .iter()
            .filter(|o| o.status == status::SKIPPED_MISFIRE)
            .collect();
        assert_eq!(pending.len(), 1, "`latest` runs one occurrence");
        assert_eq!(skipped.len(), 1, "and records the rest as one row");
        assert!(
            skipped[0]
                .error_message
                .as_deref()
                .expect("a reason")
                .contains("missed occurrence"),
            "{:?}",
            skipped[0].error_message
        );
    }

    /// Editing the expression resets that cursor, and resets it to *now* rather
    /// than replaying the gap between the old schedule and the new one.
    #[tokio::test]
    async fn a_schedule_change_resets_the_cursor_without_back_filling() {
        let repo = repo().await;
        let hourly = descriptor("ch", "0 0 * * * *");
        let start = repo.db_now().await.expect("db now");
        pass(&repo, &hourly, start).await;

        let daily = descriptor("ch", "0 15 2 * * *");
        assert_ne!(daily.config_hash, hourly.config_hash);

        // A pass hours later, with the new expression.
        let later = start + Duration::hours(6);
        pass(&repo, &daily, later).await;

        assert_eq!(
            occurrence_count(&repo).await,
            0,
            "a schedule change must not replay the old schedule's gap"
        );
        let state = repo.schedule_states().await.expect("states").remove(0);
        assert_eq!(state.config_hash, daily.config_hash);
        assert!(state.next_fire_at > later);
    }

    /// A new version whose *scheduling* fields are unchanged keeps its place:
    /// editing a payload must not move when the job next runs.
    #[tokio::test]
    async fn an_unchanged_schedule_hash_keeps_the_cursor() {
        let repo = repo().await;
        let d = descriptor("ch", "0 0 * * * *");
        // Anchored: seeded from the clock at 16:59:59, the second pass a minute
        // later lands past the top of the hour and advances the cursor for real.
        let start = half_past(&repo, 1).await;
        pass(&repo, &d, start).await;
        let cursor = repo.schedule_states().await.expect("states")[0].next_fire_at;

        // The same schedule, a later version.
        let next = descriptor_at("ch", "0 0 * * * *", 2);
        assert_eq!(next.config_hash, d.config_hash);
        pass(&repo, &next, start + Duration::minutes(1)).await;

        let state = repo.schedule_states().await.expect("states").remove(0);
        assert_eq!(
            state.next_fire_at, cursor,
            "an unchanged scheduling hash must preserve the cursor"
        );
    }
}
