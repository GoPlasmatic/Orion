use async_trait::async_trait;
use serde::Deserialize;
use sqlx::SqlitePool;

use crate::errors::OrionError;
use crate::storage::models::Connector;
use crate::storage::repositories::rules::PaginatedResult;

// -- DTOs --

#[derive(Debug, Deserialize)]
pub struct CreateConnectorRequest {
    pub id: Option<String>,
    pub name: String,
    pub connector_type: String,
    #[serde(default = "default_config")]
    pub config: serde_json::Value,
}

fn default_config() -> serde_json::Value {
    serde_json::json!({})
}

#[derive(Debug, Deserialize)]
pub struct UpdateConnectorRequest {
    pub name: Option<String>,
    pub connector_type: Option<String>,
    pub config: Option<serde_json::Value>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ConnectorFilter {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

// -- Repository trait --

#[async_trait]
pub trait ConnectorRepository: Send + Sync {
    async fn create(&self, req: &CreateConnectorRequest) -> Result<Connector, OrionError>;
    async fn get_by_id(&self, id: &str) -> Result<Connector, OrionError>;
    async fn list(&self) -> Result<Vec<Connector>, OrionError>;
    async fn list_paginated(
        &self,
        filter: &ConnectorFilter,
    ) -> Result<PaginatedResult<Connector>, OrionError>;
    async fn update(&self, id: &str, req: &UpdateConnectorRequest)
    -> Result<Connector, OrionError>;
    async fn delete(&self, id: &str) -> Result<(), OrionError>;
    async fn list_enabled(&self) -> Result<Vec<Connector>, OrionError>;
}

// -- SQLite implementation --

pub struct SqliteConnectorRepository {
    pool: SqlitePool,
}

impl SqliteConnectorRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ConnectorRepository for SqliteConnectorRepository {
    async fn create(&self, req: &CreateConnectorRequest) -> Result<Connector, OrionError> {
        let id = req
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let config_json = serde_json::to_string(&req.config)?;

        sqlx::query(
            r#"INSERT INTO connectors (id, name, connector_type, config_json)
               VALUES (?, ?, ?, ?)"#,
        )
        .bind(&id)
        .bind(&req.name)
        .bind(&req.connector_type)
        .bind(&config_json)
        .execute(&self.pool)
        .await
        .map_err(|e| match e {
            sqlx::Error::Database(ref db_err) if db_err.message().contains("UNIQUE") => {
                OrionError::Conflict(format!("Connector with name '{}' already exists", req.name))
            }
            _ => OrionError::Storage(e),
        })?;

        self.get_by_id(&id).await
    }

    async fn get_by_id(&self, id: &str) -> Result<Connector, OrionError> {
        sqlx::query_as::<_, Connector>("SELECT * FROM connectors WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| OrionError::NotFound(format!("Connector '{}' not found", id)))
    }

    async fn list(&self) -> Result<Vec<Connector>, OrionError> {
        Ok(
            sqlx::query_as::<_, Connector>("SELECT * FROM connectors ORDER BY name ASC")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    async fn list_paginated(
        &self,
        filter: &ConnectorFilter,
    ) -> Result<PaginatedResult<Connector>, OrionError> {
        let limit = filter.limit.unwrap_or(50).clamp(1, 1000);
        let offset = filter.offset.unwrap_or(0).max(0);

        let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM connectors")
            .fetch_one(&self.pool)
            .await?;

        let data = sqlx::query_as::<_, Connector>(
            "SELECT * FROM connectors ORDER BY name ASC LIMIT ? OFFSET ?",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        Ok(PaginatedResult {
            data,
            total,
            limit,
            offset,
        })
    }

    async fn update(
        &self,
        id: &str,
        req: &UpdateConnectorRequest,
    ) -> Result<Connector, OrionError> {
        let existing = self.get_by_id(id).await?;

        let name = req.name.as_deref().unwrap_or(&existing.name);
        let connector_type = req
            .connector_type
            .as_deref()
            .unwrap_or(&existing.connector_type);
        let config_json = match &req.config {
            Some(c) => serde_json::to_string(c)?,
            None => existing.config_json.clone(),
        };
        let enabled = req.enabled.unwrap_or(existing.enabled);

        sqlx::query(
            r#"UPDATE connectors
               SET name = ?, connector_type = ?, config_json = ?, enabled = ?, updated_at = datetime('now')
               WHERE id = ?"#,
        )
        .bind(name)
        .bind(connector_type)
        .bind(&config_json)
        .bind(enabled)
        .bind(id)
        .execute(&self.pool)
        .await?;

        self.get_by_id(id).await
    }

    async fn delete(&self, id: &str) -> Result<(), OrionError> {
        let result = sqlx::query("DELETE FROM connectors WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(OrionError::NotFound(format!(
                "Connector '{}' not found",
                id
            )));
        }

        Ok(())
    }

    async fn list_enabled(&self) -> Result<Vec<Connector>, OrionError> {
        Ok(sqlx::query_as::<_, Connector>(
            "SELECT * FROM connectors WHERE enabled = 1 ORDER BY name ASC",
        )
        .fetch_all(&self.pool)
        .await?)
    }
}
