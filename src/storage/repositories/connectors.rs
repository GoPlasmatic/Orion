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

#[derive(Debug, Default, Deserialize, serde::Serialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ConnectorFilter {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// Column to sort by: name (default), connector_type, created_at, updated_at.
    pub sort_by: Option<String>,
    /// Sort direction: asc (default) or desc. The default differs from the
    /// versioned lists (which default desc on priority) because connectors
    /// have always listed alphabetically — D22 added the sort fields without
    /// moving the unsorted callers' rows.
    pub sort_order: Option<String>,
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
    /// H3: present when `storage.connector_encryption_key` is set. Writes
    /// encrypt `config_json`; reads decrypt (with plaintext pass-through for
    /// rows written before the key existed).
    cipher: Option<std::sync::Arc<crate::storage::config_encryption::ConfigCipher>>,
}

impl SqlConnectorRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool, cipher: None }
    }

    /// H3: construct with the optional at-rest cipher.
    pub fn with_cipher(
        pool: DbPool,
        cipher: Option<std::sync::Arc<crate::storage::config_encryption::ConfigCipher>>,
    ) -> Self {
        Self { pool, cipher }
    }

    /// The form `config_json` takes in the database.
    fn store_form(&self, config_json: &str) -> Result<String, OrionError> {
        match &self.cipher {
            Some(cipher) => cipher.encrypt(config_json),
            None => Ok(config_json.to_string()),
        }
    }

    /// Undo [`Self::store_form`] on a fetched row. An encrypted row with no
    /// key configured is a loud error — serving the literal `enc:v1:…` string
    /// as a config would fail everywhere downstream with worse messages.
    fn open_row(&self, mut row: Connector) -> Result<Connector, OrionError> {
        use crate::storage::config_encryption::ConfigCipher;
        row.config_json = match &self.cipher {
            Some(cipher) => cipher.decrypt(&row.config_json)?,
            None if ConfigCipher::is_encrypted(&row.config_json) => {
                return Err(OrionError::internal(format!(
                    "connector '{}' is encrypted at rest but \
                     storage.connector_encryption_key is not set",
                    row.id
                )));
            }
            None => row.config_json,
        };
        Ok(row)
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
            let config_json = self.store_form(&serde_json::to_string(&req.config)?)?;

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
                .and_then(|row| self.open_row(row))
        })
        .await
    }

    async fn list_paginated(
        &self,
        filter: &ConnectorFilter,
    ) -> Result<PaginatedResult<Connector>, OrionError> {
        crate::metrics::timed_db_op("connectors.list_paginated", async {
            let (limit, offset) = super::helpers::clamp_pagination(filter.limit, filter.offset);

            let sort_iden = match filter.sort_by.as_deref() {
                Some("connector_type") => Connectors::ConnectorType,
                Some("created_at") => Connectors::CreatedAt,
                Some("updated_at") => Connectors::UpdatedAt,
                _ => Connectors::Name,
            };
            // Asc unless "desc" is asked for: the historical (and only
            // pre-D22) ordering was name ASC, and an absent sort_order must
            // keep returning the rows it always did.
            let order = match filter.sort_order.as_deref() {
                Some("desc") => Order::Desc,
                _ => Order::Asc,
            };
            let page: PaginatedResult<Connector> = super::helpers::paginate(
                &self.pool,
                Page {
                    from: Connectors::Table.into_iden(),
                    projection: Projection::All,
                    // Connectors are unversioned and unfiltered — the filter
                    // DTO carries page bounds and sort only. An empty
                    // `Condition::all()` renders no `WHERE` clause.
                    cond: sea_query::Condition::all(),
                    sort: sort_iden.into_iden(),
                    order,
                    limit,
                    offset,
                },
            )
            .await?;
            Ok(PaginatedResult {
                data: page
                    .data
                    .into_iter()
                    .map(|row| self.open_row(row))
                    .collect::<Result<_, _>>()?,
                total: page.total,
                limit: page.limit,
                offset: page.offset,
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
            let connector_type: &str = req
                .connector_type
                .as_ref()
                .map(|c| c.as_str())
                .unwrap_or(existing.connector_type.as_str());
            let config_json = self.store_form(&match &req.config {
                Some(c) => serde_json::to_string(c)?,
                None => existing.config_json.clone(),
            })?;
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

            self.pool
                .fetch_all_as::<Connector>(&sql, values)
                .await?
                .into_iter()
                .map(|row| self.open_row(row))
                .collect()
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
