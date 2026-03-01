use async_trait::async_trait;
use serde::Deserialize;
use sqlx::SqlitePool;

use crate::errors::OrionError;
use crate::storage::models::{self, Trace};
use crate::storage::repositories::rules::PaginatedResult;

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

/// Allowed columns for the `sort_by` query parameter.
const ALLOWED_SORT_COLUMNS: &[&str] = &["created_at", "updated_at", "status", "channel", "mode"];

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

// -- SQLite implementation --

pub struct SqliteTraceRepository {
    pool: SqlitePool,
}

impl SqliteTraceRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TraceRepository for SqliteTraceRepository {
    async fn create_pending(
        &self,
        channel: &str,
        mode: &str,
        input_json: Option<&str>,
    ) -> Result<Trace, OrionError> {
        crate::metrics::timed_db_op("traces.create_pending", async {
            let id = uuid::Uuid::new_v4().to_string();

            sqlx::query(
                "INSERT INTO traces (id, status, channel, mode, input_json) VALUES (?, 'pending', ?, ?, ?)",
            )
            .bind(&id)
            .bind(channel)
            .bind(mode)
            .bind(input_json)
            .execute(&self.pool)
            .await?;

            self.get_by_id(&id).await
        })
        .await
    }

    async fn get_by_id(&self, id: &str) -> Result<Trace, OrionError> {
        crate::metrics::timed_db_op("traces.get_by_id", async {
            sqlx::query_as::<_, Trace>("SELECT * FROM traces WHERE id = ?")
                .bind(id)
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

            sqlx::query(
                r#"UPDATE traces
                   SET status = ?, error_message = COALESCE(?, error_message),
                       started_at = COALESCE(?, started_at),
                       completed_at = COALESCE(?, completed_at)
                   WHERE id = ?"#,
            )
            .bind(status)
            .bind(error_message)
            .bind(&started_at)
            .bind(&completed_at)
            .bind(id)
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
            sqlx::query("UPDATE traces SET result_json = ?, duration_ms = ? WHERE id = ?")
                .bind(result_json)
                .bind(duration_ms)
                .bind(id)
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

            sqlx::query(
                r#"INSERT INTO traces (id, status, channel, mode, input_json, result_json, duration_ms, started_at, completed_at)
                   VALUES (?, 'completed', ?, ?, ?, ?, ?, ?, ?)"#,
            )
            .bind(&id)
            .bind(channel)
            .bind(mode)
            .bind(input_json)
            .bind(result_json)
            .bind(duration_ms)
            .bind(&now)
            .bind(&now)
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

            let mut where_clause = String::from("WHERE 1=1");
            let mut binds: Vec<String> = Vec::new();

            if let Some(ref status) = filter.status {
                where_clause.push_str(" AND status = ?");
                binds.push(status.clone());
            }
            if let Some(ref channel) = filter.channel {
                where_clause.push_str(" AND channel = ?");
                binds.push(channel.clone());
            }
            if let Some(ref mode) = filter.mode {
                where_clause.push_str(" AND mode = ?");
                binds.push(mode.clone());
            }

            let count_query = format!("SELECT COUNT(*) FROM traces {where_clause}");
            let mut cq = sqlx::query_as::<_, (i64,)>(&count_query);
            for b in &binds {
                cq = cq.bind(b);
            }
            let (total,) = cq.fetch_one(&self.pool).await?;

            let sort_col = filter
                .sort_by
                .as_deref()
                .filter(|c| ALLOWED_SORT_COLUMNS.contains(c))
                .unwrap_or("created_at");
            let sort_dir = match filter.sort_order.as_deref() {
                Some("asc") => "ASC",
                _ => "DESC",
            };

            let data_query = format!(
                "SELECT * FROM traces {where_clause} ORDER BY {sort_col} {sort_dir} LIMIT ? OFFSET ?"
            );
            let mut dq = sqlx::query_as::<_, Trace>(&data_query);
            for b in &binds {
                dq = dq.bind(b);
            }
            dq = dq.bind(limit).bind(offset);
            let data = dq.fetch_all(&self.pool).await?;

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

            let result = sqlx::query(
                "DELETE FROM traces WHERE created_at < ? AND status IN ('completed', 'failed')",
            )
            .bind(&cutoff)
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

    async fn test_pool() -> sqlx::SqlitePool {
        let pool = crate::storage::init_pool(&crate::config::StorageConfig {
            path: ":memory:".to_string(),
            max_connections: 1,
            ..Default::default()
        })
        .await
        .unwrap();
        pool
    }

    #[tokio::test]
    async fn test_delete_older_than_removes_old_completed_traces() {
        let pool = test_pool().await;
        let repo = SqliteTraceRepository::new(pool.clone());

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
        let repo = SqliteTraceRepository::new(pool.clone());

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
