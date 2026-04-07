use crate::storage::DbPool;
use async_trait::async_trait;
use sea_query::{Asterisk, Expr, Func, Order, Query};
use sea_query_binder::SqlxBinder;
use serde::Deserialize;

use crate::errors::OrionError;
use crate::storage::models::Connector;
use crate::storage::repositories::workflows::PaginatedResult;
use crate::storage::{query_builder, schema::Connectors};

// -- DTOs --

#[derive(Debug, Deserialize, utoipa::ToSchema)]
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

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateConnectorRequest {
    pub name: Option<String>,
    pub connector_type: Option<String>,
    pub config: Option<serde_json::Value>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
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

// -- SQL implementation --

pub struct SqlConnectorRepository {
    pool: DbPool,
}

impl SqlConnectorRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ConnectorRepository for SqlConnectorRepository {
    async fn create(&self, req: &CreateConnectorRequest) -> Result<Connector, OrionError> {
        crate::metrics::timed_db_op("connectors.create", async {
            let id = req
                .id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let config_json = serde_json::to_string(&req.config)?;

            let (sql, values) = Query::insert()
                .into_table(Connectors::Table)
                .columns([
                    Connectors::Id,
                    Connectors::Name,
                    Connectors::ConnectorType,
                    Connectors::ConfigJson,
                ])
                .values_panic([
                    id.as_str().into(),
                    req.name.as_str().into(),
                    req.connector_type.as_str().into(),
                    config_json.as_str().into(),
                ])
                .build_sqlx(query_builder());

            sqlx::query_with(&sql, values)
                .execute(&self.pool)
                .await
                .map_err(|e| match e {
                    sqlx::Error::Database(ref db_err)
                        if db_err.kind() == sqlx::error::ErrorKind::UniqueViolation =>
                    {
                        OrionError::Conflict(format!(
                            "Connector with name '{}' already exists",
                            req.name
                        ))
                    }
                    _ => OrionError::Storage(e),
                })?;

            self.get_by_id(&id).await
        })
        .await
    }

    async fn get_by_id(&self, id: &str) -> Result<Connector, OrionError> {
        crate::metrics::timed_db_op("connectors.get_by_id", async {
            let (sql, values) = Query::select()
                .column(Asterisk)
                .from(Connectors::Table)
                .and_where(Expr::col(Connectors::Id).eq(id))
                .build_sqlx(query_builder());

            sqlx::query_as_with::<_, Connector, _>(&sql, values)
                .fetch_optional(&self.pool)
                .await?
                .ok_or_else(|| OrionError::NotFound(format!("Connector '{}' not found", id)))
        })
        .await
    }

    async fn list(&self) -> Result<Vec<Connector>, OrionError> {
        let (sql, values) = Query::select()
            .column(Asterisk)
            .from(Connectors::Table)
            .order_by(Connectors::Name, Order::Asc)
            .build_sqlx(query_builder());

        Ok(sqlx::query_as_with::<_, Connector, _>(&sql, values)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn list_paginated(
        &self,
        filter: &ConnectorFilter,
    ) -> Result<PaginatedResult<Connector>, OrionError> {
        crate::metrics::timed_db_op("connectors.list_paginated", async {
            let limit = filter.limit.unwrap_or(50).clamp(1, 1000);
            let offset = filter.offset.unwrap_or(0).max(0);

            let (count_sql, count_values) = Query::select()
                .expr(Func::count(Expr::col(Asterisk)))
                .from(Connectors::Table)
                .build_sqlx(query_builder());

            let (total,): (i64,) = sqlx::query_as_with::<_, (i64,), _>(&count_sql, count_values)
                .fetch_one(&self.pool)
                .await?;

            let (sql, values) = Query::select()
                .column(Asterisk)
                .from(Connectors::Table)
                .order_by(Connectors::Name, Order::Asc)
                .limit(limit as u64)
                .offset(offset as u64)
                .build_sqlx(query_builder());

            let data = sqlx::query_as_with::<_, Connector, _>(&sql, values)
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

    async fn update(
        &self,
        id: &str,
        req: &UpdateConnectorRequest,
    ) -> Result<Connector, OrionError> {
        crate::metrics::timed_db_op("connectors.update", async {
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

            let (sql, values) = Query::update()
                .table(Connectors::Table)
                .value(Connectors::Name, name)
                .value(Connectors::ConnectorType, connector_type)
                .value(Connectors::ConfigJson, &config_json)
                .value(Connectors::Enabled, enabled)
                .and_where(Expr::col(Connectors::Id).eq(id))
                .build_sqlx(query_builder());

            sqlx::query_with(&sql, values).execute(&self.pool).await?;

            self.get_by_id(id).await
        })
        .await
    }

    async fn delete(&self, id: &str) -> Result<(), OrionError> {
        crate::metrics::timed_db_op("connectors.delete", async {
            let (sql, values) = Query::delete()
                .from_table(Connectors::Table)
                .and_where(Expr::col(Connectors::Id).eq(id))
                .build_sqlx(query_builder());

            let result = sqlx::query_with(&sql, values).execute(&self.pool).await?;

            if result.rows_affected() == 0 {
                return Err(OrionError::NotFound(format!(
                    "Connector '{}' not found",
                    id
                )));
            }

            Ok(())
        })
        .await
    }

    async fn list_enabled(&self) -> Result<Vec<Connector>, OrionError> {
        crate::metrics::timed_db_op("connectors.list_enabled", async {
            let (sql, values) = Query::select()
                .column(Asterisk)
                .from(Connectors::Table)
                .and_where(Expr::col(Connectors::Enabled).eq(true))
                .order_by(Connectors::Name, Order::Asc)
                .build_sqlx(query_builder());

            Ok(sqlx::query_as_with::<_, Connector, _>(&sql, values)
                .fetch_all(&self.pool)
                .await?)
        })
        .await
    }
}
