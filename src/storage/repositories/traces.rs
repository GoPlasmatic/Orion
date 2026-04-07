use async_trait::async_trait;
use sea_query::{Asterisk, Condition, Expr, Func, Order, Query};
use sea_query_binder::SqlxBinder;
use serde::Deserialize;
use crate::storage::DbPool;

use crate::errors::OrionError;
use crate::storage::models::{self, Trace};
use crate::storage::repositories::workflows::PaginatedResult;
use crate::storage::{query_builder, schema::Traces};

#[derive(Debug, Default, Deserialize)]
pub struct TraceFilter {
    pub status: Option<String>,
    pub channel: Option<String>,
    pub mode: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// Column to sort by: created_at (default), updated_at, status, channel, mode.
    pub sort_by: Option<String>,
    /// Sort direction: asc or desc (default).
    pub sort_order: Option<String>,
}

// -- Repository trait --

#[async_trait]
pub trait TraceRepository: Send + Sync {
    async fn create_pending(
        &self,
        channel: &str,
        mode: &str,
        input_json: Option<&str>,
    ) -> Result<Trace, OrionError>;
    async fn get_by_id(&self, id: &str) -> Result<Trace, OrionError>;
    async fn update_status(
        &self,
        id: &str,
        status: &str,
        error_message: Option<&str>,
    ) -> Result<Trace, OrionError>;
    async fn set_result(
        &self,
        id: &str,
        result_json: &str,
        duration_ms: f64,
    ) -> Result<(), OrionError>;
    async fn store_completed(
        &self,
        channel: &str,
        mode: &str,
        input_json: Option<&str>,
        result_json: &str,
        duration_ms: f64,
    ) -> Result<String, OrionError>;
    async fn list_paginated(
        &self,
        filter: &TraceFilter,
    ) -> Result<PaginatedResult<Trace>, OrionError>;
    /// Delete traces older than the given number of hours. Returns the count deleted.
    async fn delete_older_than(&self, hours: u64) -> Result<u64, OrionError>;
}

// -- SQL implementation --

pub struct SqlTraceRepository {
    pool: DbPool,
}

impl SqlTraceRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TraceRepository for SqlTraceRepository {
    async fn create_pending(
        &self,
        channel: &str,
        mode: &str,
        input_json: Option<&str>,
    ) -> Result<Trace, OrionError> {
        crate::metrics::timed_db_op("traces.create_pending", async {
            let id = uuid::Uuid::new_v4().to_string();

            let input_val: sea_query::Value = input_json
                .map(|s| s.to_string().into())
                .unwrap_or(sea_query::Value::String(None));

            let (sql, values) = Query::insert()
                .into_table(Traces::Table)
                .columns([
                    Traces::Id,
                    Traces::Status,
                    Traces::Channel,
                    Traces::Mode,
                    Traces::InputJson,
                ])
                .values_panic([
                    Expr::val(id.as_str()).into(),
                    Expr::val("pending").into(),
                    Expr::val(channel).into(),
                    Expr::val(mode).into(),
                    Expr::val(input_val).into(),
                ])
                .build_sqlx(query_builder());

            sqlx::query_with(&sql, values)
                .execute(&self.pool)
                .await?;

            self.get_by_id(&id).await
        })
        .await
    }

    async fn get_by_id(&self, id: &str) -> Result<Trace, OrionError> {
        crate::metrics::timed_db_op("traces.get_by_id", async {
            let (sql, values) = Query::select()
                .column(Asterisk)
                .from(Traces::Table)
                .and_where(Expr::col(Traces::Id).eq(id))
                .build_sqlx(query_builder());

            sqlx::query_as_with::<_, Trace, _>(&sql, values)
                .fetch_optional(&self.pool)
                .await?
                .ok_or_else(|| OrionError::NotFound(format!("Trace '{}' not found", id)))
        })
        .await
    }

    async fn update_status(
        &self,
        id: &str,
        status: &str,
        error_message: Option<&str>,
    ) -> Result<Trace, OrionError> {
        crate::metrics::timed_db_op("traces.update_status", async {
            let now = chrono::Utc::now().naive_utc().to_string();

            let (started_at, completed_at) = if status == models::TRACE_STATUS_RUNNING {
                (Some(now), None)
            } else if status == models::TRACE_STATUS_COMPLETED
                || status == models::TRACE_STATUS_FAILED
            {
                (None, Some(now))
            } else {
                (None, None)
            };

            let mut update = Query::update();
            update
                .table(Traces::Table)
                .value(Traces::Status, status);

            if let Some(err) = error_message {
                update.value(Traces::ErrorMessage, err);
            }
            if let Some(ref sa) = started_at {
                update.value(Traces::StartedAt, sa.as_str());
            }
            if let Some(ref ca) = completed_at {
                update.value(Traces::CompletedAt, ca.as_str());
            }

            update.and_where(Expr::col(Traces::Id).eq(id));

            let (sql, values) = update.build_sqlx(query_builder());

            sqlx::query_with(&sql, values)
                .execute(&self.pool)
                .await?;

            self.get_by_id(id).await
        })
        .await
    }

    async fn set_result(
        &self,
        id: &str,
        result_json: &str,
        duration_ms: f64,
    ) -> Result<(), OrionError> {
        crate::metrics::timed_db_op("traces.set_result", async {
            let (sql, values) = Query::update()
                .table(Traces::Table)
                .value(Traces::ResultJson, result_json)
                .value(Traces::DurationMs, duration_ms)
                .and_where(Expr::col(Traces::Id).eq(id))
                .build_sqlx(query_builder());

            sqlx::query_with(&sql, values)
                .execute(&self.pool)
                .await?;
            Ok(())
        })
        .await
    }

    async fn store_completed(
        &self,
        channel: &str,
        mode: &str,
        input_json: Option<&str>,
        result_json: &str,
        duration_ms: f64,
    ) -> Result<String, OrionError> {
        crate::metrics::timed_db_op("traces.store_completed", async {
            let id = uuid::Uuid::new_v4().to_string();
            let now = chrono::Utc::now().naive_utc().to_string();

            let input_val: sea_query::Value = input_json
                .map(|s| s.to_string().into())
                .unwrap_or(sea_query::Value::String(None));

            let (sql, values) = Query::insert()
                .into_table(Traces::Table)
                .columns([
                    Traces::Id,
                    Traces::Status,
                    Traces::Channel,
                    Traces::Mode,
                    Traces::InputJson,
                    Traces::ResultJson,
                    Traces::DurationMs,
                    Traces::StartedAt,
                    Traces::CompletedAt,
                ])
                .values_panic([
                    Expr::val(id.as_str()).into(),
                    Expr::val("completed").into(),
                    Expr::val(channel).into(),
                    Expr::val(mode).into(),
                    Expr::val(input_val).into(),
                    Expr::val(result_json).into(),
                    Expr::val(duration_ms).into(),
                    Expr::val(now.as_str()).into(),
                    Expr::val(now.as_str()).into(),
                ])
                .build_sqlx(query_builder());

            sqlx::query_with(&sql, values)
                .execute(&self.pool)
                .await?;

            Ok(id)
        })
        .await
    }

    async fn list_paginated(
        &self,
        filter: &TraceFilter,
    ) -> Result<PaginatedResult<Trace>, OrionError> {
        crate::metrics::timed_db_op("traces.list_paginated", async {
            let limit = filter.limit.unwrap_or(50).clamp(1, 1000);
            let offset = filter.offset.unwrap_or(0).max(0);

            let mut cond = Condition::all();
            if let Some(ref status) = filter.status {
                cond = cond.add(Expr::col(Traces::Status).eq(status.as_str()));
            }
            if let Some(ref channel) = filter.channel {
                cond = cond.add(Expr::col(Traces::Channel).eq(channel.as_str()));
            }
            if let Some(ref mode) = filter.mode {
                cond = cond.add(Expr::col(Traces::Mode).eq(mode.as_str()));
            }

            // COUNT query
            let (sql, values) = Query::select()
                .expr(Func::count(Expr::col(Asterisk)))
                .from(Traces::Table)
                .cond_where(cond.clone())
                .build_sqlx(query_builder());
            let (total,): (i64,) = sqlx::query_as_with::<_, (i64,), _>(&sql, values)
                .fetch_one(&self.pool)
                .await?;

            // Sort column mapping
            let sort_iden = match filter.sort_by.as_deref() {
                Some("updated_at") => Traces::UpdatedAt,
                Some("status") => Traces::Status,
                Some("channel") => Traces::Channel,
                Some("mode") => Traces::Mode,
                _ => Traces::CreatedAt,
            };
            let order = match filter.sort_order.as_deref() {
                Some("asc") => Order::Asc,
                _ => Order::Desc,
            };

            // DATA query
            let (sql, values) = Query::select()
                .column(Asterisk)
                .from(Traces::Table)
                .cond_where(cond)
                .order_by(sort_iden, order)
                .limit(limit as u64)
                .offset(offset as u64)
                .build_sqlx(query_builder());
            let data = sqlx::query_as_with::<_, Trace, _>(&sql, values)
                .fetch_all(&self.pool)
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

    async fn delete_older_than(&self, hours: u64) -> Result<u64, OrionError> {
        crate::metrics::timed_db_op("traces.delete_older_than", async {
            let cutoff = chrono::Utc::now()
                .naive_utc()
                .checked_sub_signed(chrono::Duration::hours(hours as i64))
                .unwrap_or(chrono::NaiveDateTime::MIN)
                .to_string();

            let (sql, values) = Query::delete()
                .from_table(Traces::Table)
                .and_where(Expr::col(Traces::CreatedAt).lt(&cutoff))
                .and_where(Expr::col(Traces::Status).is_in(["completed", "failed"]))
                .build_sqlx(query_builder());

            let result = sqlx::query_with(&sql, values)
                .execute(&self.pool)
                .await?;

            Ok(result.rows_affected())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> crate::storage::DbPool {
        crate::storage::init_pool(&crate::config::StorageConfig {
            url: "sqlite::memory:".to_string(),
            max_connections: 1,
            ..Default::default()
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn test_delete_older_than_removes_old_completed_traces() {
        let pool = test_pool().await;
        let repo = SqlTraceRepository::new(pool.clone());

        // Create a completed trace
        let id = repo
            .store_completed("orders", "sync", None, r#"{"ok":true}"#, 10.0)
            .await
            .unwrap();

        // Backdate it to 100 hours ago
        let old_time = chrono::Utc::now()
            .naive_utc()
            .checked_sub_signed(chrono::Duration::hours(100))
            .unwrap()
            .to_string();
        sqlx::query("UPDATE traces SET created_at = ? WHERE id = ?")
            .bind(&old_time)
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();

        // Create a recent trace that should NOT be deleted
        let _recent_id = repo
            .store_completed("orders", "sync", None, r#"{"ok":true}"#, 5.0)
            .await
            .unwrap();

        // Delete traces older than 72 hours
        let deleted = repo.delete_older_than(72).await.unwrap();
        assert_eq!(deleted, 1);

        // Verify the recent trace still exists
        let remaining = repo.list_paginated(&TraceFilter::default()).await.unwrap();
        assert_eq!(remaining.total, 1);
    }

    #[tokio::test]
    async fn test_delete_older_than_preserves_pending_traces() {
        let pool = test_pool().await;
        let repo = SqlTraceRepository::new(pool.clone());

        // Create a pending trace
        let trace = repo.create_pending("orders", "async", None).await.unwrap();

        // Backdate it
        let old_time = chrono::Utc::now()
            .naive_utc()
            .checked_sub_signed(chrono::Duration::hours(200))
            .unwrap()
            .to_string();
        sqlx::query("UPDATE traces SET created_at = ? WHERE id = ?")
            .bind(&old_time)
            .bind(&trace.id)
            .execute(&pool)
            .await
            .unwrap();

        // Cleanup should NOT delete pending traces
        let deleted = repo.delete_older_than(72).await.unwrap();
        assert_eq!(deleted, 0);
    }
}
