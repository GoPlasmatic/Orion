use async_trait::async_trait;
use sea_query::{Asterisk, Condition, Expr, Query};

use crate::errors::OrionError;
use crate::storage::models::TraceDlqEntry;
use crate::storage::schema::TraceDlq;
use crate::storage::{DbPool, build_sqlx};

use super::helpers::PaginatedResult;

// -- DTOs --

/// List-view projection. Deliberately omits `payload_json` / `metadata_json`:
/// a DLQ listing would otherwise dump every failed request's body (and, until
/// S10 lands, its headers) into one response. Payloads are served one at a
/// time by `get_by_id`.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct TraceDlqSummary {
    pub id: String,
    pub trace_id: String,
    pub channel: String,
    pub error_message: String,
    pub retry_count: i64,
    pub max_retries: i64,
    pub next_retry_at: chrono::NaiveDateTime,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

#[derive(Debug, Clone, Default)]
pub struct TraceDlqFilter {
    pub channel: Option<String>,
    /// `Some(true)` = only exhausted entries (`retry_count >= max_retries`),
    /// `Some(false)` = only entries with retries left, `None` = both.
    pub exhausted: Option<bool>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Exhaustion is a column-to-column comparison, expressed the same way
/// `claim_pending` expresses it so the two can never drift apart.
const EXHAUSTED: &str = "retry_count >= max_retries";
const NOT_EXHAUSTED: &str = "retry_count < max_retries";

impl TraceDlqFilter {
    fn condition(&self) -> Condition {
        let mut cond = Condition::all();
        if let Some(ref channel) = self.channel {
            cond = cond.add(Expr::col(TraceDlq::Channel).eq(channel.as_str()));
        }
        match self.exhausted {
            Some(true) => cond = cond.add(Expr::cust(EXHAUSTED)),
            Some(false) => cond = cond.add(Expr::cust(NOT_EXHAUSTED)),
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
                .map_err(|e| OrionError::InternalSource {
                    context: "Failed to fetch inserted DLQ entry".to_string(),
                    source: Box::new(e),
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
        use sea_query::{Value, Values};
        use sea_query_binder::SqlxValues;

        crate::metrics::timed_db_op("trace_dlq.claim_pending", async {
            let backend = crate::storage::get_backend();
            let now = super::helpers::sql_now(backend);
            let lease_until = super::helpers::sql_now_plus_secs(backend, lease_secs);
            // Due = past next_retry_at, retries left, and no live lease.
            let due = format!(
                "next_retry_at <= {now} AND retry_count < max_retries \
                 AND (claimed_until IS NULL OR claimed_until < {now})"
            );
            match backend {
                crate::storage::DbBackend::Postgres => {
                    // Single statement: SKIP LOCKED prevents two nodes from
                    // claiming the same rows even mid-transaction.
                    let sql = format!(
                        "UPDATE trace_dlq SET claimed_by = $1, claimed_until = {lease_until} \
                         WHERE id IN ( \
                             SELECT id FROM trace_dlq WHERE {due} \
                             ORDER BY next_retry_at ASC LIMIT {limit} \
                             FOR UPDATE SKIP LOCKED) \
                         RETURNING *"
                    );
                    Ok(self
                        .pool
                        .fetch_all_as::<TraceDlqEntry>(
                            &sql,
                            SqlxValues(Values(vec![Value::from(claimant)])),
                        )
                        .await?)
                }
                crate::storage::DbBackend::Mysql => {
                    // MySQL 8 has SKIP LOCKED but no UPDATE ... RETURNING:
                    // select-for-update the full rows, then lease them. The
                    // model carries no lease columns, so the pre-UPDATE rows
                    // are already what the caller needs — no read-back.
                    let mut tx = self.pool.begin_tx().await.map_err(OrionError::Storage)?;
                    let rows: Vec<TraceDlqEntry> = tx
                        .fetch_all_as(
                            &format!(
                                "SELECT * FROM trace_dlq WHERE {due} \
                                 ORDER BY next_retry_at ASC LIMIT {limit} \
                                 FOR UPDATE SKIP LOCKED"
                            ),
                            SqlxValues(Values(Vec::new())),
                        )
                        .await?;
                    if rows.is_empty() {
                        tx.commit().await.map_err(OrionError::Storage)?;
                        return Ok(rows);
                    }
                    let placeholders = vec!["?"; rows.len()].join(", ");
                    let mut update_values: Vec<Value> = vec![Value::from(claimant)];
                    update_values.extend(rows.iter().map(|r| Value::from(r.id.as_str())));
                    tx.execute_query(
                        &format!(
                            "UPDATE trace_dlq SET claimed_by = ?, claimed_until = {lease_until} \
                             WHERE id IN ({placeholders})"
                        ),
                        SqlxValues(Values(update_values)),
                    )
                    .await?;
                    tx.commit().await.map_err(OrionError::Storage)?;
                    Ok(rows)
                }
                crate::storage::DbBackend::Sqlite => {
                    // Single-host by construction (D2): no locking clause
                    // needed, writes are serialized by SQLite itself.
                    let sql = format!(
                        "UPDATE trace_dlq SET claimed_by = ?, claimed_until = {lease_until} \
                         WHERE id IN ( \
                             SELECT id FROM trace_dlq WHERE {due} \
                             ORDER BY next_retry_at ASC LIMIT {limit}) \
                         RETURNING *"
                    );
                    Ok(self
                        .pool
                        .fetch_all_as::<TraceDlqEntry>(
                            &sql,
                            SqlxValues(Values(vec![Value::from(claimant)])),
                        )
                        .await?)
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
                Query::update()
                    .table(TraceDlq::Table)
                    .value(TraceDlq::RetryCount, Expr::col(TraceDlq::RetryCount).add(1))
                    .value(TraceDlq::NextRetryAt, next_retry_at)
                    .value(
                        TraceDlq::ClaimedBy,
                        super::helpers::optional_string_value(None),
                    )
                    // `claimed_until` is a timestamp: a NULL *string* param is
                    // a 42804 type mismatch on postgres (D1). Bind the chrono
                    // NULL, same as `next_retry_at` binds a chrono value.
                    .value(
                        TraceDlq::ClaimedUntil,
                        sea_query::Value::ChronoDateTime(None),
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
                Query::update()
                    .table(TraceDlq::Table)
                    .value(TraceDlq::RetryCount, Expr::col(TraceDlq::MaxRetries))
                    .value(
                        TraceDlq::ClaimedBy,
                        super::helpers::optional_string_value(None),
                    )
                    // See `record_retry`: chrono NULL, not a string NULL (D1).
                    .value(
                        TraceDlq::ClaimedUntil,
                        sea_query::Value::ChronoDateTime(None),
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
            let (limit, offset) = super::helpers::clamp_pagination(filter.limit, filter.offset);
            let cond = filter.condition();

            let total =
                super::helpers::count_where(&self.pool, TraceDlq::Table, cond.clone()).await?;

            let (sql, values) = build_sqlx(
                Query::select()
                    .columns([
                        TraceDlq::Id,
                        TraceDlq::TraceId,
                        TraceDlq::Channel,
                        TraceDlq::ErrorMessage,
                        TraceDlq::RetryCount,
                        TraceDlq::MaxRetries,
                        TraceDlq::NextRetryAt,
                        TraceDlq::CreatedAt,
                        TraceDlq::UpdatedAt,
                    ])
                    .from(TraceDlq::Table)
                    .cond_where(cond)
                    .order_by(TraceDlq::CreatedAt, sea_query::Order::Desc)
                    .limit(limit as u64)
                    .offset(offset as u64),
            );
            let data = self
                .pool
                .fetch_all_as::<TraceDlqSummary>(&sql, values)
                .await?;

            Ok(PaginatedResult {
                data,
                total,
                limit,
                offset,
            })
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
                Query::update()
                    .table(TraceDlq::Table)
                    .value(TraceDlq::RetryCount, 0i64)
                    .value(TraceDlq::NextRetryAt, now)
                    .value(
                        TraceDlq::ClaimedBy,
                        super::helpers::optional_string_value(None),
                    )
                    // See `record_retry`: chrono NULL, not a string NULL (D1).
                    .value(
                        TraceDlq::ClaimedUntil,
                        sea_query::Value::ChronoDateTime(None),
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

            let (sql, values) = build_sqlx(
                Query::delete()
                    .from_table(TraceDlq::Table)
                    .and_where(Expr::cust(EXHAUSTED))
                    .and_where(Expr::col(TraceDlq::CreatedAt).lt(cutoff)),
            );

            Ok(self.pool.execute_query(&sql, values).await?)
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
