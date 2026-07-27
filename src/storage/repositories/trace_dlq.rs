use async_trait::async_trait;
use sea_query::{Asterisk, Expr, Query};

use crate::errors::OrionError;
use crate::storage::models::TraceDlqEntry;
use crate::storage::schema::TraceDlq;
use crate::storage::{DbPool, build_sqlx};

// -- Repository trait --

#[async_trait]
pub trait TraceDlqRepository: Send + Sync {
    /// Enqueue a failed trace for later retry.
    async fn enqueue(
        &self,
        trace_id: &str,
        channel: &str,
        payload_json: &str,
        metadata_json: &str,
        error_message: &str,
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
                    .value(
                        TraceDlq::ClaimedUntil,
                        super::helpers::optional_string_value(None),
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
                    .value(
                        TraceDlq::ClaimedUntil,
                        super::helpers::optional_string_value(None),
                    )
                    .and_where(Expr::col(TraceDlq::Id).eq(id)),
            );

            self.pool.execute_query(&sql, values).await?;
            Ok(())
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

    async fn enqueue_due(repo: &SqlTraceDlqRepository, trace_id: &str) -> String {
        let entry = repo
            .enqueue(trace_id, "orders", "{}", "{}", "boom", 5)
            .await
            .expect("enqueue");
        // Entries become due 1s after enqueue — backdate to now-2s.
        let DbPool::Sqlite(p) = &repo.pool else {
            unreachable!("sqlite expected");
        };
        sqlx::query(
            "UPDATE trace_dlq SET next_retry_at = datetime('now', '-2 seconds') WHERE id = ?",
        )
        .bind(&entry.id)
        .execute(p)
        .await
        .expect("backdate");
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
