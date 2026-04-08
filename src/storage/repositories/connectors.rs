use crate::storage::DbPool;
use async_trait::async_trait;
use sea_query::{Asterisk, Expr, Func, Order, Query};
use serde::Deserialize;

use crate::errors::OrionError;
use crate::storage::models::Connector;
use crate::storage::repositories::workflows::PaginatedResult;
use crate::storage::{build_sqlx, schema::Connectors};

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

            let (sql, values) = build_sqlx(
                Query::insert()
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
                    ]),
            );

            self.pool
                .execute_query(&sql, values)
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
            let (sql, values) = build_sqlx(
                Query::select()
                    .column(Asterisk)
                    .from(Connectors::Table)
                    .and_where(Expr::col(Connectors::Id).eq(id)),
            );

            self.pool
                .fetch_optional_as::<Connector>(&sql, values)
                .await?
                .ok_or_else(|| OrionError::NotFound(format!("Connector '{}' not found", id)))
        })
        .await
    }

    async fn list(&self) -> Result<Vec<Connector>, OrionError> {
        let (sql, values) = build_sqlx(
            Query::select()
                .column(Asterisk)
                .from(Connectors::Table)
                .order_by(Connectors::Name, Order::Asc),
        );

        Ok(self.pool.fetch_all_as::<Connector>(&sql, values).await?)
    }

    async fn list_paginated(
        &self,
        filter: &ConnectorFilter,
    ) -> Result<PaginatedResult<Connector>, OrionError> {
        crate::metrics::timed_db_op("connectors.list_paginated", async {
            let (limit, offset) = super::helpers::clamp_pagination(filter.limit, filter.offset);

            let (count_sql, count_values) = build_sqlx(
                Query::select()
                    .expr(Func::count(Expr::col(Asterisk)))
                    .from(Connectors::Table),
            );

            let (total,): (i64,) = self
                .pool
                .fetch_one_as::<(i64,)>(&count_sql, count_values)
                .await?;

            let (sql, values) = build_sqlx(
                Query::select()
                    .column(Asterisk)
                    .from(Connectors::Table)
                    .order_by(Connectors::Name, Order::Asc)
                    .limit(limit as u64)
                    .offset(offset as u64),
            );

            let data = self.pool.fetch_all_as::<Connector>(&sql, values).await?;

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

            let (sql, values) = build_sqlx(
                Query::update()
                    .table(Connectors::Table)
                    .value(Connectors::Name, name)
                    .value(Connectors::ConnectorType, connector_type)
                    .value(Connectors::ConfigJson, &config_json)
                    .value(Connectors::Enabled, enabled)
                    .and_where(Expr::col(Connectors::Id).eq(id)),
            );

            self.pool.execute_query(&sql, values).await?;

            self.get_by_id(id).await
        })
        .await
    }

    async fn delete(&self, id: &str) -> Result<(), OrionError> {
        crate::metrics::timed_db_op("connectors.delete", async {
            let (sql, values) = build_sqlx(
                Query::delete()
                    .from_table(Connectors::Table)
                    .and_where(Expr::col(Connectors::Id).eq(id)),
            );

            let rows_affected = self.pool.execute_query(&sql, values).await?;

            if rows_affected == 0 {
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
            let (sql, values) = build_sqlx(
                Query::select()
                    .column(Asterisk)
                    .from(Connectors::Table)
                    .and_where(Expr::col(Connectors::Enabled).eq(true))
                    .order_by(Connectors::Name, Order::Asc),
            );

            Ok(self.pool.fetch_all_as::<Connector>(&sql, values).await?)
        })
        .await
    }
}
