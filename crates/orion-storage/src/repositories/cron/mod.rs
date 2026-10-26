//! The durable side of a schedule: cursors, occurrences and singletons.
//!
//! Everything correctness-bearing about cron scheduling is in this file,
//! because all of it is the same kind of statement — a conditional write whose
//! *number of affected rows* is the answer. Nothing here trusts a read it took
//! earlier, and nothing here trusts a node's clock:
//!
//! - **Materialisation** is insert-if-absent against
//!   `UNIQUE(channel_id, scheduled_for)`. Two reconcilers, or one reconciler
//!   retried after a crash, produce one row.
//! - **Cursor advance** is a compare-and-set on the value the caller planned
//!   from. A reconciler whose plan is stale loses and re-plans next pass.
//! - **Claiming** leases rows with the trace DLQ's per-backend shapes:
//!   `UPDATE … RETURNING` where it exists, `SELECT … FOR UPDATE SKIP LOCKED`
//!   then `UPDATE` on MySQL.
//! - **Singleton acquisition** happens in the *same transaction* that moves the
//!   occurrence to `running`, which is the whole non-overlap guarantee: there
//!   is no instant at which an occurrence is running without holding its key.
//! - **Heartbeat, settle and release** are all conditional on
//!   `(occurrence, holder, fencing_token)`. A superseded holder matches zero
//!   rows and writes nothing.
//!
//! Every timestamp comparison uses [`helpers::sql_now`] — the database's clock,
//! following `cluster.rs`. Node clock skew decides nothing.

use async_trait::async_trait;
use chrono::NaiveDateTime;
use sea_query::{Asterisk, Condition, Expr, ExprTrait, IntoIden, Query, SimpleExpr};

use super::helpers::{self, Page, PaginatedResult, Projection};
use crate::build_sqlx;
use crate::models::{CronOccurrence, CronScheduleState};
use crate::schema::{
    CronOccurrences, CronScheduleState as ScheduleState, CronSingletonSlots, CronSingletons,
};
use crate::{DbBackend, DbPool};
use orion_common::errors::OrionError;

// ============================================================
// Status vocabulary
// ============================================================

/// An occurrence's lifecycle, as written to `cron_occurrences.status`.
///
/// String constants rather than an enum on the row: the column is an open
/// string on the wire (a client must tolerate a status it does not know), and
/// the queries below compare against these same literals, so one table of names
/// serves both.
pub mod status {
    /// Materialised and waiting for a worker.
    pub const PENDING: &str = "pending";
    /// Leased by a worker that has not yet started it.
    pub const CLAIMED: &str = "claimed";
    /// Executing, holding its claim and (under `forbid`) its singleton.
    pub const RUNNING: &str = "running";
    pub const COMPLETED: &str = "completed";
    pub const FAILED: &str = "failed";
    /// Its time passed while nothing could run it. One row summarises a run of
    /// them.
    pub const SKIPPED_MISFIRE: &str = "skipped_misfire";
    /// Its singleton key was held when it was claimed, under
    /// `concurrency.policy = "forbid"`.
    pub const SKIPPED_SINGLETON: &str = "skipped_singleton";

    /// The statuses a worker may still act on — what "claimable" means before
    /// the lease conditions are applied.
    pub const ACTIVE: [&str; 3] = [CLAIMED, RUNNING, PENDING];

    /// The statuses `/retry` accepts: an occurrence that reached an end and did
    /// not do its work.
    ///
    /// `completed` is absent deliberately. Re-running finished work is not a
    /// retry, and an occurrence is identified by the instant it was *due* — so
    /// "run it again now" is a manual trigger, which mints its own occurrence
    /// rather than overwriting the record of one that succeeded.
    pub const RETRYABLE: [&str; 3] = [FAILED, SKIPPED_MISFIRE, SKIPPED_SINGLETON];
}

/// How an occurrence came to exist.
pub mod trigger {
    /// Materialised by the reconciler from the channel's schedule.
    pub const CRON: &str = "cron";
    /// Created by `POST /api/v2/admin/channels/{id}/trigger`. Runs through the
    /// identical claim, singleton and execution path.
    pub const MANUAL: &str = "manual";
}

// ============================================================
// Inputs
// ============================================================

/// One occurrence to materialise.
#[derive(Debug, Clone)]
pub struct NewOccurrence<'a> {
    pub id: &'a str,
    pub channel_id: &'a str,
    pub channel_name: &'a str,
    pub channel_version: i64,
    pub workflow_id: Option<&'a str>,
    pub trigger: &'a str,
    pub scheduled_for: NaiveDateTime,
    /// `pending` for work to run, `skipped_misfire` for a summary row,
    /// `skipped_singleton` for a `forbid` occurrence due while every slot of
    /// its key was held.
    pub status: &'a str,
    /// Why the row is already settled, for either skipped status.
    pub error_message: Option<&'a str>,
}

/// The cursor to write for one channel.
#[derive(Debug, Clone)]
pub struct ScheduleCursor<'a> {
    pub channel_id: &'a str,
    pub channel_version: i64,
    pub config_hash: &'a str,
    pub next_fire_at: NaiveDateTime,
}

/// Who is claiming, and for how long.
#[derive(Debug, Clone)]
pub struct ClaimRequest<'a> {
    pub claimant: &'a str,
    pub limit: i64,
    pub lease_secs: u64,
}

/// The singleton half of the acquisition, when the channel's policy takes one.
#[derive(Debug, Clone)]
pub struct SingletonRequest<'a> {
    pub key: &'a str,
    pub holder: &'a str,
    pub lease_secs: u64,
    /// How many occurrences of `key` this channel may run at once: the
    /// attempt may hold any slot below this, lowest free first.
    pub slots: u32,
}

/// The slot of a singleton key an attempt holds, and the token it holds it
/// under. Slot 0 is the `cron_singletons` row; the rest are
/// `cron_singleton_slots` rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeldSlot {
    pub slot: u32,
    pub fencing_token: i64,
}

/// What [`CronRepository::start_attempt`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptStart {
    /// The occurrence is `running`. Under `forbid`, it holds this slot of its
    /// singleton key; under `allow` nothing is held and no row was taken.
    Started { held: Option<HeldSlot> },
    /// Every slot the channel may use is held by a live lease belonging to
    /// another occurrence. The caller applies the channel's policy.
    SingletonBusy,
    /// The occurrence was not in the state the caller claimed it in — another
    /// node took it over after its lease expired, or an operator retried it.
    /// The caller drops the attempt without writing anything.
    Lost,
}

/// Terminal state for one attempt.
#[derive(Debug, Clone)]
pub struct Settlement<'a> {
    pub occurrence_id: &'a str,
    pub claimant: &'a str,
    pub status: &'a str,
    pub error_message: Option<&'a str>,
    pub trace_id: Option<&'a str>,
}

/// Filter for the occurrence listing.
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CronOccurrenceFilter {
    /// Stable channel id, not the name — an occurrence outlives the name it was
    /// materialised under.
    pub channel_id: Option<String>,
    /// One status, spelled as the column stores it.
    pub status: Option<String>,
    /// Inclusive lower bound on `scheduled_for`.
    pub since: Option<NaiveDateTime>,
    /// Inclusive upper bound on `scheduled_for`.
    pub until: Option<NaiveDateTime>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

impl CronOccurrenceFilter {
    fn condition(&self) -> Condition {
        let mut cond = Condition::all();
        if let Some(ref channel_id) = self.channel_id {
            cond = cond.add(Expr::col(CronOccurrences::ChannelId).eq(channel_id.as_str()));
        }
        if let Some(ref status) = self.status {
            cond = cond.add(Expr::col(CronOccurrences::Status).eq(status.as_str()));
        }
        if let Some(since) = self.since {
            cond = cond.add(Expr::col(CronOccurrences::ScheduledFor).gte(since));
        }
        if let Some(until) = self.until {
            cond = cond.add(Expr::col(CronOccurrences::ScheduledFor).lte(until));
        }
        cond
    }
}

// ============================================================
// The trait
// ============================================================

#[async_trait]
pub trait CronRepository: Send + Sync {
    /// The database's own clock.
    ///
    /// Every planning decision is made against this rather than against
    /// `Utc::now()`, so two nodes with skewed clocks plan the same passes and
    /// a node whose clock jumps cannot invent or skip occurrences.
    async fn db_now(&self) -> Result<NaiveDateTime, OrionError>;

    /// Every cursor, for one reconciliation pass.
    async fn schedule_states(&self) -> Result<Vec<CronScheduleState>, OrionError>;

    /// Create or reset one channel's cursor, clearing `paused_at`.
    async fn upsert_cursor(&self, cursor: ScheduleCursor<'_>) -> Result<(), OrionError>;

    /// Mark a cursor paused: the channel is no longer active, so it stops
    /// producing occurrences. Its history is kept.
    async fn pause_cursor(&self, channel_id: &str) -> Result<(), OrionError>;

    /// Advance a cursor **only if** it still holds the value the caller planned
    /// from. `false` means another reconciler moved it first; the caller
    /// discards its plan rather than re-applying it.
    async fn advance_cursor(
        &self,
        channel_id: &str,
        from: NaiveDateTime,
        to: NaiveDateTime,
    ) -> Result<bool, OrionError>;

    /// Insert an occurrence unless `(channel_id, scheduled_for)` already
    /// exists. `true` when this call created it.
    async fn insert_occurrence(&self, occurrence: NewOccurrence<'_>) -> Result<bool, OrionError>;

    /// Lease up to `limit` due occurrences: `pending` and due, or an expired
    /// claim to recover.
    async fn claim_due(&self, req: ClaimRequest<'_>) -> Result<Vec<CronOccurrence>, OrionError>;

    /// Take the singleton (when one is asked for) and move the occurrence to
    /// `running`, in one transaction.
    async fn start_attempt(
        &self,
        occurrence: &CronOccurrence,
        claimant: &str,
        executing_version: i64,
        singleton: Option<SingletonRequest<'_>>,
        lease_secs: u64,
    ) -> Result<AttemptStart, OrionError>;

    /// Extend an attempt's claim and its singleton. `false` means ownership was
    /// lost and the caller must cancel.
    async fn renew(
        &self,
        occurrence_id: &str,
        claimant: &str,
        held: Option<HeldSlot>,
        lease_secs: u64,
    ) -> Result<bool, OrionError>;

    /// Write the terminal status, conditional on still owning the occurrence.
    async fn settle(&self, settlement: Settlement<'_>) -> Result<bool, OrionError>;

    /// Record a `skipped_singleton` outcome for a claimed occurrence.
    async fn settle_skipped(
        &self,
        occurrence_id: &str,
        claimant: &str,
        reason: &str,
    ) -> Result<bool, OrionError>;

    /// Return a claimed occurrence to `pending` without consuming an attempt's
    /// worth of progress — used when a guard defers rather than refuses.
    async fn release_claim(&self, occurrence_id: &str, claimant: &str) -> Result<bool, OrionError>;

    /// Drop a singleton slot, but only if this occurrence still holds it
    /// under this token.
    async fn release_singleton(
        &self,
        key: &str,
        occurrence_id: &str,
        held: HeldSlot,
    ) -> Result<bool, OrionError>;

    /// Live leases per singleton key, across slot 0 and the extra slots —
    /// what `GET /cron/status` reports as held.
    async fn singleton_holds(
        &self,
        keys: &[&str],
    ) -> Result<std::collections::HashMap<String, u32>, OrionError>;

    /// Attach the trace this attempt wrote.
    async fn set_trace_id(&self, occurrence_id: &str, trace_id: &str) -> Result<(), OrionError>;

    async fn get_by_id(&self, id: &str) -> Result<CronOccurrence, OrionError>;

    async fn list_paginated(
        &self,
        filter: &CronOccurrenceFilter,
    ) -> Result<PaginatedResult<CronOccurrence>, OrionError>;

    /// Reset a terminal occurrence to `pending` for another attempt, keeping its
    /// identity and its `scheduled_for`.
    async fn requeue(&self, id: &str) -> Result<CronOccurrence, OrionError>;

    /// Stop an occurrence that has not finished: settle it `failed` with
    /// `reason`, take its claim away from whoever holds it, and shorten its
    /// singleton hold to end `grace_secs` from now.
    ///
    /// Nothing here can tell a dead holder from a live one, and nothing needs
    /// to. A live holder's next heartbeat finds its claim gone and cancels the
    /// run, and its late settle matches nothing. A dead holder's slot is free
    /// once the grace ends, rather than when the lease it last renewed ends.
    /// The grace, rather than a delete, covers the live holder until its next
    /// heartbeat: freed at once, the slot could admit a new attempt beside a
    /// run still finishing its current step.
    async fn cancel(
        &self,
        id: &str,
        reason: &str,
        grace_secs: u64,
    ) -> Result<CronOccurrence, OrionError>;

    /// The newest occurrence for one channel, for the status endpoint.
    async fn latest_for_channel(
        &self,
        channel_id: &str,
    ) -> Result<Option<CronOccurrence>, OrionError>;

    /// How many occurrences are waiting for a worker — for one channel, or
    /// across the instance when `channel_id` is `None`.
    async fn pending_count(&self, channel_id: Option<&str>) -> Result<i64, OrionError>;

    /// The age in seconds of the oldest `pending` occurrence, for `/health`.
    /// `None` when nothing is waiting.
    async fn oldest_pending_age_secs(&self) -> Result<Option<i64>, OrionError>;

    /// Delete terminal occurrences older than `hours`. Retention, on the same
    /// cadence as trace cleanup.
    async fn delete_terminal_older_than(&self, hours: u64) -> Result<u64, OrionError>;

    /// Remove a channel's cursor and its occurrences. Called when the channel
    /// row is deleted outright — an archived channel keeps both.
    async fn purge_channel(&self, channel_id: &str) -> Result<(), OrionError>;
}

// ============================================================
// Query shapes
// ============================================================

/// Claimable = due and unowned, or owned by a lease that has expired.
///
/// The second half is crash recovery: a node that died mid-attempt left an
/// occurrence `claimed` or `running` with a `claimed_until` that stops moving,
/// and this is what lets a peer take it over — after the lease, never before.
fn claimable(now: &'static str) -> Condition {
    Condition::any()
        .add(
            Condition::all()
                .add(Expr::col(CronOccurrences::Status).eq(status::PENDING))
                .add(Expr::col(CronOccurrences::ScheduledFor).lte(Expr::cust(now)))
                .add(
                    Condition::any()
                        .add(Expr::col(CronOccurrences::ClaimedUntil).is_null())
                        .add(Expr::col(CronOccurrences::ClaimedUntil).lt(Expr::cust(now))),
                ),
        )
        .add(
            Condition::all()
                .add(Expr::col(CronOccurrences::Status).is_in([status::CLAIMED, status::RUNNING]))
                .add(Expr::col(CronOccurrences::ClaimedUntil).lt(Expr::cust(now))),
        )
}

/// Single-statement claim for the backends with `UPDATE … RETURNING`
/// (Postgres, SQLite) — the trace DLQ's shape, for the same reasons.
fn claim_update_query(
    claimant: &str,
    limit: i64,
    now: &'static str,
    lease_until: &str,
    skip_locked: bool,
) -> sea_query::UpdateStatement {
    let mut due_ids = Query::select()
        .column(CronOccurrences::Id)
        .from(CronOccurrences::Table)
        .cond_where(claimable(now))
        // Oldest first: a backlog drains in the order the work was due.
        .order_by(CronOccurrences::ScheduledFor, sea_query::Order::Asc)
        .limit(Ord::max(limit, 0) as u64)
        .to_owned();
    if skip_locked {
        due_ids.lock_with_behavior(
            sea_query::LockType::Update,
            sea_query::LockBehavior::SkipLocked,
        );
    }
    let mut update = Query::update()
        .table(CronOccurrences::Table)
        .value(CronOccurrences::UpdatedAt, helpers::updated_now())
        .value(CronOccurrences::Status, status::CLAIMED)
        .value(CronOccurrences::ClaimedBy, claimant)
        .value(
            CronOccurrences::ClaimedUntil,
            Expr::cust(lease_until.to_owned()),
        )
        .value(
            CronOccurrences::Attempt,
            Expr::col(CronOccurrences::Attempt).add(1),
        )
        .and_where(Expr::col(CronOccurrences::Id).in_subquery(due_ids))
        .to_owned();
    update.returning_all();
    update
}

/// MySQL claim, step 1: lock the due rows. MySQL 8 has `SKIP LOCKED` but no
/// `UPDATE … RETURNING`.
fn claim_select_query(limit: i64, now: &'static str) -> sea_query::SelectStatement {
    let mut select = Query::select()
        .column(Asterisk)
        .from(CronOccurrences::Table)
        .cond_where(claimable(now))
        .order_by(CronOccurrences::ScheduledFor, sea_query::Order::Asc)
        .limit(Ord::max(limit, 0) as u64)
        .to_owned();
    select.lock_with_behavior(
        sea_query::LockType::Update,
        sea_query::LockBehavior::SkipLocked,
    );
    select
}

/// MySQL claim, step 2: lease the rows step 1 locked.
fn lease_claimed_query<'a>(
    claimant: &str,
    lease_until: &str,
    ids: impl IntoIterator<Item = &'a str>,
) -> sea_query::UpdateStatement {
    Query::update()
        .table(CronOccurrences::Table)
        .value(CronOccurrences::UpdatedAt, helpers::updated_now())
        .value(CronOccurrences::Status, status::CLAIMED)
        .value(CronOccurrences::ClaimedBy, claimant)
        .value(
            CronOccurrences::ClaimedUntil,
            Expr::cust(lease_until.to_owned()),
        )
        .value(
            CronOccurrences::Attempt,
            Expr::col(CronOccurrences::Attempt).add(1),
        )
        .and_where(Expr::col(CronOccurrences::Id).is_in(ids))
        .to_owned()
}

fn occurrence_select(id: &str) -> sea_query::SelectStatement {
    Query::select()
        .column(Asterisk)
        .from(CronOccurrences::Table)
        .and_where(Expr::col(CronOccurrences::Id).eq(id))
        .to_owned()
}

// ============================================================
// SQL implementation
// ============================================================

pub struct SqlCronRepository {
    pool: DbPool,
}

impl SqlCronRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    fn now_expr(&self) -> SimpleExpr {
        Expr::cust(helpers::sql_now(self.pool.backend()))
    }

    fn lease_until(&self, secs: u64) -> String {
        helpers::sql_now_plus_secs(self.pool.backend(), secs)
    }
}

#[async_trait]
impl CronRepository for SqlCronRepository {
    async fn db_now(&self) -> Result<NaiveDateTime, OrionError> {
        orion_common::metrics::timed_db_op("cron.db_now", async {
            let (sql, values) =
                build_sqlx(self.pool.backend(), Query::select().expr(self.now_expr()));
            self.pool
                .fetch_scalar::<NaiveDateTime>(&sql, values)
                .await
                .map_err(OrionError::Storage)
        })
        .await
    }

    async fn schedule_states(&self) -> Result<Vec<CronScheduleState>, OrionError> {
        orion_common::metrics::timed_db_op("cron.schedule_states", async {
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::select()
                    .column(Asterisk)
                    .from(ScheduleState::Table)
                    .order_by(ScheduleState::ChannelId, sea_query::Order::Asc),
            );
            Ok(self
                .pool
                .fetch_all_as::<CronScheduleState>(&sql, values)
                .await?)
        })
        .await
    }

    async fn upsert_cursor(&self, cursor: ScheduleCursor<'_>) -> Result<(), OrionError> {
        orion_common::metrics::timed_db_op("cron.upsert_cursor", async {
            // Update-then-insert rather than a backend-specific upsert: the
            // update is the common path (a reactivation, a schedule edit) and
            // the insert only ever runs once per channel.
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::update()
                    .table(ScheduleState::Table)
                    .value(ScheduleState::UpdatedAt, helpers::updated_now())
                    .value(ScheduleState::ChannelVersion, cursor.channel_version)
                    .value(ScheduleState::ConfigHash, cursor.config_hash)
                    .value(ScheduleState::NextFireAt, cursor.next_fire_at)
                    .value(ScheduleState::PausedAt, Option::<NaiveDateTime>::None)
                    .and_where(Expr::col(ScheduleState::ChannelId).eq(cursor.channel_id)),
            );
            if self.pool.execute_query(&sql, values).await? > 0 {
                return Ok(());
            }
            let insert = Query::insert()
                .into_table(ScheduleState::Table)
                .columns([
                    ScheduleState::ChannelId,
                    ScheduleState::ChannelVersion,
                    ScheduleState::ConfigHash,
                    ScheduleState::NextFireAt,
                ])
                .values_panic([
                    cursor.channel_id.into(),
                    cursor.channel_version.into(),
                    cursor.config_hash.into(),
                    cursor.next_fire_at.into(),
                ])
                .to_owned();
            helpers::insert_if_absent(&self.pool, insert, ScheduleState::ChannelId).await?;
            Ok(())
        })
        .await
    }

    async fn pause_cursor(&self, channel_id: &str) -> Result<(), OrionError> {
        orion_common::metrics::timed_db_op("cron.pause_cursor", async {
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::update()
                    .table(ScheduleState::Table)
                    .value(ScheduleState::UpdatedAt, helpers::updated_now())
                    .value(ScheduleState::PausedAt, self.now_expr())
                    .and_where(Expr::col(ScheduleState::ChannelId).eq(channel_id))
                    .and_where(Expr::col(ScheduleState::PausedAt).is_null()),
            );
            self.pool.execute_query(&sql, values).await?;
            Ok(())
        })
        .await
    }

    async fn advance_cursor(
        &self,
        channel_id: &str,
        from: NaiveDateTime,
        to: NaiveDateTime,
    ) -> Result<bool, OrionError> {
        orion_common::metrics::timed_db_op("cron.advance_cursor", async {
            // Compare-and-set on the value the caller planned from. Two
            // reconcilers that planned the same pass both insert the same
            // occurrence (idempotent) and exactly one advances the cursor; the
            // loser re-plans next pass and finds nothing left to do.
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::update()
                    .table(ScheduleState::Table)
                    .value(ScheduleState::UpdatedAt, helpers::updated_now())
                    .value(ScheduleState::NextFireAt, to)
                    .and_where(Expr::col(ScheduleState::ChannelId).eq(channel_id))
                    .and_where(Expr::col(ScheduleState::NextFireAt).eq(from)),
            );
            Ok(self.pool.execute_query(&sql, values).await? > 0)
        })
        .await
    }

    async fn insert_occurrence(&self, occurrence: NewOccurrence<'_>) -> Result<bool, OrionError> {
        orion_common::metrics::timed_db_op("cron.insert_occurrence", async {
            // A row written already settled — a misfire summary, a `forbid`
            // tick that came due with every slot held — is settled as of now,
            // like one a worker settles.
            let completed_at = if occurrence.status == status::PENDING {
                Expr::val(Option::<NaiveDateTime>::None)
            } else {
                Expr::cust(helpers::sql_now(self.pool.backend()))
            };
            let insert = Query::insert()
                .into_table(CronOccurrences::Table)
                .columns([
                    CronOccurrences::Id,
                    CronOccurrences::ChannelId,
                    CronOccurrences::ChannelName,
                    CronOccurrences::ChannelVersion,
                    CronOccurrences::WorkflowId,
                    CronOccurrences::Trigger,
                    CronOccurrences::ScheduledFor,
                    CronOccurrences::Status,
                    CronOccurrences::ErrorMessage,
                    CronOccurrences::CompletedAt,
                ])
                .values_panic([
                    occurrence.id.into(),
                    occurrence.channel_id.into(),
                    occurrence.channel_name.into(),
                    occurrence.channel_version.into(),
                    helpers::optional_string_value(occurrence.workflow_id).into(),
                    occurrence.trigger.into(),
                    occurrence.scheduled_for.into(),
                    occurrence.status.into(),
                    helpers::optional_string_value(occurrence.error_message).into(),
                    completed_at,
                ])
                .to_owned();
            // The conflict is on `(channel_id, scheduled_for)`, not on the
            // primary key: the id is freshly minted every call, so a duplicate
            // is only ever detectable by the identity index.
            Ok(helpers::insert_if_absent_on(
                &self.pool,
                insert,
                [CronOccurrences::ChannelId, CronOccurrences::ScheduledFor],
            )
            .await?
                > 0)
        })
        .await
    }

    async fn claim_due(&self, req: ClaimRequest<'_>) -> Result<Vec<CronOccurrence>, OrionError> {
        orion_common::metrics::timed_db_op("cron.claim_due", async {
            let backend = self.pool.backend();
            let now = helpers::sql_now(backend);
            let lease_until = self.lease_until(req.lease_secs);
            match backend {
                DbBackend::Postgres | DbBackend::Sqlite => {
                    let (sql, values) = build_sqlx(
                        backend,
                        &mut claim_update_query(
                            req.claimant,
                            req.limit,
                            now,
                            &lease_until,
                            backend == DbBackend::Postgres,
                        ),
                    );
                    Ok(self
                        .pool
                        .fetch_all_as::<CronOccurrence>(&sql, values)
                        .await?)
                }
                DbBackend::Mysql => {
                    let mut tx = self.pool.begin_tx().await.map_err(OrionError::Storage)?;
                    let (sql, values) =
                        build_sqlx(backend, &mut claim_select_query(req.limit, now));
                    let rows: Vec<CronOccurrence> = tx.fetch_all_as(&sql, values).await?;
                    if rows.is_empty() {
                        tx.commit().await.map_err(OrionError::Storage)?;
                        return Ok(rows);
                    }
                    let (sql, values) = build_sqlx(
                        backend,
                        &mut lease_claimed_query(
                            req.claimant,
                            &lease_until,
                            rows.iter().map(|r| r.id.as_str()),
                        ),
                    );
                    tx.execute_query(&sql, values).await?;
                    tx.commit().await.map_err(OrionError::Storage)?;
                    // The pre-UPDATE rows, with the fields the caller needs
                    // corrected to what the UPDATE wrote — the same read-back
                    // avoidance the DLQ takes, except that `attempt` is
                    // incremented here and the caller reports it.
                    Ok(rows
                        .into_iter()
                        .map(|mut row| {
                            row.status = status::CLAIMED.to_string();
                            row.claimed_by = Some(req.claimant.to_string());
                            row.attempt += 1;
                            row
                        })
                        .collect())
                }
            }
        })
        .await
    }

    async fn start_attempt(
        &self,
        occurrence: &CronOccurrence,
        claimant: &str,
        executing_version: i64,
        singleton: Option<SingletonRequest<'_>>,
        lease_secs: u64,
    ) -> Result<AttemptStart, OrionError> {
        orion_common::metrics::timed_db_op("cron.start_attempt", async {
            let backend = self.pool.backend();
            let now = helpers::sql_now(backend);
            let lease_until = self.lease_until(lease_secs);
            // `begin_write_tx` rather than `begin_tx`: this reads the singleton
            // row before it writes one, which is exactly the shape SQLite needs
            // an immediate transaction for (D30).
            let mut tx = self
                .pool
                .begin_write_tx()
                .await
                .map_err(OrionError::Storage)?;

            let mut held = None;
            if let Some(singleton) = singleton.as_ref() {
                // One occurrence holds at most one slot. A re-started attempt
                // of the same occurrence — its node died after this
                // transaction committed and a peer re-claimed it while the
                // dead node's lease is still live — re-takes the slot it
                // already holds, fencing the dead holder, rather than a
                // second one. With one slot there is nothing else to take,
                // and the statements stay exactly those of a single lock.
                let own = if singleton.slots > 1 {
                    own_slot(&mut tx, backend, singleton.key, &occurrence.id).await?
                } else {
                    None
                };
                let candidates: Vec<u32> = match own {
                    Some(slot) => vec![slot],
                    None => (0..Ord::max(singleton.slots, 1)).collect(),
                };
                // Ascending, always: on MySQL an UPDATE that examined a row
                // keeps its record lock to the end of the transaction even
                // when the predicate failed, so every transaction taking
                // slots in one order is what rules out a lock cycle. Never
                // start from a random slot for "fairness".
                for slot in candidates {
                    if let Some(fencing_token) = take_slot(
                        &mut tx,
                        backend,
                        SlotRow::of(singleton.key, slot),
                        &occurrence.id,
                        singleton.holder,
                        &lease_until,
                        now,
                    )
                    .await?
                    {
                        held = Some(HeldSlot {
                            slot,
                            fencing_token,
                        });
                        break;
                    }
                }
                if held.is_none() {
                    // Live leases on every slot. Nothing was written; roll
                    // back so the occurrence keeps its claim for the caller
                    // to settle.
                    drop(tx);
                    return Ok(AttemptStart::SingletonBusy);
                }
            }

            // …and move the occurrence to `running` in the same transaction.
            // Conditional on this node still holding the claim: if the lease
            // expired and a peer took over between the claim and here, this
            // matches nothing and the attempt is abandoned.
            let mut update = Query::update()
                .table(CronOccurrences::Table)
                .value(CronOccurrences::UpdatedAt, helpers::updated_now())
                .value(CronOccurrences::Status, status::RUNNING)
                .value(CronOccurrences::StartedAt, Expr::cust(now))
                .value(CronOccurrences::ExecutingVersion, executing_version)
                .value(
                    CronOccurrences::ClaimedUntil,
                    Expr::cust(lease_until.clone()),
                )
                .and_where(Expr::col(CronOccurrences::Id).eq(occurrence.id.as_str()))
                .and_where(Expr::col(CronOccurrences::ClaimedBy).eq(claimant))
                .and_where(Expr::col(CronOccurrences::Status).eq(status::CLAIMED))
                .to_owned();
            if let Some(singleton) = singleton.as_ref() {
                update.value(CronOccurrences::SingletonKey, singleton.key);
            }
            if let Some(held) = held {
                update.value(CronOccurrences::FencingToken, held.fencing_token);
                update.value(CronOccurrences::SingletonSlot, i64::from(held.slot));
            }
            let (sql, values) = build_sqlx(backend, &mut update);
            let started = tx.execute_query(&sql, values).await? > 0;
            if !started {
                drop(tx);
                return Ok(AttemptStart::Lost);
            }
            tx.commit().await.map_err(OrionError::Storage)?;
            Ok(AttemptStart::Started { held })
        })
        .await
    }

    async fn renew(
        &self,
        occurrence_id: &str,
        claimant: &str,
        held: Option<HeldSlot>,
        lease_secs: u64,
    ) -> Result<bool, OrionError> {
        orion_common::metrics::timed_db_op("cron.renew", async {
            let backend = self.pool.backend();
            let lease_until = self.lease_until(lease_secs);

            let (sql, values) = build_sqlx(
                backend,
                Query::update()
                    .table(CronOccurrences::Table)
                    .value(CronOccurrences::UpdatedAt, helpers::updated_now())
                    .value(
                        CronOccurrences::ClaimedUntil,
                        Expr::cust(lease_until.clone()),
                    )
                    .and_where(Expr::col(CronOccurrences::Id).eq(occurrence_id))
                    .and_where(Expr::col(CronOccurrences::ClaimedBy).eq(claimant))
                    .and_where(Expr::col(CronOccurrences::Status).eq(status::RUNNING)),
            );
            if self.pool.execute_query(&sql, values).await? == 0 {
                return Ok(false);
            }
            let Some(held) = held else {
                return Ok(true);
            };
            // The singleton half. Conditional on the occurrence *and* the token,
            // so a holder superseded after a lease expiry cannot keep renewing
            // a key it no longer owns.
            let mut update = if held.slot == 0 {
                Query::update()
                    .table(CronSingletons::Table)
                    .value(CronSingletons::UpdatedAt, helpers::updated_now())
                    .value(CronSingletons::LeaseUntil, Expr::cust(lease_until))
                    .and_where(Expr::col(CronSingletons::OccurrenceId).eq(occurrence_id))
                    .and_where(Expr::col(CronSingletons::Holder).eq(claimant))
                    .and_where(Expr::col(CronSingletons::FencingToken).eq(held.fencing_token))
                    .to_owned()
            } else {
                Query::update()
                    .table(CronSingletonSlots::Table)
                    .value(CronSingletonSlots::UpdatedAt, helpers::updated_now())
                    .value(CronSingletonSlots::LeaseUntil, Expr::cust(lease_until))
                    .and_where(Expr::col(CronSingletonSlots::Slot).eq(i64::from(held.slot)))
                    .and_where(Expr::col(CronSingletonSlots::OccurrenceId).eq(occurrence_id))
                    .and_where(Expr::col(CronSingletonSlots::Holder).eq(claimant))
                    .and_where(Expr::col(CronSingletonSlots::FencingToken).eq(held.fencing_token))
                    .to_owned()
            };
            let (sql, values) = build_sqlx(backend, &mut update);
            Ok(self.pool.execute_query(&sql, values).await? > 0)
        })
        .await
    }

    async fn settle(&self, settlement: Settlement<'_>) -> Result<bool, OrionError> {
        orion_common::metrics::timed_db_op("cron.settle", async {
            let backend = self.pool.backend();
            let now = helpers::sql_now(backend);
            let mut update = Query::update()
                .table(CronOccurrences::Table)
                .value(CronOccurrences::UpdatedAt, helpers::updated_now())
                .value(CronOccurrences::Status, settlement.status)
                .value(CronOccurrences::CompletedAt, Expr::cust(now))
                .value(CronOccurrences::ClaimedUntil, Option::<NaiveDateTime>::None)
                .value(
                    CronOccurrences::ErrorMessage,
                    helpers::optional_string_value(settlement.error_message),
                )
                .and_where(Expr::col(CronOccurrences::Id).eq(settlement.occurrence_id))
                .and_where(Expr::col(CronOccurrences::ClaimedBy).eq(settlement.claimant))
                .to_owned();
            if let Some(trace_id) = settlement.trace_id {
                update.value(CronOccurrences::TraceId, trace_id);
            }
            let (sql, values) = build_sqlx(backend, &mut update);
            Ok(self.pool.execute_query(&sql, values).await? > 0)
        })
        .await
    }

    async fn settle_skipped(
        &self,
        occurrence_id: &str,
        claimant: &str,
        reason: &str,
    ) -> Result<bool, OrionError> {
        self.settle(Settlement {
            occurrence_id,
            claimant,
            status: status::SKIPPED_SINGLETON,
            error_message: Some(reason),
            trace_id: None,
        })
        .await
    }

    async fn release_claim(&self, occurrence_id: &str, claimant: &str) -> Result<bool, OrionError> {
        orion_common::metrics::timed_db_op("cron.release_claim", async {
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::update()
                    .table(CronOccurrences::Table)
                    .value(CronOccurrences::UpdatedAt, helpers::updated_now())
                    .value(CronOccurrences::Status, status::PENDING)
                    .value(CronOccurrences::ClaimedBy, Option::<String>::None)
                    .value(CronOccurrences::ClaimedUntil, Option::<NaiveDateTime>::None)
                    .and_where(Expr::col(CronOccurrences::Id).eq(occurrence_id))
                    .and_where(Expr::col(CronOccurrences::ClaimedBy).eq(claimant)),
            );
            Ok(self.pool.execute_query(&sql, values).await? > 0)
        })
        .await
    }

    async fn release_singleton(
        &self,
        key: &str,
        occurrence_id: &str,
        held: HeldSlot,
    ) -> Result<bool, OrionError> {
        orion_common::metrics::timed_db_op("cron.release_singleton", async {
            // Conditional on all of them, which is what stops a slow holder
            // that has already been superseded from deleting the *new*
            // holder's row and letting another occurrence in alongside it.
            let mut delete = if held.slot == 0 {
                Query::delete()
                    .from_table(CronSingletons::Table)
                    .and_where(Expr::col(CronSingletons::SingletonKey).eq(key))
                    .and_where(Expr::col(CronSingletons::OccurrenceId).eq(occurrence_id))
                    .and_where(Expr::col(CronSingletons::FencingToken).eq(held.fencing_token))
                    .to_owned()
            } else {
                Query::delete()
                    .from_table(CronSingletonSlots::Table)
                    .and_where(Expr::col(CronSingletonSlots::SingletonKey).eq(key))
                    .and_where(Expr::col(CronSingletonSlots::Slot).eq(i64::from(held.slot)))
                    .and_where(Expr::col(CronSingletonSlots::OccurrenceId).eq(occurrence_id))
                    .and_where(Expr::col(CronSingletonSlots::FencingToken).eq(held.fencing_token))
                    .to_owned()
            };
            let (sql, values) = build_sqlx(self.pool.backend(), &mut delete);
            Ok(self.pool.execute_query(&sql, values).await? > 0)
        })
        .await
    }

    async fn singleton_holds(
        &self,
        keys: &[&str],
    ) -> Result<std::collections::HashMap<String, u32>, OrionError> {
        orion_common::metrics::timed_db_op("cron.singleton_holds", async {
            let mut out = std::collections::HashMap::new();
            if keys.is_empty() {
                return Ok(out);
            }
            let backend = self.pool.backend();
            let now = helpers::sql_now(backend);
            let keys: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
            let (sql, values) = build_sqlx(
                backend,
                Query::select()
                    .column(CronSingletons::SingletonKey)
                    .from(CronSingletons::Table)
                    .and_where(Expr::col(CronSingletons::SingletonKey).is_in(keys.clone()))
                    .and_where(Expr::col(CronSingletons::LeaseUntil).gte(Expr::cust(now))),
            );
            let legacy: Vec<(String,)> = self.pool.fetch_all_as(&sql, values).await?;
            let (sql, values) = build_sqlx(
                backend,
                Query::select()
                    .column(CronSingletonSlots::SingletonKey)
                    .from(CronSingletonSlots::Table)
                    .and_where(Expr::col(CronSingletonSlots::SingletonKey).is_in(keys))
                    .and_where(Expr::col(CronSingletonSlots::LeaseUntil).gte(Expr::cust(now))),
            );
            let extra: Vec<(String,)> = self.pool.fetch_all_as(&sql, values).await?;
            for (key,) in legacy.into_iter().chain(extra) {
                *out.entry(key).or_insert(0) += 1;
            }
            Ok(out)
        })
        .await
    }

    async fn set_trace_id(&self, occurrence_id: &str, trace_id: &str) -> Result<(), OrionError> {
        orion_common::metrics::timed_db_op("cron.set_trace_id", async {
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::update()
                    .table(CronOccurrences::Table)
                    .value(CronOccurrences::UpdatedAt, helpers::updated_now())
                    .value(CronOccurrences::TraceId, trace_id)
                    .and_where(Expr::col(CronOccurrences::Id).eq(occurrence_id)),
            );
            self.pool.execute_query(&sql, values).await?;
            Ok(())
        })
        .await
    }

    async fn get_by_id(&self, id: &str) -> Result<CronOccurrence, OrionError> {
        orion_common::metrics::timed_db_op("cron.get_by_id", async {
            let (sql, values) = build_sqlx(self.pool.backend(), &mut occurrence_select(id));
            helpers::fetch_required(&self.pool, &sql, values, || {
                OrionError::NotFound(format!("Cron occurrence '{id}' not found"))
            })
            .await
        })
        .await
    }

    async fn list_paginated(
        &self,
        filter: &CronOccurrenceFilter,
    ) -> Result<PaginatedResult<CronOccurrence>, OrionError> {
        orion_common::metrics::timed_db_op("cron.list_paginated", async {
            let (limit, offset) = helpers::clamp_pagination(filter.limit, filter.offset);
            let page = Page {
                from: sea_query::IntoIden::into_iden(CronOccurrences::Table),
                projection: Projection::All,
                cond: filter.condition(),
                // Newest first: an operator opening the list is asking what
                // happened recently.
                sort: sea_query::IntoIden::into_iden(CronOccurrences::CreatedAt),
                order: sea_query::Order::Desc,
                limit,
                offset,
            };
            helpers::paginate(&self.pool, page).await
        })
        .await
    }

    async fn requeue(&self, id: &str) -> Result<CronOccurrence, OrionError> {
        orion_common::metrics::timed_db_op("cron.requeue", async {
            // Only from a terminal, unsuccessful state. `completed` is refused
            // by the route with a 409 rather than silently re-running finished
            // work.
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::update()
                    .table(CronOccurrences::Table)
                    .value(CronOccurrences::UpdatedAt, helpers::updated_now())
                    .value(CronOccurrences::Status, status::PENDING)
                    .value(CronOccurrences::ClaimedBy, Option::<String>::None)
                    .value(CronOccurrences::ClaimedUntil, Option::<NaiveDateTime>::None)
                    .value(CronOccurrences::SingletonKey, Option::<String>::None)
                    .value(CronOccurrences::FencingToken, Option::<i64>::None)
                    .value(CronOccurrences::SingletonSlot, Option::<i64>::None)
                    .value(CronOccurrences::CompletedAt, Option::<NaiveDateTime>::None)
                    .value(CronOccurrences::ErrorMessage, Option::<String>::None)
                    .and_where(Expr::col(CronOccurrences::Id).eq(id))
                    .and_where(Expr::col(CronOccurrences::Status).is_in(status::RETRYABLE)),
            );
            if self.pool.execute_query(&sql, values).await? == 0 {
                // Distinguish "no such occurrence" from "wrong state", which
                // are a 404 and a 409 at the route.
                let current = self.get_by_id(id).await?;
                return Err(OrionError::Conflict(format!(
                    "Cron occurrence '{id}' is '{}' and cannot be retried. Retry applies \
                     to {}; to run this schedule again now, trigger the channel.",
                    current.status,
                    status::RETRYABLE.join(", ")
                )));
            }
            self.get_by_id(id).await
        })
        .await
    }

    async fn cancel(
        &self,
        id: &str,
        reason: &str,
        grace_secs: u64,
    ) -> Result<CronOccurrence, OrionError> {
        orion_common::metrics::timed_db_op("cron.cancel", async {
            let backend = self.pool.backend();
            let now = helpers::sql_now(backend);
            let grace_until = self.lease_until(grace_secs);
            let mut tx = self
                .pool
                .begin_write_tx()
                .await
                .map_err(OrionError::Storage)?;

            // `claimed_by` cleared is what fences the holder: `renew`,
            // `start_attempt` and `settle` all match on it.
            let (sql, values) = build_sqlx(
                backend,
                Query::update()
                    .table(CronOccurrences::Table)
                    .value(CronOccurrences::UpdatedAt, helpers::updated_now())
                    .value(CronOccurrences::Status, status::FAILED)
                    .value(CronOccurrences::ClaimedBy, Option::<String>::None)
                    .value(CronOccurrences::ClaimedUntil, Option::<NaiveDateTime>::None)
                    .value(CronOccurrences::CompletedAt, Expr::cust(now))
                    .value(CronOccurrences::ErrorMessage, reason)
                    .and_where(Expr::col(CronOccurrences::Id).eq(id))
                    .and_where(Expr::col(CronOccurrences::Status).is_in(status::ACTIVE)),
            );
            if tx.execute_query(&sql, values).await? == 0 {
                drop(tx);
                let current = self.get_by_id(id).await?;
                return Err(OrionError::Conflict(format!(
                    "Cron occurrence '{id}' is '{}' and has already finished. Cancel \
                     applies to {}.",
                    current.status,
                    status::ACTIVE.join(", ")
                )));
            }

            // Shorten, never lengthen: a hold already ending sooner keeps its
            // own end.
            for mut update in [
                Query::update()
                    .table(CronSingletons::Table)
                    .value(CronSingletons::UpdatedAt, helpers::updated_now())
                    .value(CronSingletons::LeaseUntil, Expr::cust(grace_until.clone()))
                    .and_where(Expr::col(CronSingletons::OccurrenceId).eq(id))
                    .and_where(
                        Expr::col(CronSingletons::LeaseUntil).gt(Expr::cust(grace_until.clone())),
                    )
                    .to_owned(),
                Query::update()
                    .table(CronSingletonSlots::Table)
                    .value(CronSingletonSlots::UpdatedAt, helpers::updated_now())
                    .value(
                        CronSingletonSlots::LeaseUntil,
                        Expr::cust(grace_until.clone()),
                    )
                    .and_where(Expr::col(CronSingletonSlots::OccurrenceId).eq(id))
                    .and_where(
                        Expr::col(CronSingletonSlots::LeaseUntil)
                            .gt(Expr::cust(grace_until.clone())),
                    )
                    .to_owned(),
            ] {
                let (sql, values) = build_sqlx(backend, &mut update);
                tx.execute_query(&sql, values).await?;
            }
            tx.commit().await.map_err(OrionError::Storage)?;
            self.get_by_id(id).await
        })
        .await
    }

    /// The newest occurrence per channel, for the status endpoint.
    async fn latest_for_channel(
        &self,
        channel_id: &str,
    ) -> Result<Option<CronOccurrence>, OrionError> {
        orion_common::metrics::timed_db_op("cron.latest_for_channel", async {
            // One indexed read per channel rather than a "newest per group"
            // query. The status endpoint has the channel list in hand and a
            // scheduled estate is tens of channels, not thousands; a correlated
            // subquery or a window function would buy nothing and would have to
            // render identically on three backends.
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::select()
                    .column(Asterisk)
                    .from(CronOccurrences::Table)
                    .and_where(Expr::col(CronOccurrences::ChannelId).eq(channel_id))
                    .order_by(CronOccurrences::CreatedAt, sea_query::Order::Desc)
                    .order_by(CronOccurrences::Id, sea_query::Order::Desc)
                    .limit(1),
            );
            Ok(self
                .pool
                .fetch_optional_as::<CronOccurrence>(&sql, values)
                .await?)
        })
        .await
    }

    async fn pending_count(&self, channel_id: Option<&str>) -> Result<i64, OrionError> {
        orion_common::metrics::timed_db_op("cron.pending_count", async {
            let mut cond =
                Condition::all().add(Expr::col(CronOccurrences::Status).eq(status::PENDING));
            if let Some(channel_id) = channel_id {
                cond = cond.add(Expr::col(CronOccurrences::ChannelId).eq(channel_id));
            }
            helpers::count_where(
                &self.pool,
                sea_query::IntoIden::into_iden(CronOccurrences::Table),
                cond,
            )
            .await
        })
        .await
    }

    async fn oldest_pending_age_secs(&self) -> Result<Option<i64>, OrionError> {
        orion_common::metrics::timed_db_op("cron.oldest_pending_age", async {
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::select()
                    .expr(Expr::col(CronOccurrences::ScheduledFor).min())
                    .from(CronOccurrences::Table)
                    .and_where(Expr::col(CronOccurrences::Status).eq(status::PENDING)),
            );
            // `MIN` over no rows is one row holding NULL, not zero rows, so the
            // scalar read is `Option` twice over.
            let oldest: Option<NaiveDateTime> = self
                .pool
                .fetch_scalar::<Option<NaiveDateTime>>(&sql, values)
                .await
                .map_err(OrionError::Storage)?;
            let Some(oldest) = oldest else {
                return Ok(None);
            };
            // Against the database clock, like everything else here: an age
            // computed from a skewed node clock is what would make this alert
            // fire on the wrong node.
            let now = self.db_now().await?;
            Ok(Some(Ord::max((now - oldest).num_seconds(), 0)))
        })
        .await
    }

    async fn delete_terminal_older_than(&self, hours: u64) -> Result<u64, OrionError> {
        orion_common::metrics::timed_db_op("cron.delete_terminal_older_than", async {
            let now = self.db_now().await?;
            let cutoff = helpers::cutoff_hours_ago(now, hours);
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::delete()
                    .from_table(CronOccurrences::Table)
                    // Terminal only. A `pending` occurrence older than the
                    // retention window is a backlog, not history, and deleting
                    // it would silently drop work.
                    .and_where(Expr::col(CronOccurrences::Status).is_in([
                        status::COMPLETED,
                        status::FAILED,
                        status::SKIPPED_MISFIRE,
                        status::SKIPPED_SINGLETON,
                    ]))
                    .and_where(Expr::col(CronOccurrences::CreatedAt).lt(cutoff)),
            );
            self.pool
                .execute_query(&sql, values)
                .await
                .map_err(OrionError::Storage)
        })
        .await
    }

    async fn purge_channel(&self, channel_id: &str) -> Result<(), OrionError> {
        orion_common::metrics::timed_db_op("cron.purge_channel", async {
            let mut occurrences = Query::delete()
                .from_table(CronOccurrences::Table)
                .and_where(Expr::col(CronOccurrences::ChannelId).eq(channel_id))
                .to_owned();
            let (sql, values) = build_sqlx(self.pool.backend(), &mut occurrences);
            self.pool.execute_query(&sql, values).await?;

            let mut cursor = Query::delete()
                .from_table(ScheduleState::Table)
                .and_where(Expr::col(ScheduleState::ChannelId).eq(channel_id))
                .to_owned();
            let (sql, values) = build_sqlx(self.pool.backend(), &mut cursor);
            self.pool.execute_query(&sql, values).await?;
            Ok(())
        })
        .await
    }
}

mod singletons;
use singletons::*;

#[cfg(test)]
mod tests;
