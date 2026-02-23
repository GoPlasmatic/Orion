use async_trait::async_trait;
use sqlx::SqlitePool;

use crate::errors::OrionError;
use crate::storage::models::Job;

// -- Repository trait --

#[async_trait]
pub trait JobRepository: Send + Sync {
    async fn create(&self, connector_id: &str) -> Result<Job, OrionError>;
    async fn create_data_job(&self, channel: &str) -> Result<Job, OrionError>;
    async fn get_by_id(&self, id: &str) -> Result<Job, OrionError>;
    async fn list_by_connector(&self, connector_id: &str) -> Result<Vec<Job>, OrionError>;
    async fn update_status(
        &self,
        id: &str,
        status: &str,
        error_message: Option<&str>,
        records_processed: Option<i64>,
    ) -> Result<Job, OrionError>;
    async fn set_result(&self, id: &str, result_json: &str) -> Result<(), OrionError>;
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
    async fn create(&self, connector_id: &str) -> Result<Job, OrionError> {
        let id = uuid::Uuid::new_v4().to_string();

        sqlx::query(r#"INSERT INTO jobs (id, connector_id, status) VALUES (?, ?, 'pending')"#)
            .bind(&id)
            .bind(connector_id)
            .execute(&self.pool)
            .await?;

        self.get_by_id(&id).await
    }

    async fn create_data_job(&self, channel: &str) -> Result<Job, OrionError> {
        let id = uuid::Uuid::new_v4().to_string();

        sqlx::query(
            r#"INSERT INTO jobs (id, connector_id, status, channel) VALUES (?, '__data_api__', 'pending', ?)"#,
        )
        .bind(&id)
        .bind(channel)
        .execute(&self.pool)
        .await?;

        self.get_by_id(&id).await
    }

    async fn get_by_id(&self, id: &str) -> Result<Job, OrionError> {
        sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| OrionError::NotFound(format!("Job '{}' not found", id)))
    }

    async fn list_by_connector(&self, connector_id: &str) -> Result<Vec<Job>, OrionError> {
        Ok(sqlx::query_as::<_, Job>(
            "SELECT * FROM jobs WHERE connector_id = ? ORDER BY created_at DESC",
        )
        .bind(connector_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn update_status(
        &self,
        id: &str,
        status: &str,
        error_message: Option<&str>,
        records_processed: Option<i64>,
    ) -> Result<Job, OrionError> {
        let now = chrono::Utc::now().naive_utc().to_string();

        let started_at = if status == "running" {
            Some(now.clone())
        } else {
            None
        };
        let completed_at = if status == "completed" || status == "failed" {
            Some(now)
        } else {
            None
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
    }

    async fn set_result(&self, id: &str, result_json: &str) -> Result<(), OrionError> {
        sqlx::query("UPDATE jobs SET result_json = ?, updated_at = datetime('now') WHERE id = ?")
            .bind(result_json)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
