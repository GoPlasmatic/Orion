use async_trait::async_trait;
use sea_query::{Asterisk, Condition, Expr, IntoIden, Query, SimpleExpr};

use crate::errors::OrionError;
// D28: both row shapes this repository reads — `TraceDlqEntry` and the
// payload-free `TraceDlqSummary` — live in `storage::models::rows`, like every
// other row struct. Only the request-side filter is defined here.
use crate::storage::models::{TraceDlqEntry, TraceDlqSummary};
use crate::storage::schema::TraceDlq;
use crate::storage::{DbBackend, DbPool, build_sqlx};

use super::helpers::{Page, PaginatedResult, Projection};

// -- Request DTOs --

#[derive(Debug, Clone, Default)]
pub struct TraceDlqFilter {
    pub channel: Option<String>,
    /// `Some(true)` = only exhausted entries (`retry_count >= max_retries`),
    /// `Some(false)` = only entries with retries left, `None` = both.
    pub exhausted: Option<bool>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Exhaustion is a column-to-column comparison, built from the Iden enum in
/// one place so the filter, `claim_pending` and `purge_exhausted` can never
/// drift apart (and a rename in `schema.rs` cannot fail only at runtime, D25).
fn exhausted() -> SimpleExpr {
    Expr::col(TraceDlq::RetryCount).gte(Expr::col(TraceDlq::MaxRetries))
}
fn not_exhausted() -> SimpleExpr {
    Expr::col(TraceDlq::RetryCount).lt(Expr::col(TraceDlq::MaxRetries))
}

/// Release the claim lease on an entry: the `claimed_by` / `claimed_until`
/// pair every UPDATE that hands an entry back has to clear.
///
/// `claimed_until` is a timestamp: a NULL *string* param is a 42804 type
/// mismatch on postgres (D1). Bind the chrono NULL, same as `next_retry_at`
/// binds a chrono value.
fn clear_lease(q: &mut sea_query::UpdateStatement) -> &mut sea_query::UpdateStatement {
    q.value(
        TraceDlq::ClaimedBy,
        super::helpers::optional_string_value(None),
    )
    .value(
        TraceDlq::ClaimedUntil,
        sea_query::Value::ChronoDateTime(None),
    )
}

/// The 9 columns a DLQ *listing* reads, in
/// [`crate::storage::models::TraceDlqSummary`] order.
fn summary_columns() -> [TraceDlq; 9] {
    [
        TraceDlq::Id,
        TraceDlq::TraceId,
        TraceDlq::Channel,
        TraceDlq::ErrorMessage,
        TraceDlq::RetryCount,
        TraceDlq::MaxRetries,
        TraceDlq::NextRetryAt,
        TraceDlq::CreatedAt,
        TraceDlq::UpdatedAt,
    ]
}

/// The page `list_paginated` reads. A function, not an inline block, so a test
/// can assert the exact SQL the repository runs.
fn list_page(filter: &TraceDlqFilter) -> Page {
    let (limit, offset) = super::helpers::clamp_pagination(filter.limit, filter.offset);
    Page {
        from: TraceDlq::Table.into_iden(),
        // Narrower than the table on purpose: no `payload_json`, no
        // `metadata_json`. See `models::rows::TraceDlqSummary`.
        projection: Projection::Columns(
            summary_columns()
                .into_iter()
                .map(IntoIden::into_iden)
                .collect(),
        ),
        cond: filter.condition(),
        sort: TraceDlq::CreatedAt.into_iden(),
        order: sea_query::Order::Desc,
        limit,
        offset,
    }
}

impl TraceDlqFilter {
    fn condition(&self) -> Condition {
        let mut cond = Condition::all();
        if let Some(ref channel) = self.channel {
            cond = cond.add(Expr::col(TraceDlq::Channel).eq(channel.as_str()));
        }
        match self.exhausted {
            Some(true) => cond = cond.add(exhausted()),
            Some(false) => cond = cond.add(not_exhausted()),
            None => {}
        }
        cond
    }
}

// -- Repository trait --

#[async_trait]
pub trait TraceDlqRepository: Send + Sync {
    /// Enqueue a failed trace for later retry.
    ///
    /// `retry_count` seeds the new row rather than always starting at 0: the
    /// retry loop deletes a row once resubmission succeeds, so a message that
    /// fails again must re-enter carrying what its lineage already spent, or
    /// `max_retries` is never reached (Q3). A row seeded at
    /// `retry_count >= max_retries` is born exhausted — `claim_pending` skips
    /// it, which is exactly the state `mark_exhausted` writes.
    #[allow(clippy::too_many_arguments)]
    async fn enqueue(
        &self,
        trace_id: &str,
        channel: &str,
        payload_json: &str,
        metadata_json: &str,
        error_message: &str,
        retry_count: i64,
        max_retries: i64,
    ) -> Result<TraceDlqEntry, OrionError>;

    /// Atomically claim up to `limit` due, unleased entries for `claimant`,
    /// leasing them until now + `lease_secs`. Due = `next_retry_at <= now`,
    /// `retry_count < max_retries`, and no live lease (`claimed_until` NULL
    /// or expired). Expired leases are re-claimable, so a crashed claimant's
    /// entries recover declaratively. All time comparisons use the DB clock.
    async fn claim_pending(
        &self,
        claimant: &str,
        limit: i64,
        lease_secs: u64,
    ) -> Result<Vec<TraceDlqEntry>, OrionError>;

    /// Increment retry count and set next retry time for a DLQ entry.
    /// Releases any claim lease.
    async fn record_retry(
        &self,
        id: &str,
        next_retry_at: chrono::NaiveDateTime,
    ) -> Result<(), OrionError>;

    /// Remove an entry after successful retry.
    async fn remove(&self, id: &str) -> Result<(), OrionError>;

    /// Mark an entry as permanently failed by setting retry_count = max_retries.
    async fn mark_exhausted(&self, id: &str) -> Result<(), OrionError>;

    // -- Operator surface (O4) --

    /// Page through DLQ entries, newest first, without their payloads.
    async fn list_paginated(
        &self,
        filter: &TraceDlqFilter,
    ) -> Result<PaginatedResult<TraceDlqSummary>, OrionError>;

    /// Count matching entries. Cheap enough to run on every DLQ retry tick,
    /// which is where the `trace_dlq_depth` gauge is refreshed.
    async fn count(&self, filter: &TraceDlqFilter) -> Result<i64, OrionError>;

    /// Fetch one entry with its full payload and metadata.
    async fn get_by_id(&self, id: &str) -> Result<TraceDlqEntry, OrionError>;

    /// Reset an entry for immediate retry: `retry_count = 0`, due now, lease
    /// released. The operator's manual replay for entries the automatic
    /// backoff already gave up on.
    async fn requeue(&self, id: &str) -> Result<TraceDlqEntry, OrionError>;

    /// Delete exhausted entries older than `older_than_hours` (0 = every
    /// exhausted entry). Nothing else removes them, so without this they
    /// accumulate forever with full payloads.
    async fn purge_exhausted(&self, older_than_hours: u64) -> Result<u64, OrionError>;
}

// -- claim_pending query builders (D25) --
//
// Free functions so the per-backend SQL shapes can be pinned by a unit test
// (see `per_backend_sql_shapes` below, after cluster.rs). Column identifiers
// come from the Iden enum and `limit` is a bound parameter; the only raw SQL
// is the backend clock expressions (`sql_now` / `sql_now_plus_secs`), per the
// cluster.rs convention.

/// Due = past `next_retry_at`, retries left, and no live lease
/// (`claimed_until` NULL or expired). All time comparisons use the DB clock.
fn due_condition(now: &str) -> Condition {
    Condition::all()
        .add(Expr::col(TraceDlq::NextRetryAt).lte(Expr::cust(now)))
        .add(not_exhausted())
        .add(
            Condition::any()
                .add(Expr::col(TraceDlq::ClaimedUntil).is_null())
                .add(Expr::col(TraceDlq::ClaimedUntil).lt(Expr::cust(now))),
        )
}

/// Single-statement claim for the backends with `UPDATE … RETURNING`
/// (Postgres, SQLite): lease the `limit` oldest due rows and return them.
/// `skip_locked` adds `FOR UPDATE SKIP LOCKED` to the inner select — required
/// on Postgres so two nodes cannot claim the same rows even mid-transaction;
/// SQLite is single-host by construction (D2) and serializes writes itself.
fn claim_update_query(
    claimant: &str,
    limit: i64,
    now: &str,
    lease_until: &str,
    skip_locked: bool,
) -> sea_query::UpdateStatement {
    let mut due_ids = Query::select()
        .column(TraceDlq::Id)
        .from(TraceDlq::Table)
        .cond_where(due_condition(now))
        .order_by(TraceDlq::NextRetryAt, sea_query::Order::Asc)
        .limit(limit.max(0) as u64)
        .to_owned();
    if skip_locked {
        due_ids.lock_with_behavior(
            sea_query::LockType::Update,
            sea_query::LockBehavior::SkipLocked,
        );
    }
    let mut update = Query::update()
        .table(TraceDlq::Table)
        .value(TraceDlq::ClaimedBy, claimant)
        .value(TraceDlq::ClaimedUntil, Expr::cust(lease_until))
        .and_where(Expr::col(TraceDlq::Id).in_subquery(due_ids))
        .to_owned();
    update.returning_all();
    update
}

/// MySQL claim, step 1: select the full due rows `FOR UPDATE SKIP LOCKED`.
/// MySQL 8 has SKIP LOCKED but no `UPDATE … RETURNING`, and the model carries
/// no lease columns, so the pre-UPDATE rows are already what the caller
/// needs — no read-back.
fn claim_select_query(limit: i64, now: &str) -> sea_query::SelectStatement {
    let mut select = Query::select()
        .column(Asterisk)
        .from(TraceDlq::Table)
        .cond_where(due_condition(now))
        .order_by(TraceDlq::NextRetryAt, sea_query::Order::Asc)
        .limit(limit.max(0) as u64)
        .to_owned();
    select.lock_with_behavior(
        sea_query::LockType::Update,
        sea_query::LockBehavior::SkipLocked,
    );
    select
}

/// MySQL claim, step 2: lease the selected rows by id.
fn lease_claimed_query<'a>(
    claimant: &str,
    lease_until: &str,
    ids: impl IntoIterator<Item = &'a str>,
) -> sea_query::UpdateStatement {
    Query::update()
        .table(TraceDlq::Table)
        .value(TraceDlq::ClaimedBy, claimant)
        .value(TraceDlq::ClaimedUntil, Expr::cust(lease_until))
        .and_where(Expr::col(TraceDlq::Id).is_in(ids))
        .to_owned()
}

// -- SQL implementation --

pub struct SqlTraceDlqRepository {
    pool: DbPool,
}

impl SqlTraceDlqRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TraceDlqRepository for SqlTraceDlqRepository {
    async fn enqueue(
        &self,
        trace_id: &str,
        channel: &str,
        payload_json: &str,
        metadata_json: &str,
        error_message: &str,
        retry_count: i64,
        max_retries: i64,
    ) -> Result<TraceDlqEntry, OrionError> {
        crate::metrics::timed_db_op("trace_dlq.enqueue", async {
            let id = uuid::Uuid::new_v4().to_string();

            // First retry after 1 second. Bind chrono values, not strings:
            // postgres rejects TEXT parameters against timestamp columns.
            let next_retry = chrono::Utc::now()
                .naive_utc()
                .checked_add_signed(chrono::Duration::seconds(1))
                .unwrap_or(chrono::Utc::now().naive_utc());

            let (sql, values) = build_sqlx(
                Query::insert()
                    .into_table(TraceDlq::Table)
                    .columns([
                        TraceDlq::Id,
                        TraceDlq::TraceId,
                        TraceDlq::Channel,
                        TraceDlq::PayloadJson,
                        TraceDlq::MetadataJson,
                        TraceDlq::ErrorMessage,
                        TraceDlq::RetryCount,
                        TraceDlq::MaxRetries,
                        TraceDlq::NextRetryAt,
                    ])
                    .values_panic([
                        Expr::val(id.as_str()).into(),
                        Expr::val(trace_id).into(),
                        Expr::val(channel).into(),
                        Expr::val(payload_json).into(),
                        Expr::val(metadata_json).into(),
                        Expr::val(error_message).into(),
                        Expr::val(retry_count.max(0)).into(),
                        Expr::val(max_retries).into(),
                        Expr::val(next_retry).into(),
                    ]),
            );

            self.pool.execute_query(&sql, values).await?;

            // Fetch the inserted entry
            let (sql, values) = build_sqlx(
                Query::select()
                    .column(Asterisk)
                    .from(TraceDlq::Table)
                    .and_where(Expr::col(TraceDlq::Id).eq(id.as_str())),
            );

            self.pool
                .fetch_one_as::<TraceDlqEntry>(&sql, values)
                .await
                .map_err(|e| OrionError::Internal {
                    context: "Failed to fetch inserted DLQ entry".to_string(),
                    source: Some(Box::new(e)),
                })
        })
        .await
    }

    async fn claim_pending(
        &self,
        claimant: &str,
        limit: i64,
        lease_secs: u64,
    ) -> Result<Vec<TraceDlqEntry>, OrionError> {
        crate::metrics::timed_db_op("trace_dlq.claim_pending", async {
            let backend = crate::storage::get_backend();
            let now = super::helpers::sql_now(backend);
            let lease_until = super::helpers::sql_now_plus_secs(backend, lease_secs);
            match backend {
                DbBackend::Postgres | DbBackend::Sqlite => {
                    let (sql, values) = build_sqlx(&mut claim_update_query(
                        claimant,
                        limit,
                        now,
                        &lease_until,
                        backend == DbBackend::Postgres,
                    ));
                    Ok(self
                        .pool
                        .fetch_all_as::<TraceDlqEntry>(&sql, values)
                        .await?)
                }
                DbBackend::Mysql => {
                    let mut tx = self.pool.begin_tx().await.map_err(OrionError::Storage)?;
                    let (sql, values) = build_sqlx(&mut claim_select_query(limit, now));
                    let rows: Vec<TraceDlqEntry> = tx.fetch_all_as(&sql, values).await?;
                    if rows.is_empty() {
                        tx.commit().await.map_err(OrionError::Storage)?;
                        return Ok(rows);
                    }
                    let (sql, values) = build_sqlx(&mut lease_claimed_query(
                        claimant,
                        &lease_until,
                        rows.iter().map(|r| r.id.as_str()),
                    ));
                    tx.execute_query(&sql, values).await?;
                    tx.commit().await.map_err(OrionError::Storage)?;
                    Ok(rows)
                }
            }
        })
        .await
    }

    async fn record_retry(
        &self,
        id: &str,
        next_retry_at: chrono::NaiveDateTime,
    ) -> Result<(), OrionError> {
        crate::metrics::timed_db_op("trace_dlq.record_retry", async {
            let (sql, values) = build_sqlx(
                clear_lease(
                    Query::update()
                        .table(TraceDlq::Table)
                        .value(TraceDlq::RetryCount, Expr::col(TraceDlq::RetryCount).add(1))
                        .value(TraceDlq::NextRetryAt, next_retry_at),
                )
                .and_where(Expr::col(TraceDlq::Id).eq(id)),
            );

            self.pool.execute_query(&sql, values).await?;
            Ok(())
        })
        .await
    }

    async fn remove(&self, id: &str) -> Result<(), OrionError> {
        crate::metrics::timed_db_op("trace_dlq.remove", async {
            let (sql, values) = build_sqlx(
                Query::delete()
                    .from_table(TraceDlq::Table)
                    .and_where(Expr::col(TraceDlq::Id).eq(id)),
            );

            self.pool.execute_query(&sql, values).await?;
            Ok(())
        })
        .await
    }

    async fn mark_exhausted(&self, id: &str) -> Result<(), OrionError> {
        crate::metrics::timed_db_op("trace_dlq.mark_exhausted", async {
            let (sql, values) = build_sqlx(
                clear_lease(
                    Query::update()
                        .table(TraceDlq::Table)
                        .value(TraceDlq::RetryCount, Expr::col(TraceDlq::MaxRetries)),
                )
                .and_where(Expr::col(TraceDlq::Id).eq(id)),
            );

            self.pool.execute_query(&sql, values).await?;
            Ok(())
        })
        .await
    }

    async fn list_paginated(
        &self,
        filter: &TraceDlqFilter,
    ) -> Result<PaginatedResult<TraceDlqSummary>, OrionError> {
        crate::metrics::timed_db_op("trace_dlq.list_paginated", async {
            super::helpers::paginate(&self.pool, list_page(filter)).await
        })
        .await
    }

    async fn count(&self, filter: &TraceDlqFilter) -> Result<i64, OrionError> {
        crate::metrics::timed_db_op("trace_dlq.count", async {
            super::helpers::count_where(&self.pool, TraceDlq::Table, filter.condition()).await
        })
        .await
    }

    async fn get_by_id(&self, id: &str) -> Result<TraceDlqEntry, OrionError> {
        crate::metrics::timed_db_op("trace_dlq.get_by_id", async {
            let (sql, values) = build_sqlx(
                Query::select()
                    .column(Asterisk)
                    .from(TraceDlq::Table)
                    .and_where(Expr::col(TraceDlq::Id).eq(id)),
            );
            super::helpers::fetch_required::<TraceDlqEntry>(&self.pool, &sql, values, || {
                OrionError::NotFound(format!("DLQ entry '{id}' not found"))
            })
            .await
        })
        .await
    }

    async fn requeue(&self, id: &str) -> Result<TraceDlqEntry, OrionError> {
        crate::metrics::timed_db_op("trace_dlq.requeue", async {
            let now = chrono::Utc::now().naive_utc();
            let (sql, values) = build_sqlx(
                clear_lease(
                    Query::update()
                        .table(TraceDlq::Table)
                        .value(TraceDlq::RetryCount, 0i64)
                        .value(TraceDlq::NextRetryAt, now),
                )
                .and_where(Expr::col(TraceDlq::Id).eq(id)),
            );

            if self.pool.execute_query(&sql, values).await? == 0 {
                return Err(OrionError::NotFound(format!("DLQ entry '{id}' not found")));
            }
            self.get_by_id(id).await
        })
        .await
    }

    async fn purge_exhausted(&self, older_than_hours: u64) -> Result<u64, OrionError> {
        crate::metrics::timed_db_op("trace_dlq.purge_exhausted", async {
            let cutoff = chrono::Utc::now()
                .naive_utc()
                .checked_sub_signed(chrono::Duration::hours(older_than_hours as i64))
                .unwrap_or(chrono::NaiveDateTime::MIN);

            // D6: chunked — see `delete_chunked`.
            super::helpers::delete_chunked(
                &self.pool,
                TraceDlq::Table,
                TraceDlq::Id,
                Condition::all()
                    .add(exhausted())
                    .add(Expr::col(TraceDlq::CreatedAt).lt(cutoff)),
            )
            .await
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_repo() -> SqlTraceDlqRepository {
        SqlTraceDlqRepository::new(crate::storage::test_sqlite_pool().await)
    }

    /// The listing must never read the failed request's body. Asserted against
    /// the statement `list_paginated` actually runs, because `TraceDlqSummary`
    /// would decode a `SELECT *` just as happily — sqlx ignores extra columns.
    #[test]
    fn list_projection_never_reads_the_payload_columns() {
        crate::storage::set_backend_for_test(crate::storage::DbBackend::Sqlite);
        let sql = super::super::helpers::page_select(&list_page(&TraceDlqFilter::default()))
            .to_string(sea_query::SqliteQueryBuilder);
        for withheld in ["payload_json", "metadata_json"] {
            assert!(
                !sql.contains(withheld),
                "the DLQ listing projection names `{withheld}`: {sql}"
            );
        }
        assert!(!sql.contains('*'), "{sql}");
    }

    /// Entries become due 1s after enqueue — backdate to now-2s.
    async fn make_due(repo: &SqlTraceDlqRepository, id: &str) {
        let DbPool::Sqlite(p) = &repo.pool else {
            unreachable!("sqlite expected");
        };
        sqlx::query(
            "UPDATE trace_dlq SET next_retry_at = datetime('now', '-2 seconds') WHERE id = ?",
        )
        .bind(id)
        .execute(p)
        .await
        .expect("backdate");
    }

    async fn enqueue_due(repo: &SqlTraceDlqRepository, trace_id: &str) -> String {
        let entry = repo
            .enqueue(trace_id, "orders", "{}", "{}", "boom", 0, 5)
            .await
            .expect("enqueue");
        make_due(repo, &entry.id).await;
        entry.id
    }

    #[tokio::test]
    async fn test_claim_leases_and_blocks_second_claimant() {
        let repo = test_repo().await;
        let id = enqueue_due(&repo, "t1").await;

        let claimed = repo.claim_pending("node-a", 10, 60).await.expect("claim");
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, id);

        // Leased — a second claim (any claimant) gets nothing.
        let claimed = repo.claim_pending("node-b", 10, 60).await.expect("claim");
        assert!(claimed.is_empty());
    }

    #[tokio::test]
    async fn test_expired_lease_is_reclaimable() {
        let repo = test_repo().await;
        enqueue_due(&repo, "t1").await;
        assert_eq!(
            repo.claim_pending("node-a", 10, 60)
                .await
                .expect("claim")
                .len(),
            1
        );

        // Force-expire the lease → reclaimable by another node.
        let DbPool::Sqlite(p) = &repo.pool else {
            unreachable!("sqlite expected");
        };
        sqlx::query("UPDATE trace_dlq SET claimed_until = datetime('now', '-1 seconds')")
            .execute(p)
            .await
            .expect("expire");
        assert_eq!(
            repo.claim_pending("node-b", 10, 60)
                .await
                .expect("claim")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn test_record_retry_clears_lease() {
        let repo = test_repo().await;
        let id = enqueue_due(&repo, "t1").await;
        assert_eq!(
            repo.claim_pending("node-a", 10, 60)
                .await
                .expect("claim")
                .len(),
            1
        );

        // record_retry releases the lease; once due again it is claimable.
        let past = chrono::Utc::now()
            .naive_utc()
            .checked_sub_signed(chrono::Duration::seconds(2))
            .expect("past");
        repo.record_retry(&id, past).await.expect("retry");
        let claimed = repo.claim_pending("node-b", 10, 60).await.expect("claim");
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].retry_count, 1);
    }

    /// Q3 regression. Replays the retry loop's full cycle — claim, resubmit
    /// (carrying `retry_count + 1`), delete the old row, worker fails again,
    /// re-enqueue seeded with the carried count — and asserts it terminates.
    /// Before the carry, every re-enqueue landed at 0 and this loop never
    /// stopped producing claimable rows.
    #[tokio::test]
    async fn test_poison_message_converges_on_max_retries() {
        let repo = test_repo().await;
        let max_retries = 3;

        let first = repo
            .enqueue("t-poison", "orders", "{}", "{}", "boom", 0, max_retries)
            .await
            .expect("enqueue");
        make_due(&repo, &first.id).await;

        let mut cycles = 0;
        while let Some(claimed) = repo
            .claim_pending("node-a", 10, 60)
            .await
            .expect("claim")
            .into_iter()
            .next()
        {
            cycles += 1;
            assert!(
                cycles <= max_retries + 1,
                "poison message is still claimable after {cycles} cycles"
            );

            let carried = claimed.retry_count + 1;
            repo.remove(&claimed.id).await.expect("remove");
            let requeued = repo
                .enqueue(
                    "t-poison",
                    "orders",
                    "{}",
                    "{}",
                    "boom",
                    carried,
                    max_retries,
                )
                .await
                .expect("re-enqueue");
            assert_eq!(requeued.retry_count, carried, "carried count must persist");
            make_due(&repo, &requeued.id).await;
        }

        assert_eq!(
            cycles, max_retries,
            "a poison message must be retried exactly max_retries times"
        );
    }

    /// The Postgres/MySQL arms cannot execute without containers (CI covers
    /// that); pin the rendered SQL shapes so a sea-query upgrade, a schema
    /// rename, or a regression to formatting `limit` into the statement text
    /// fails here first (D25).
    #[test]
    fn per_backend_sql_shapes() {
        use sea_query::{MysqlQueryBuilder, PostgresQueryBuilder, SqliteQueryBuilder};

        // Postgres: one UPDATE … RETURNING over a SKIP LOCKED subselect, with
        // identifiers from the Iden enum and the limit bound, never inlined.
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
        assert!(sql.contains("\"next_retry_at\""), "{sql}");
        assert!(sql.contains("\"claimed_until\""), "{sql}");
        assert!(
            sql.contains("LIMIT $"),
            "limit must be a placeholder: {sql}"
        );
        assert!(!sql.contains("25"), "limit must not be inlined: {sql}");
        assert!(
            values.iter().any(|v| *v == sea_query::Value::from(25u64)),
            "limit must travel as a bound value: {values:?}"
        );

        // SQLite: same statement without the locking clause.
        let (sql, _) = claim_update_query(
            "node-a",
            25,
            "datetime('now')",
            "datetime('now', '+60 seconds')",
            false,
        )
        .build(SqliteQueryBuilder);
        assert!(sql.contains("RETURNING"), "{sql}");
        assert!(!sql.contains("FOR UPDATE"), "{sql}");
        assert!(
            sql.contains("LIMIT ?"),
            "limit must be a placeholder: {sql}"
        );

        // MySQL: SELECT … FOR UPDATE SKIP LOCKED, then the lease UPDATE.
        let (sql, _) = claim_select_query(25, "UTC_TIMESTAMP()").build(MysqlQueryBuilder);
        assert!(sql.contains("FOR UPDATE SKIP LOCKED"), "{sql}");
        assert!(sql.contains("`next_retry_at`"), "{sql}");
        assert!(
            sql.contains("LIMIT ?"),
            "limit must be a placeholder: {sql}"
        );

        let (sql, _) = lease_claimed_query(
            "node-a",
            "DATE_ADD(UTC_TIMESTAMP(), INTERVAL 60 SECOND)",
            ["id-1", "id-2"],
        )
        .build(MysqlQueryBuilder);
        assert!(sql.contains("`claimed_by`"), "{sql}");
        assert!(sql.contains("IN (?, ?)"), "{sql}");
    }

    #[tokio::test]
    async fn test_claim_respects_batch_limit() {
        let repo = test_repo().await;
        for i in 0..5 {
            enqueue_due(&repo, &format!("t{i}")).await;
        }
        assert_eq!(
            repo.claim_pending("node-a", 3, 60)
                .await
                .expect("claim")
                .len(),
            3
        );
        assert_eq!(
            repo.claim_pending("node-a", 3, 60)
                .await
                .expect("claim")
                .len(),
            2
        );
    }
}
