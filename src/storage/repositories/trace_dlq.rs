use async_trait::async_trait;
use sea_query::{Asterisk, Condition, Expr, Order, Query};

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

    /// Fetch entries that are due for retry (next_retry_at <= now AND retry_count < max_retries).
    async fn list_pending(&self, limit: i64) -> Result<Vec<TraceDlqEntry>, OrionError>;

    /// Increment retry count and set next retry time for a DLQ entry.
    async fn record_retry(&self, id: &str, next_retry_at: &str) -> Result<(), OrionError>;

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

            // First retry after 1 second
            let next_retry = chrono::Utc::now()
                .naive_utc()
                .checked_add_signed(chrono::Duration::seconds(1))
                .unwrap_or(chrono::Utc::now().naive_utc())
                .to_string();

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
                        Expr::val(next_retry.as_str()).into(),
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

    async fn list_pending(&self, limit: i64) -> Result<Vec<TraceDlqEntry>, OrionError> {
        crate::metrics::timed_db_op("trace_dlq.list_pending", async {
            let now = chrono::Utc::now().naive_utc().to_string();

            let cond = Condition::all()
                .add(Expr::col(TraceDlq::NextRetryAt).lte(now.as_str()))
                .add(Expr::col(TraceDlq::RetryCount).lt(Expr::col(TraceDlq::MaxRetries)));

            let (sql, values) = build_sqlx(
                Query::select()
                    .column(Asterisk)
                    .from(TraceDlq::Table)
                    .cond_where(cond)
                    .order_by(TraceDlq::NextRetryAt, Order::Asc)
                    .limit(limit as u64),
            );

            Ok(self
                .pool
                .fetch_all_as::<TraceDlqEntry>(&sql, values)
                .await?)
        })
        .await
    }

    async fn record_retry(&self, id: &str, next_retry_at: &str) -> Result<(), OrionError> {
        crate::metrics::timed_db_op("trace_dlq.record_retry", async {
            let (sql, values) = build_sqlx(
                Query::update()
                    .table(TraceDlq::Table)
                    .value(TraceDlq::RetryCount, Expr::col(TraceDlq::RetryCount).add(1))
                    .value(TraceDlq::NextRetryAt, next_retry_at)
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
                    .and_where(Expr::col(TraceDlq::Id).eq(id)),
            );

            self.pool.execute_query(&sql, values).await?;
            Ok(())
        })
        .await
    }
}
