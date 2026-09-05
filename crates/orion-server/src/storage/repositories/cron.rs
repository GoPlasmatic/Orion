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
use sea_query::{Asterisk, Condition, Expr, ExprTrait, Query, SimpleExpr};

use super::helpers::{self, Page, PaginatedResult, Projection};
use crate::errors::OrionError;
use crate::storage::build_sqlx;
use crate::storage::models::{CronOccurrence, CronScheduleState};
use crate::storage::schema::{CronOccurrences, CronScheduleState as ScheduleState, CronSingletons};
use crate::storage::{DbBackend, DbPool};

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
    /// Created by `POST /api/v1/admin/channels/{id}/trigger`. Runs through the
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
    /// `pending` for work to run, `skipped_misfire` for a summary row.
    pub status: &'a str,
    /// The summary text, for a `skipped_misfire` row.
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
}

/// What [`CronRepository::start_attempt`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptStart {
    /// The occurrence is `running`. Under `forbid`, the singleton is held under
    /// this token; under `allow` the token is `None` and no row was taken.
    Started { fencing_token: Option<i64> },
    /// The singleton key is held by a live lease belonging to another
    /// occurrence. The caller applies the channel's policy.
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
        fencing_token: Option<i64>,
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

    /// Drop a singleton row, but only if this occurrence still holds it under
    /// this token.
    async fn release_singleton(
        &self,
        key: &str,
        occurrence_id: &str,
        fencing_token: i64,
    ) -> Result<bool, OrionError>;

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
        crate::metrics::timed_db_op("cron.db_now", async {
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
        crate::metrics::timed_db_op("cron.schedule_states", async {
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
        crate::metrics::timed_db_op("cron.upsert_cursor", async {
            // Update-then-insert rather than a backend-specific upsert: the
            // update is the common path (a reactivation, a schedule edit) and
            // the insert only ever runs once per channel.
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::update()
                    .table(ScheduleState::Table)
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
        crate::metrics::timed_db_op("cron.pause_cursor", async {
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::update()
                    .table(ScheduleState::Table)
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
        crate::metrics::timed_db_op("cron.advance_cursor", async {
            // Compare-and-set on the value the caller planned from. Two
            // reconcilers that planned the same pass both insert the same
            // occurrence (idempotent) and exactly one advances the cursor; the
            // loser re-plans next pass and finds nothing left to do.
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::update()
                    .table(ScheduleState::Table)
                    .value(ScheduleState::NextFireAt, to)
                    .and_where(Expr::col(ScheduleState::ChannelId).eq(channel_id))
                    .and_where(Expr::col(ScheduleState::NextFireAt).eq(from)),
            );
            Ok(self.pool.execute_query(&sql, values).await? > 0)
        })
        .await
    }

    async fn insert_occurrence(&self, occurrence: NewOccurrence<'_>) -> Result<bool, OrionError> {
        crate::metrics::timed_db_op("cron.insert_occurrence", async {
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
        crate::metrics::timed_db_op("cron.claim_due", async {
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
        crate::metrics::timed_db_op("cron.start_attempt", async {
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

            let mut fencing_token = None;
            if let Some(singleton) = singleton.as_ref() {
                // Take the key: renew our own, or take over one whose lease has
                // expired. `fencing_token + 1` on every acquisition, so a
                // superseded holder's conditional writes match nothing.
                let (sql, values) = build_sqlx(
                    backend,
                    Query::update()
                        .table(CronSingletons::Table)
                        .value(CronSingletons::OccurrenceId, occurrence.id.as_str())
                        .value(CronSingletons::Holder, singleton.holder)
                        .value(
                            CronSingletons::FencingToken,
                            Expr::col(CronSingletons::FencingToken).add(1),
                        )
                        .value(CronSingletons::LeaseUntil, Expr::cust(lease_until.clone()))
                        .cond_where(
                            Condition::all()
                                .add(Expr::col(CronSingletons::SingletonKey).eq(singleton.key))
                                // Ours to renew, or nobody's because the lease
                                // ran out. Anything else is a live holder and
                                // this matches nothing.
                                .add(
                                    Condition::any()
                                        .add(
                                            Expr::col(CronSingletons::OccurrenceId)
                                                .eq(occurrence.id.as_str()),
                                        )
                                        .add(
                                            Expr::col(CronSingletons::LeaseUntil)
                                                .lt(Expr::cust(now)),
                                        ),
                                ),
                        ),
                );
                let took = tx.execute_query(&sql, values).await? > 0;

                if !took {
                    // Either no row exists (the key is free) or a live lease
                    // holds it. Insert-if-absent settles which, without a read.
                    let insert = Query::insert()
                        .into_table(CronSingletons::Table)
                        .columns([
                            CronSingletons::SingletonKey,
                            CronSingletons::OccurrenceId,
                            CronSingletons::Holder,
                            CronSingletons::FencingToken,
                            CronSingletons::LeaseUntil,
                        ])
                        .values_panic([
                            singleton.key.into(),
                            occurrence.id.as_str().into(),
                            singleton.holder.into(),
                            1i64.into(),
                            Expr::cust(lease_until.clone()),
                        ])
                        .to_owned();
                    let inserted = helpers::insert_if_absent_tx(
                        &mut tx,
                        insert,
                        [CronSingletons::SingletonKey],
                    )
                    .await?;
                    if inserted == 0 {
                        // A live lease. Nothing was written; roll back so the
                        // occurrence keeps its claim for the caller to settle.
                        drop(tx);
                        return Ok(AttemptStart::SingletonBusy);
                    }
                }

                // Read back the token we now hold. One extra statement inside
                // the transaction, and it is what the heartbeat and the release
                // are checked against for the rest of the attempt.
                let (sql, values) = build_sqlx(
                    backend,
                    Query::select()
                        .column(CronSingletons::FencingToken)
                        .from(CronSingletons::Table)
                        .and_where(Expr::col(CronSingletons::SingletonKey).eq(singleton.key)),
                );
                fencing_token = tx
                    .fetch_optional_as::<FencingTokenRow>(&sql, values)
                    .await?
                    .map(|row| row.fencing_token);
            }

            // …and move the occurrence to `running` in the same transaction.
            // Conditional on this node still holding the claim: if the lease
            // expired and a peer took over between the claim and here, this
            // matches nothing and the attempt is abandoned.
            let mut update = Query::update()
                .table(CronOccurrences::Table)
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
            if let Some(token) = fencing_token {
                update.value(CronOccurrences::FencingToken, token);
            }
            let (sql, values) = build_sqlx(backend, &mut update);
            let started = tx.execute_query(&sql, values).await? > 0;
            if !started {
                drop(tx);
                return Ok(AttemptStart::Lost);
            }
            tx.commit().await.map_err(OrionError::Storage)?;
            Ok(AttemptStart::Started { fencing_token })
        })
        .await
    }

    async fn renew(
        &self,
        occurrence_id: &str,
        claimant: &str,
        fencing_token: Option<i64>,
        lease_secs: u64,
    ) -> Result<bool, OrionError> {
        crate::metrics::timed_db_op("cron.renew", async {
            let backend = self.pool.backend();
            let lease_until = self.lease_until(lease_secs);

            let (sql, values) = build_sqlx(
                backend,
                Query::update()
                    .table(CronOccurrences::Table)
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
            let Some(token) = fencing_token else {
                return Ok(true);
            };
            // The singleton half. Conditional on the occurrence *and* the token,
            // so a holder superseded after a lease expiry cannot keep renewing
            // a key it no longer owns.
            let (sql, values) = build_sqlx(
                backend,
                Query::update()
                    .table(CronSingletons::Table)
                    .value(CronSingletons::LeaseUntil, Expr::cust(lease_until))
                    .and_where(Expr::col(CronSingletons::OccurrenceId).eq(occurrence_id))
                    .and_where(Expr::col(CronSingletons::Holder).eq(claimant))
                    .and_where(Expr::col(CronSingletons::FencingToken).eq(token)),
            );
            Ok(self.pool.execute_query(&sql, values).await? > 0)
        })
        .await
    }

    async fn settle(&self, settlement: Settlement<'_>) -> Result<bool, OrionError> {
        crate::metrics::timed_db_op("cron.settle", async {
            let backend = self.pool.backend();
            let now = helpers::sql_now(backend);
            let mut update = Query::update()
                .table(CronOccurrences::Table)
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
        crate::metrics::timed_db_op("cron.release_claim", async {
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::update()
                    .table(CronOccurrences::Table)
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
        fencing_token: i64,
    ) -> Result<bool, OrionError> {
        crate::metrics::timed_db_op("cron.release_singleton", async {
            // Conditional on all three, which is what stops a slow holder that
            // has already been superseded from deleting the *new* holder's row
            // and letting a third occurrence in alongside it.
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::delete()
                    .from_table(CronSingletons::Table)
                    .and_where(Expr::col(CronSingletons::SingletonKey).eq(key))
                    .and_where(Expr::col(CronSingletons::OccurrenceId).eq(occurrence_id))
                    .and_where(Expr::col(CronSingletons::FencingToken).eq(fencing_token)),
            );
            Ok(self.pool.execute_query(&sql, values).await? > 0)
        })
        .await
    }

    async fn set_trace_id(&self, occurrence_id: &str, trace_id: &str) -> Result<(), OrionError> {
        crate::metrics::timed_db_op("cron.set_trace_id", async {
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::update()
                    .table(CronOccurrences::Table)
                    .value(CronOccurrences::TraceId, trace_id)
                    .and_where(Expr::col(CronOccurrences::Id).eq(occurrence_id)),
            );
            self.pool.execute_query(&sql, values).await?;
            Ok(())
        })
        .await
    }

    async fn get_by_id(&self, id: &str) -> Result<CronOccurrence, OrionError> {
        crate::metrics::timed_db_op("cron.get_by_id", async {
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
        crate::metrics::timed_db_op("cron.list_paginated", async {
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
        crate::metrics::timed_db_op("cron.requeue", async {
            // Only from a terminal, unsuccessful state. `completed` is refused
            // by the route with a 409 rather than silently re-running finished
            // work.
            let (sql, values) = build_sqlx(
                self.pool.backend(),
                Query::update()
                    .table(CronOccurrences::Table)
                    .value(CronOccurrences::Status, status::PENDING)
                    .value(CronOccurrences::ClaimedBy, Option::<String>::None)
                    .value(CronOccurrences::ClaimedUntil, Option::<NaiveDateTime>::None)
                    .value(CronOccurrences::SingletonKey, Option::<String>::None)
                    .value(CronOccurrences::FencingToken, Option::<i64>::None)
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

    /// The newest occurrence per channel, for the status endpoint.
    async fn latest_for_channel(
        &self,
        channel_id: &str,
    ) -> Result<Option<CronOccurrence>, OrionError> {
        crate::metrics::timed_db_op("cron.latest_for_channel", async {
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
        crate::metrics::timed_db_op("cron.pending_count", async {
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
        crate::metrics::timed_db_op("cron.oldest_pending_age", async {
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
        crate::metrics::timed_db_op("cron.delete_terminal_older_than", async {
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
        crate::metrics::timed_db_op("cron.purge_channel", async {
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

/// A one-column read-back. `sqlx::FromRow` needs a named struct even for a
/// single column, and this one is read inside the acquisition transaction.
#[derive(sqlx::FromRow)]
struct FencingTokenRow {
    fencing_token: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    async fn test_repo() -> SqlCronRepository {
        SqlCronRepository::new(crate::storage::test_sqlite_pool().await)
    }

    fn occurrence<'a>(id: &'a str, channel_id: &'a str, at: NaiveDateTime) -> NewOccurrence<'a> {
        NewOccurrence {
            id,
            channel_id,
            channel_name: "nightly",
            channel_version: 1,
            workflow_id: Some("wf"),
            trigger: trigger::CRON,
            scheduled_for: at,
            status: status::PENDING,
            error_message: None,
        }
    }

    /// Materialise one occurrence due in the past, and hand back its id.
    async fn seed_due(repo: &SqlCronRepository, id: &str, channel_id: &str) -> NaiveDateTime {
        let due = repo.db_now().await.expect("db now") - Duration::seconds(30);
        assert!(
            repo.insert_occurrence(occurrence(id, channel_id, due))
                .await
                .expect("insert")
        );
        due
    }

    /// Expire a lease by writing it into the past.
    ///
    /// A zero-second lease is **not** expired: `sql_now` is `datetime('now')`
    /// on SQLite, which has one-second granularity, so a lease written at
    /// `now` compares equal to `now` for the rest of that second and
    /// `claimed_until < now` is false. Production leases are tens of seconds so
    /// this never bites there, but a test that wants "already expired" has to
    /// say so explicitly rather than race the clock's resolution.
    async fn expire_claim(repo: &SqlCronRepository, id: &str) {
        let past = repo.db_now().await.expect("db now") - Duration::hours(1);
        let (sql, values) = build_sqlx(
            repo.pool.backend(),
            Query::update()
                .table(CronOccurrences::Table)
                .value(CronOccurrences::ClaimedUntil, past)
                .and_where(Expr::col(CronOccurrences::Id).eq(id)),
        );
        repo.pool.execute_query(&sql, values).await.expect("expire");
    }

    async fn expire_singleton(repo: &SqlCronRepository, key: &str) {
        let past = repo.db_now().await.expect("db now") - Duration::hours(1);
        let (sql, values) = build_sqlx(
            repo.pool.backend(),
            Query::update()
                .table(CronSingletons::Table)
                .value(CronSingletons::LeaseUntil, past)
                .and_where(Expr::col(CronSingletons::SingletonKey).eq(key)),
        );
        repo.pool.execute_query(&sql, values).await.expect("expire");
    }

    /// Age a row's `created_at`, for the retention tests — same clock-resolution
    /// reason as [`expire_claim`].
    async fn age_row(repo: &SqlCronRepository, id: &str, by: Duration) {
        let then = repo.db_now().await.expect("db now") - by;
        let (sql, values) = build_sqlx(
            repo.pool.backend(),
            Query::update()
                .table(CronOccurrences::Table)
                .value(CronOccurrences::CreatedAt, then)
                .and_where(Expr::col(CronOccurrences::Id).eq(id)),
        );
        repo.pool.execute_query(&sql, values).await.expect("age");
    }

    fn claim(claimant: &str, lease_secs: u64) -> ClaimRequest<'_> {
        ClaimRequest {
            claimant,
            limit: 10,
            lease_secs,
        }
    }

    // ---- materialisation ----

    /// The property the whole reconciler rests on: the identity is
    /// `(channel_id, scheduled_for)`, so a retried tick or a second reconciler
    /// cannot produce a second row — even though the caller mints a fresh id
    /// each time.
    #[tokio::test]
    async fn materialisation_is_idempotent_on_the_scheduled_instant() {
        let repo = test_repo().await;
        let at = repo.db_now().await.expect("db now");

        assert!(
            repo.insert_occurrence(occurrence("a", "ch", at))
                .await
                .expect("first")
        );
        assert!(
            !repo
                .insert_occurrence(occurrence("b", "ch", at))
                .await
                .expect("second"),
            "a second id for the same instant must lose"
        );

        let page = repo
            .list_paginated(&CronOccurrenceFilter::default())
            .await
            .expect("list");
        assert_eq!(page.total, 1);
        assert_eq!(page.data[0].id, "a");

        // A different instant, and a different channel at the same instant, are
        // both distinct occurrences.
        assert!(
            repo.insert_occurrence(occurrence("c", "ch", at + Duration::seconds(1)))
                .await
                .expect("next instant")
        );
        assert!(
            repo.insert_occurrence(occurrence("d", "other", at))
                .await
                .expect("other channel")
        );
    }

    // ---- the cursor ----

    #[tokio::test]
    async fn a_cursor_advances_only_from_the_value_it_was_planned_from() {
        let repo = test_repo().await;
        let t0 = repo.db_now().await.expect("db now");
        let t1 = t0 + Duration::hours(1);
        let t2 = t0 + Duration::hours(2);

        repo.upsert_cursor(ScheduleCursor {
            channel_id: "ch",
            channel_version: 1,
            config_hash: "hash",
            next_fire_at: t0,
        })
        .await
        .expect("upsert");

        // The winner moves it…
        assert!(repo.advance_cursor("ch", t0, t1).await.expect("advance"));
        // …and a reconciler that planned from the old value writes nothing,
        // rather than dragging the cursor backwards and replaying a pass.
        assert!(!repo.advance_cursor("ch", t0, t2).await.expect("stale"));

        let states = repo.schedule_states().await.expect("states");
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].next_fire_at, t1);
    }

    /// A schedule edit resets the cursor and clears a pause; the two are one
    /// write so a reactivated channel cannot come back still paused.
    #[tokio::test]
    async fn upserting_a_cursor_clears_a_pause() {
        let repo = test_repo().await;
        let t0 = repo.db_now().await.expect("db now");
        let cursor = ScheduleCursor {
            channel_id: "ch",
            channel_version: 1,
            config_hash: "hash",
            next_fire_at: t0,
        };
        repo.upsert_cursor(cursor.clone()).await.expect("upsert");
        repo.pause_cursor("ch").await.expect("pause");
        assert!(
            repo.schedule_states().await.expect("states")[0]
                .paused_at
                .is_some()
        );

        repo.upsert_cursor(ScheduleCursor {
            channel_version: 2,
            config_hash: "other",
            next_fire_at: t0 + Duration::hours(3),
            ..cursor
        })
        .await
        .expect("re-upsert");
        let state = repo.schedule_states().await.expect("states").remove(0);
        assert!(
            state.paused_at.is_none(),
            "reactivation must clear the pause"
        );
        assert_eq!(state.channel_version, 2);
        assert_eq!(state.config_hash, "other");
    }

    // ---- claiming ----

    #[tokio::test]
    async fn a_claim_leases_the_row_and_blocks_a_second_claimant() {
        let repo = test_repo().await;
        seed_due(&repo, "occ", "ch").await;

        let first = repo.claim_due(claim("node-a", 60)).await.expect("claim");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].status, status::CLAIMED);
        assert_eq!(first[0].claimed_by.as_deref(), Some("node-a"));
        // The attempt counter moves on the claim, so it counts attempts even
        // when a node dies before settling one.
        assert_eq!(first[0].attempt, 1);

        assert!(
            repo.claim_due(claim("node-b", 60))
                .await
                .expect("claim")
                .is_empty(),
            "a live lease must not be claimable"
        );
    }

    /// Crash recovery: the lease is the only thing standing between a dead
    /// node's work and a peer picking it up.
    #[tokio::test]
    async fn an_expired_claim_is_reclaimable_and_a_live_one_is_not() {
        let repo = test_repo().await;
        seed_due(&repo, "occ", "ch").await;

        repo.claim_due(claim("node-a", 60)).await.expect("claim");
        expire_claim(&repo, "occ").await;
        let reclaimed = repo.claim_due(claim("node-b", 60)).await.expect("reclaim");
        assert_eq!(reclaimed.len(), 1);
        assert_eq!(reclaimed[0].claimed_by.as_deref(), Some("node-b"));
        assert_eq!(reclaimed[0].attempt, 2, "the takeover is a second attempt");

        // And now that node-b holds a live lease, nobody else may.
        assert!(
            repo.claim_due(claim("node-c", 60))
                .await
                .expect("claim")
                .is_empty()
        );
    }

    /// An occurrence whose time has not come is not work yet.
    #[tokio::test]
    async fn an_occurrence_due_in_the_future_is_not_claimed() {
        let repo = test_repo().await;
        let later = repo.db_now().await.expect("db now") + Duration::hours(1);
        repo.insert_occurrence(occurrence("occ", "ch", later))
            .await
            .expect("insert");
        assert!(
            repo.claim_due(claim("node-a", 60))
                .await
                .expect("claim")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn the_claim_batch_is_bounded_and_oldest_first() {
        let repo = test_repo().await;
        let now = repo.db_now().await.expect("db now");
        for i in 0..5 {
            repo.insert_occurrence(occurrence(
                &format!("occ-{i}"),
                "ch",
                now - Duration::minutes(10 - i),
            ))
            .await
            .expect("insert");
        }
        let claimed = repo
            .claim_due(ClaimRequest {
                claimant: "node-a",
                limit: 2,
                lease_secs: 60,
            })
            .await
            .expect("claim");
        assert_eq!(claimed.len(), 2);
        // Oldest first: a backlog drains in the order the work was due.
        assert_eq!(claimed[0].id, "occ-0");
        assert_eq!(claimed[1].id, "occ-1");
    }

    // ---- the singleton ----

    async fn start(
        repo: &SqlCronRepository,
        occurrence: &CronOccurrence,
        claimant: &str,
        key: Option<&str>,
    ) -> AttemptStart {
        repo.start_attempt(
            occurrence,
            claimant,
            1,
            key.map(|key| SingletonRequest {
                key,
                holder: claimant,
                lease_secs: 60,
            }),
            60,
        )
        .await
        .expect("start")
    }

    /// The non-overlap guarantee, at its narrowest: two occurrences, one key,
    /// and only one of them may be running.
    #[tokio::test]
    async fn one_singleton_key_admits_one_occurrence() {
        let repo = test_repo().await;
        let now = repo.db_now().await.expect("db now");
        for (i, id) in ["first", "second"].iter().enumerate() {
            repo.insert_occurrence(occurrence(id, "ch", now - Duration::minutes(5 - i as i64)))
                .await
                .expect("insert");
        }
        let claimed = repo.claim_due(claim("node-a", 60)).await.expect("claim");
        assert_eq!(claimed.len(), 2);

        assert!(matches!(
            start(&repo, &claimed[0], "node-a", Some("key")).await,
            AttemptStart::Started {
                fencing_token: Some(1)
            }
        ));
        assert_eq!(
            start(&repo, &claimed[1], "node-a", Some("key")).await,
            AttemptStart::SingletonBusy,
            "a live key must refuse the second occurrence"
        );
    }

    #[tokio::test]
    async fn distinct_keys_do_not_contend() {
        let repo = test_repo().await;
        let now = repo.db_now().await.expect("db now");
        for (i, id) in ["first", "second"].iter().enumerate() {
            repo.insert_occurrence(occurrence(id, "ch", now - Duration::minutes(5 - i as i64)))
                .await
                .expect("insert");
        }
        let claimed = repo.claim_due(claim("node-a", 60)).await.expect("claim");
        for (occurrence, key) in claimed.iter().zip(["key-a", "key-b"]) {
            assert!(matches!(
                start(&repo, occurrence, "node-a", Some(key)).await,
                AttemptStart::Started { .. }
            ));
        }
    }

    /// `allow` takes no row at all, so two occurrences of the same channel run
    /// side by side.
    #[tokio::test]
    async fn without_a_singleton_occurrences_overlap() {
        let repo = test_repo().await;
        let now = repo.db_now().await.expect("db now");
        for (i, id) in ["first", "second"].iter().enumerate() {
            repo.insert_occurrence(occurrence(id, "ch", now - Duration::minutes(5 - i as i64)))
                .await
                .expect("insert");
        }
        let claimed = repo.claim_due(claim("node-a", 60)).await.expect("claim");
        for occurrence in &claimed {
            assert_eq!(
                start(&repo, occurrence, "node-a", None).await,
                AttemptStart::Started {
                    fencing_token: None
                }
            );
        }
    }

    /// The takeover path, and the reason the token exists: the new holder gets
    /// a higher generation, and the old one's conditional writes stop matching.
    #[tokio::test]
    async fn taking_over_an_expired_key_bumps_the_fencing_token() {
        let repo = test_repo().await;
        let now = repo.db_now().await.expect("db now");
        for (i, id) in ["first", "second"].iter().enumerate() {
            repo.insert_occurrence(occurrence(id, "ch", now - Duration::minutes(5 - i as i64)))
                .await
                .expect("insert");
        }
        let claimed = repo.claim_due(claim("node-a", 60)).await.expect("claim");

        assert_eq!(
            start(&repo, &claimed[0], "node-a", Some("key")).await,
            AttemptStart::Started {
                fencing_token: Some(1)
            }
        );

        // node-a dies. Both its occurrence claim and its singleton lease run
        // out, and node-b picks the second occurrence up.
        expire_singleton(&repo, "key").await;
        expire_claim(&repo, &claimed[1].id).await;
        let taken_over = repo
            .claim_due(claim("node-b", 60))
            .await
            .expect("reclaim")
            .remove(0);
        assert_eq!(taken_over.id, claimed[1].id);

        assert_eq!(
            start(&repo, &taken_over, "node-b", Some("key")).await,
            AttemptStart::Started {
                fencing_token: Some(2)
            },
            "an expired key is takeable, under a higher generation"
        );

        // The superseded holder cannot renew, and cannot release the row the
        // new holder owns — which is what stops a third occurrence slipping in
        // alongside it.
        assert!(
            !repo
                .renew(&claimed[0].id, "node-a", Some(1), 60)
                .await
                .expect("renew"),
            "a superseded holder must not renew"
        );
        assert!(
            !repo
                .release_singleton("key", &claimed[0].id, 1)
                .await
                .expect("release"),
            "a superseded holder must not delete the new holder's row"
        );
        assert!(
            repo.release_singleton("key", &taken_over.id, 2)
                .await
                .expect("release"),
            "the real holder releases its own row"
        );
    }

    /// An occurrence another node took over is not one this node may start.
    #[tokio::test]
    async fn starting_an_occurrence_this_node_no_longer_holds_is_lost() {
        let repo = test_repo().await;
        seed_due(&repo, "occ", "ch").await;
        let mine = repo
            .claim_due(claim("node-a", 60))
            .await
            .expect("claim")
            .remove(0);
        // This node stalls long enough to lose its lease, and someone else
        // takes the occurrence over.
        expire_claim(&repo, "occ").await;
        repo.claim_due(claim("node-b", 60)).await.expect("reclaim");

        assert_eq!(
            start(&repo, &mine, "node-a", None).await,
            AttemptStart::Lost,
            "a stale claimant must write nothing"
        );
    }

    /// The heartbeat's mechanism: a renewal moves both deadlines forward, so a
    /// run outlasts the lease it started under.
    ///
    /// Both halves are conditional on ownership, which is the other thing this
    /// pins: the renewal names the occurrence, the holder and the token, so a
    /// superseded node's beat extends nothing.
    #[tokio::test]
    async fn a_renewal_extends_both_the_claim_and_the_singleton() {
        let repo = test_repo().await;
        seed_due(&repo, "occ", "ch").await;
        let claimed = repo
            .claim_due(claim("node-a", 60))
            .await
            .expect("claim")
            .remove(0);
        let token = match start(&repo, &claimed, "node-a", Some("key")).await {
            AttemptStart::Started { fencing_token } => fencing_token.expect("a token"),
            other => unreachable!("the attempt must start, got {other:?}"),
        };

        // Age both deadlines into the past, as a stalled node would leave them.
        expire_claim(&repo, "occ").await;
        expire_singleton(&repo, "key").await;
        let before = repo.get_by_id("occ").await.expect("get");
        assert!(before.claimed_until.expect("a lease") < repo.db_now().await.expect("now"));

        assert!(
            repo.renew("occ", "node-a", Some(token), 600)
                .await
                .expect("renew"),
            "the holder must be able to extend its own lease"
        );
        let after = repo.get_by_id("occ").await.expect("get");
        assert!(
            after.claimed_until.expect("a lease") > repo.db_now().await.expect("now"),
            "a renewal must move the claim deadline into the future"
        );
        // And the occurrence is no longer reclaimable, which is the point.
        assert!(
            repo.claim_due(claim("node-b", 60))
                .await
                .expect("claim")
                .is_empty(),
            "a renewed occurrence must not be takeable"
        );

        // A wrong token renews nothing, however live the claim is.
        assert!(
            !repo
                .renew("occ", "node-a", Some(token + 1), 600)
                .await
                .expect("renew"),
            "a superseded token must extend nothing"
        );
    }

    /// Fail closed: when the database is gone, claiming must **error** rather
    /// than answer "nothing is due".
    ///
    /// The distinction is the whole safety property. An empty answer is
    /// indistinguishable from a healthy idle tick, so a worker would keep
    /// looping quietly while occurrences piled up — and, worse, a *singleton*
    /// read that failed open would let a second node start alongside a live
    /// holder. The worker's `Err` arm records `db_unavailable` and claims
    /// nothing; this pins that the arm is reachable.
    #[tokio::test]
    async fn a_dead_database_errors_rather_than_reporting_nothing_due() {
        let repo = test_repo().await;
        seed_due(&repo, "occ", "ch").await;
        // Sanity: there *is* work, so an empty answer below would be a lie
        // rather than the truth.
        assert_eq!(
            repo.claim_due(claim("node-a", 60))
                .await
                .expect("claim")
                .len(),
            1
        );

        // Close the underlying pool: every subsequent statement fails the way
        // an unreachable database does.
        let pool = match &repo.pool {
            crate::storage::DbPool::Sqlite(pool) => pool,
            _ => unreachable!("the in-memory test pool is SQLite"),
        };
        pool.close().await;

        assert!(
            repo.claim_due(claim("node-b", 60)).await.is_err(),
            "a claim against a dead database must fail, not report an empty queue"
        );
        assert!(
            repo.db_now().await.is_err(),
            "the clock read fails closed too, so a pass cannot plan against a guess"
        );
        assert!(
            repo.start_attempt(
                &CronOccurrence {
                    id: "occ".into(),
                    ..repo_row()
                },
                "node-b",
                1,
                Some(SingletonRequest {
                    key: "key",
                    holder: "node-b",
                    lease_secs: 60,
                }),
                60,
            )
            .await
            .is_err(),
            "singleton acquisition must fail closed rather than assume the key is free"
        );
    }

    /// A row shaped like the ledger's, for the calls that need one without
    /// reading it back.
    fn repo_row() -> CronOccurrence {
        let now = chrono::Utc::now().naive_utc();
        CronOccurrence {
            id: String::new(),
            channel_id: "ch".into(),
            channel_name: "nightly".into(),
            channel_version: 1,
            executing_version: None,
            workflow_id: Some("wf".into()),
            trigger: trigger::CRON.into(),
            scheduled_for: now,
            status: status::CLAIMED.into(),
            attempt: 1,
            claimed_by: Some("node-b".into()),
            claimed_until: None,
            singleton_key: None,
            fencing_token: None,
            trace_id: None,
            error_message: None,
            started_at: None,
            completed_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    // ---- settling ----

    #[tokio::test]
    async fn settling_requires_still_holding_the_claim() {
        let repo = test_repo().await;
        seed_due(&repo, "occ", "ch").await;
        let claimed = repo
            .claim_due(claim("node-a", 60))
            .await
            .expect("claim")
            .remove(0);
        start(&repo, &claimed, "node-a", None).await;

        assert!(
            !repo
                .settle(Settlement {
                    occurrence_id: "occ",
                    claimant: "node-b",
                    status: status::COMPLETED,
                    error_message: None,
                    trace_id: None,
                })
                .await
                .expect("settle"),
            "a node that does not hold the claim writes nothing"
        );

        assert!(
            repo.settle(Settlement {
                occurrence_id: "occ",
                claimant: "node-a",
                status: status::COMPLETED,
                error_message: None,
                trace_id: Some("trace-1"),
            })
            .await
            .expect("settle")
        );
        let row = repo.get_by_id("occ").await.expect("get");
        assert_eq!(row.status, status::COMPLETED);
        assert_eq!(row.trace_id.as_deref(), Some("trace-1"));
        assert!(row.completed_at.is_some());
        assert!(row.claimed_until.is_none(), "a settled row holds no lease");
    }

    #[tokio::test]
    async fn a_deferred_occurrence_returns_to_pending_and_is_claimable_again() {
        let repo = test_repo().await;
        seed_due(&repo, "occ", "ch").await;
        let claimed = repo
            .claim_due(claim("node-a", 60))
            .await
            .expect("claim")
            .remove(0);

        assert!(
            repo.release_claim(&claimed.id, "node-a")
                .await
                .expect("release")
        );
        let again = repo.claim_due(claim("node-b", 60)).await.expect("claim");
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].id, "occ");
    }

    // ---- retry ----

    #[tokio::test]
    async fn retry_keeps_the_occurrence_identity() {
        let repo = test_repo().await;
        let due = seed_due(&repo, "occ", "ch").await;
        let claimed = repo
            .claim_due(claim("node-a", 60))
            .await
            .expect("claim")
            .remove(0);
        start(&repo, &claimed, "node-a", None).await;
        repo.settle(Settlement {
            occurrence_id: "occ",
            claimant: "node-a",
            status: status::FAILED,
            error_message: Some("boom"),
            trace_id: None,
        })
        .await
        .expect("settle");

        let requeued = repo.requeue("occ").await.expect("requeue");
        assert_eq!(requeued.id, "occ");
        assert_eq!(requeued.status, status::PENDING);
        assert_eq!(
            requeued.scheduled_for, due,
            "a retry is another attempt at the same scheduled instant, not a new one"
        );
        assert_eq!(requeued.error_message, None);
        // And it is claimable again, as a second attempt.
        let again = repo.claim_due(claim("node-a", 60)).await.expect("claim");
        assert_eq!(again[0].attempt, 2);
    }

    /// Re-running finished work is not a retry — it is a new run, which is what
    /// the manual trigger is for.
    #[tokio::test]
    async fn a_completed_occurrence_cannot_be_retried() {
        let repo = test_repo().await;
        seed_due(&repo, "occ", "ch").await;
        let claimed = repo
            .claim_due(claim("node-a", 60))
            .await
            .expect("claim")
            .remove(0);
        start(&repo, &claimed, "node-a", None).await;
        repo.settle(Settlement {
            occurrence_id: "occ",
            claimant: "node-a",
            status: status::COMPLETED,
            error_message: None,
            trace_id: None,
        })
        .await
        .expect("settle");

        let err = repo.requeue("occ").await.expect_err("must refuse");
        assert!(matches!(err, OrionError::Conflict(_)), "{err:?}");
    }

    // ---- retention and reporting ----

    /// Retention removes history, never a backlog: a `pending` occurrence older
    /// than the window is work that has not happened yet.
    #[tokio::test]
    async fn retention_deletes_terminal_rows_and_keeps_pending_ones() {
        let repo = test_repo().await;
        let old = repo.db_now().await.expect("db now") - Duration::days(30);

        repo.insert_occurrence(occurrence("pending-old", "ch", old))
            .await
            .expect("insert");
        repo.insert_occurrence(NewOccurrence {
            status: status::COMPLETED,
            ..occurrence("done-old", "ch", old + Duration::seconds(1))
        })
        .await
        .expect("insert");

        // Both rows were created now, so a one-hour window keeps everything.
        assert_eq!(repo.delete_terminal_older_than(1).await.expect("delete"), 0);
        // Age both past the window: the terminal row goes and the pending one
        // stays, because a pending occurrence older than the retention window
        // is a backlog rather than history.
        age_row(&repo, "pending-old", Duration::days(30)).await;
        age_row(&repo, "done-old", Duration::days(30)).await;
        assert_eq!(repo.delete_terminal_older_than(1).await.expect("delete"), 1);
        let page = repo
            .list_paginated(&CronOccurrenceFilter::default())
            .await
            .expect("list");
        assert_eq!(page.total, 1);
        assert_eq!(page.data[0].id, "pending-old");
    }

    #[tokio::test]
    async fn the_listing_filters_by_channel_status_and_window() {
        let repo = test_repo().await;
        let now = repo.db_now().await.expect("db now");
        repo.insert_occurrence(occurrence("a", "ch-1", now - Duration::hours(2)))
            .await
            .expect("insert");
        repo.insert_occurrence(NewOccurrence {
            status: status::COMPLETED,
            ..occurrence("b", "ch-1", now - Duration::hours(1))
        })
        .await
        .expect("insert");
        repo.insert_occurrence(occurrence("c", "ch-2", now))
            .await
            .expect("insert");

        let by_channel = repo
            .list_paginated(&CronOccurrenceFilter {
                channel_id: Some("ch-1".into()),
                ..Default::default()
            })
            .await
            .expect("list");
        assert_eq!(by_channel.total, 2);

        let by_status = repo
            .list_paginated(&CronOccurrenceFilter {
                status: Some(status::COMPLETED.into()),
                ..Default::default()
            })
            .await
            .expect("list");
        assert_eq!(by_status.total, 1);
        assert_eq!(by_status.data[0].id, "b");

        let windowed = repo
            .list_paginated(&CronOccurrenceFilter {
                since: Some(now - Duration::minutes(90)),
                ..Default::default()
            })
            .await
            .expect("list");
        assert_eq!(windowed.total, 2);
    }

    #[tokio::test]
    async fn the_status_view_reads_the_newest_occurrence_and_the_backlog() {
        let repo = test_repo().await;
        let now = repo.db_now().await.expect("db now");
        assert_eq!(repo.pending_count(None).await.expect("count"), 0);
        assert_eq!(repo.oldest_pending_age_secs().await.expect("age"), None);

        repo.insert_occurrence(occurrence("old", "ch", now - Duration::minutes(10)))
            .await
            .expect("insert");
        repo.insert_occurrence(occurrence("new", "ch", now - Duration::minutes(1)))
            .await
            .expect("insert");
        repo.insert_occurrence(occurrence("other", "ch-2", now))
            .await
            .expect("insert");

        assert_eq!(repo.pending_count(None).await.expect("count"), 3);
        assert_eq!(repo.pending_count(Some("ch")).await.expect("count"), 2);

        let latest = repo
            .latest_for_channel("ch")
            .await
            .expect("latest")
            .expect("a row");
        assert_eq!(latest.channel_id, "ch");

        let age = repo
            .oldest_pending_age_secs()
            .await
            .expect("age")
            .expect("an age");
        assert!(
            (595..=605).contains(&age),
            "the oldest pending occurrence is ten minutes overdue, got {age}s"
        );

        assert!(
            repo.latest_for_channel("absent")
                .await
                .expect("latest")
                .is_none()
        );
    }

    /// Deleting a channel takes its ledger with it; archiving does not, which
    /// is why this is a separate call rather than part of the pause.
    #[tokio::test]
    async fn purging_a_channel_removes_its_cursor_and_its_occurrences() {
        let repo = test_repo().await;
        let now = repo.db_now().await.expect("db now");
        repo.insert_occurrence(occurrence("a", "ch", now))
            .await
            .expect("insert");
        repo.insert_occurrence(occurrence("b", "keep", now))
            .await
            .expect("insert");
        repo.upsert_cursor(ScheduleCursor {
            channel_id: "ch",
            channel_version: 1,
            config_hash: "hash",
            next_fire_at: now,
        })
        .await
        .expect("upsert");

        repo.purge_channel("ch").await.expect("purge");
        assert!(repo.schedule_states().await.expect("states").is_empty());
        let page = repo
            .list_paginated(&CronOccurrenceFilter::default())
            .await
            .expect("list");
        assert_eq!(page.total, 1);
        assert_eq!(page.data[0].channel_id, "keep");
    }

    // ---- the SQL itself ----

    /// The per-backend claim shapes, pinned the way the trace DLQ's are: the
    /// two that matter are that Postgres locks with SKIP LOCKED (so two nodes
    /// cannot claim one row mid-transaction) and that the limit travels as a
    /// bound value rather than inlined.
    #[test]
    fn per_backend_claim_shapes() {
        use sea_query::{MysqlQueryBuilder, PostgresQueryBuilder, SqliteQueryBuilder};

        let (sql, values) = claim_update_query(
            "node-a",
            25,
            "LOCALTIMESTAMP",
            "LOCALTIMESTAMP + interval '60 seconds'",
            true,
        )
        .build(PostgresQueryBuilder);
        assert!(sql.contains("RETURNING"), "{sql}");
        assert!(sql.contains("FOR UPDATE SKIP LOCKED"), "{sql}");
        assert!(sql.contains("\"scheduled_for\""), "{sql}");
        assert!(sql.contains("\"claimed_until\""), "{sql}");
        assert!(
            sql.contains("LIMIT $"),
            "limit must be a placeholder: {sql}"
        );
        assert!(
            values.iter().any(|v| *v == sea_query::Value::from(25u64)),
            "limit must travel as a bound value: {values:?}"
        );

        let (sql, _) = claim_update_query(
            "node-a",
            25,
            "datetime('now')",
            "datetime('now', '+60 seconds')",
            false,
        )
        .build(SqliteQueryBuilder);
        assert!(sql.contains("RETURNING"), "{sql}");
        assert!(
            !sql.contains("FOR UPDATE"),
            "SQLite has no row locks: {sql}"
        );

        // MySQL has SKIP LOCKED but no UPDATE … RETURNING, so it is two
        // statements inside one transaction.
        let (sql, _) = claim_select_query(25, "UTC_TIMESTAMP()").build(MysqlQueryBuilder);
        assert!(sql.contains("FOR UPDATE SKIP LOCKED"), "{sql}");
        let (sql, _) =
            lease_claimed_query("node-a", "UTC_TIMESTAMP()", ["a", "b"]).build(MysqlQueryBuilder);
        assert!(sql.contains("`claimed_by`"), "{sql}");
        assert!(!sql.contains("RETURNING"), "{sql}");
    }

    /// `trigger` is a reserved word in MySQL, so the column has to come out
    /// quoted or every insert fails on that backend alone.
    #[test]
    fn the_trigger_column_is_quoted_on_every_backend() {
        use sea_query::{Iden, MysqlQueryBuilder, PostgresQueryBuilder, SqliteQueryBuilder};
        assert_eq!(Iden::to_string(&CronOccurrences::Trigger), "trigger");

        let insert = || {
            Query::insert()
                .into_table(CronOccurrences::Table)
                .columns([CronOccurrences::Id, CronOccurrences::Trigger])
                .values_panic(["a".into(), trigger::CRON.into()])
                .to_owned()
        };
        assert!(insert().build(MysqlQueryBuilder).0.contains("`trigger`"));
        assert!(
            insert()
                .build(PostgresQueryBuilder)
                .0
                .contains("\"trigger\"")
        );
        assert!(insert().build(SqliteQueryBuilder).0.contains("\"trigger\""));
    }
}
