use async_trait::async_trait;
use serde::Deserialize;
use sqlx::SqlitePool;

use crate::errors::OrionError;
use crate::storage::models::{self, Job};
use crate::storage::repositories::rules::PaginatedResult;

#[derive(Debug, Default, Deserialize)]
pub struct JobFilter {
    pub status: Option<String>,
    pub channel: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

// -- Repository trait --

#[async_trait]
pub trait JobRepository: Send + Sync {
    async fn create_data_job(&self, channel: &str) -> Result<Job, OrionError>;
    async fn get_by_id(&self, id: &str) -> Result<Job, OrionError>;
    async fn update_status(
        &self,
        id: &str,
        status: &str,
        error_message: Option<&str>,
        records_processed: Option<i64>,
    ) -> Result<Job, OrionError>;
    async fn set_result(&self, id: &str, result_json: &str) -> Result<(), OrionError>;
    async fn list_paginated(&self, filter: &JobFilter) -> Result<PaginatedResult<Job>, OrionError>;
}

// -- SQLite implementation --

pub struct SqliteJobRepository {
    pool: SqlitePool,
}

impl SqliteJobRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl JobRepository for SqliteJobRepository {
    async fn create_data_job(&self, channel: &str) -> Result<Job, OrionError> {
        crate::metrics::timed_db_op("jobs.create_data_job", async {
            let id = uuid::Uuid::new_v4().to_string();

            sqlx::query(
                "INSERT INTO jobs (id, connector_id, status, channel) VALUES (?, ?, 'pending', ?)",
            )
            .bind(&id)
            .bind(models::DATA_API_CONNECTOR)
            .bind(channel)
            .execute(&self.pool)
            .await?;

            self.get_by_id(&id).await
        })
        .await
    }

    async fn get_by_id(&self, id: &str) -> Result<Job, OrionError> {
        crate::metrics::timed_db_op("jobs.get_by_id", async {
            sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?
                .ok_or_else(|| OrionError::NotFound(format!("Job '{}' not found", id)))
        })
        .await
    }

    async fn update_status(
        &self,
        id: &str,
        status: &str,
        error_message: Option<&str>,
        records_processed: Option<i64>,
    ) -> Result<Job, OrionError> {
        crate::metrics::timed_db_op("jobs.update_status", async {
            let now = chrono::Utc::now().naive_utc().to_string();

            // started_at and completed_at are mutually exclusive — no need to clone
            let (started_at, completed_at) = if status == models::JOB_STATUS_RUNNING {
                (Some(now), None)
            } else if status == models::JOB_STATUS_COMPLETED || status == models::JOB_STATUS_FAILED
            {
                (None, Some(now))
            } else {
                (None, None)
            };

            sqlx::query(
                r#"UPDATE jobs
                   SET status = ?, error_message = COALESCE(?, error_message),
                       records_processed = COALESCE(?, records_processed),
                       started_at = COALESCE(?, started_at),
                       completed_at = COALESCE(?, completed_at),
                       updated_at = datetime('now')
                   WHERE id = ?"#,
            )
            .bind(status)
            .bind(error_message)
            .bind(records_processed)
            .bind(&started_at)
            .bind(&completed_at)
            .bind(id)
            .execute(&self.pool)
            .await?;

            self.get_by_id(id).await
        })
        .await
    }

    async fn set_result(&self, id: &str, result_json: &str) -> Result<(), OrionError> {
        crate::metrics::timed_db_op("jobs.set_result", async {
            sqlx::query(
                "UPDATE jobs SET result_json = ?, updated_at = datetime('now') WHERE id = ?",
            )
            .bind(result_json)
            .bind(id)
            .execute(&self.pool)
            .await?;
            Ok(())
        })
        .await
    }

    async fn list_paginated(&self, filter: &JobFilter) -> Result<PaginatedResult<Job>, OrionError> {
        crate::metrics::timed_db_op("jobs.list_paginated", async {
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

            let count_query = format!("SELECT COUNT(*) FROM jobs {where_clause}");
            let mut cq = sqlx::query_as::<_, (i64,)>(&count_query);
            for b in &binds {
                cq = cq.bind(b);
            }
            let (total,) = cq.fetch_one(&self.pool).await?;

            let data_query = format!(
                "SELECT * FROM jobs {where_clause} ORDER BY created_at DESC LIMIT ? OFFSET ?"
            );
            let mut dq = sqlx::query_as::<_, Job>(&data_query);
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
}
