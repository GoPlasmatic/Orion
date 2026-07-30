use crate::storage::DbPool;
use async_trait::async_trait;
use sea_query::{Asterisk, Expr, IntoIden, Order, Query};
use serde::Deserialize;

use super::helpers::{Page, PaginatedResult, Projection};
use crate::errors::OrionError;
use crate::storage::models::Connector;
use crate::storage::{build_sqlx, schema::Connectors};

// -- DTOs --

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateConnectorRequest {
    pub id: Option<String>,
    pub name: String,
    pub connector_type: crate::connector::ConnectorType,
    #[serde(default = "default_config")]
    pub config: serde_json::Value,
}

fn default_config() -> serde_json::Value {
    serde_json::json!({})
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateConnectorRequest {
    pub name: Option<String>,
    pub connector_type: Option<crate::connector::ConnectorType>,
    pub config: Option<serde_json::Value>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ConnectorFilter {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

// -- Repository trait --

#[async_trait]
pub trait ConnectorRepository: Send + Sync {
    async fn create(&self, req: &CreateConnectorRequest) -> Result<Connector, OrionError>;
    async fn get_by_id(&self, id: &str) -> Result<Connector, OrionError>;
    async fn list_paginated(
        &self,
        filter: &ConnectorFilter,
    ) -> Result<PaginatedResult<Connector>, OrionError>;
    async fn update(&self, id: &str, req: &UpdateConnectorRequest)
    -> Result<Connector, OrionError>;
    async fn delete(&self, id: &str) -> Result<(), OrionError>;
    async fn list_enabled(&self) -> Result<Vec<Connector>, OrionError>;
    /// Whether a connector with this name is already stored.
    ///
    /// R15: `name` carries the unique constraint, so it is what a bulk import
    /// collides on. Dry-run used to skip the database entirely and therefore
    /// could not see the single most common real failure.
    async fn exists_by_name(&self, name: &str) -> Result<bool, OrionError>;
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

            self.pool.execute_query(&sql, values).await.map_err(|e| {
                super::helpers::map_duplicate(e, || {
                    format!("Connector with name '{}' already exists", req.name)
                })
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
                .ok_or_else(|| OrionError::NotFound(format!("Connector '{id}' not found")))
        })
        .await
    }

    async fn list_paginated(
        &self,
        filter: &ConnectorFilter,
    ) -> Result<PaginatedResult<Connector>, OrionError> {
        crate::metrics::timed_db_op("connectors.list_paginated", async {
            let (limit, offset) = super::helpers::clamp_pagination(filter.limit, filter.offset);

            super::helpers::paginate(
                &self.pool,
                Page {
                    from: Connectors::Table.into_iden(),
                    projection: Projection::All,
                    // Connectors are unversioned and unfiltered — the filter
                    // DTO carries page bounds only. An empty `Condition::all()`
                    // renders no `WHERE` clause.
                    cond: sea_query::Condition::all(),
                    sort: Connectors::Name.into_iden(),
                    order: Order::Asc,
                    limit,
                    offset,
                },
            )
            .await
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
            let connector_type: &str = req
                .connector_type
                .as_ref()
                .map(|c| c.as_str())
                .unwrap_or(existing.connector_type.as_str());
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
                return Err(OrionError::NotFound(format!("Connector '{id}' not found")));
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

    async fn exists_by_name(&self, name: &str) -> Result<bool, OrionError> {
        crate::metrics::timed_db_op("connectors.exists_by_name", async {
            // A COUNT, not a `SELECT *`: an existence check has no business
            // materialising the row — least of all `config_json`, which carries
            // the connector's unmasked secrets.
            Ok(super::helpers::count_where(
                &self.pool,
                Connectors::Table,
                sea_query::Condition::all().add(Expr::col(Connectors::Name).eq(name)),
            )
            .await?
                > 0)
        })
        .await
    }
}
