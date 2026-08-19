use crate::storage::DbPool;
use async_trait::async_trait;
use sea_query::{Asterisk, Expr, ExprTrait, IntoIden, Order, Query};
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
    /// Whether the connector loads into the registry. Defaults to true (K1).
    ///
    /// `/export` has always emitted this field; until K1 the create path
    /// silently dropped it, so a *disabled* connector promoted through
    /// export → import came back **enabled** in the target environment.
    pub enabled: Option<bool>,
    /// Selection labels (K6), same contract as workflow tags — filter with
    /// `?tag=` on list and export.
    #[serde(default)]
    pub tags: Vec<String>,
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
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize, serde::Serialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ConnectorFilter {
    /// Only connectors carrying this tag (K6).
    pub tag: Option<String>,
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

/// The one connector filter: `?tag=` (K6). Shared by `list_paginated` and
/// `snapshot`, the pattern the two versioned repositories already use.
fn build_condition(filter: &ConnectorFilter) -> sea_query::Condition {
    let mut cond = sea_query::Condition::all();
    if let Some(ref tag) = filter.tag {
        cond = cond.add(
            Expr::col(Connectors::TagsJson).like(super::helpers::tag_like_pattern(tag.as_str())),
        );
    }
    cond
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
    /// Fetch a connector by its unique `name` (K2).
    ///
    /// The upsert import matches on `name` — the column the unique constraint
    /// lives on — and needs the stored row to compare content and address the
    /// update, which takes the `id`.
    async fn get_by_name(&self, name: &str) -> Result<Connector, OrionError>;
    /// Every connector matching `filter` (`tag`; the filter's own page
    /// bounds are ignored), read as one consistent snapshot (K12) — the
    /// export contract.
    async fn snapshot(&self, filter: &ConnectorFilter) -> Result<Vec<Connector>, OrionError>;

    // -- Managed-OAuth2 runtime state (#268) --
    //
    // A separate table rather than a column on `connectors`: that table is
    // declarative, user-authored config (export/import, masking, content
    // hashing all treat it that way), and rotation state mutating it would
    // race its owner. `state_json` is encrypted at rest exactly like
    // `config_json` when `storage.connector_encryption_key` is set.

    /// The stored OAuth2 token state for `connector_name`, decrypted.
    /// `None` when the connector has never refreshed on this estate.
    async fn get_oauth_state(
        &self,
        connector_name: &str,
    ) -> Result<Option<crate::storage::models::ConnectorOauthStateRow>, OrionError>;

    /// Upsert the OAuth2 token state for `connector_name`.
    async fn put_oauth_state(
        &self,
        connector_name: &str,
        fingerprint: &str,
        state_json: &str,
    ) -> Result<(), OrionError>;

    /// Drop the OAuth2 token state for `connector_name` (no-op when absent).
    async fn delete_oauth_state(&self, connector_name: &str) -> Result<(), OrionError>;
}

// -- SQL implementation --

/// `SELECT * FROM connectors WHERE id = ?` — the single-row read shape,
/// shared by `get_by_id` and the read-back arm of the write-returning paths.
fn connector_select(id: &str) -> sea_query::SelectStatement {
    Query::select()
        .column(Asterisk)
        .from(Connectors::Table)
        .and_where(Expr::col(Connectors::Id).eq(id))
        .to_owned()
}

fn connector_not_found(id: &str) -> OrionError {
    OrionError::NotFound(format!("Connector '{id}' not found"))
}

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
            let tags_json = serde_json::to_string(&req.tags)?;

            let mut insert = Query::insert();
            insert
                .into_table(Connectors::Table)
                .columns([
                    Connectors::Id,
                    Connectors::Name,
                    Connectors::ConnectorType,
                    Connectors::ConfigJson,
                    Connectors::Enabled,
                    Connectors::TagsJson,
                ])
                .values_panic([
                    id.as_str().into(),
                    req.name.as_str().into(),
                    req.connector_type.as_str().into(),
                    config_json.as_str().into(),
                    req.enabled.unwrap_or(true).into(),
                    tags_json.as_str().into(),
                ]);

            // D23: the INSERT and the row it wrote travel together. The row
            // carries the stored (possibly encrypted) form of `config_json`,
            // so it goes through `open_row` like every other read.
            let row = super::helpers::write_returning_row(
                &self.pool,
                super::helpers::WriteStatement::Insert(&mut insert),
                &mut connector_select(&id),
                |e| {
                    super::helpers::map_duplicate(e, || {
                        format!("Connector with name '{}' already exists", req.name)
                    })
                },
                || connector_not_found(&id),
            )
            .await?;
            self.open_row(row)
        })
        .await
    }

    async fn get_by_id(&self, id: &str) -> Result<Connector, OrionError> {
        crate::metrics::timed_db_op("connectors.get_by_id", async {
            let (sql, values) = build_sqlx(&mut connector_select(id));

            self.pool
                .fetch_optional_as::<Connector>(&sql, values)
                .await?
                .ok_or_else(|| connector_not_found(id))
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
            // Connectors are unversioned; the one filter is `?tag=` (K6). An
            // empty `Condition::all()` renders no `WHERE` clause.
            let cond = build_condition(filter);
            let page: PaginatedResult<Connector> = super::helpers::paginate(
                &self.pool,
                Page {
                    from: Connectors::Table.into_iden(),
                    projection: Projection::All,
                    cond,
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
            let tags_json = match &req.tags {
                Some(t) => serde_json::to_string(t)?,
                None => existing.tags_json.clone(),
            };

            let mut update = Query::update()
                .table(Connectors::Table)
                .value(Connectors::Name, name)
                .value(Connectors::ConnectorType, connector_type)
                .value(Connectors::ConfigJson, &config_json)
                .value(Connectors::Enabled, enabled)
                .value(Connectors::TagsJson, tags_json.as_str())
                .and_where(Expr::col(Connectors::Id).eq(id))
                .to_owned();

            // D23: the UPDATE and the row it wrote travel together; stored
            // form decrypted through `open_row`, like `create`.
            let row = super::helpers::write_returning_row(
                &self.pool,
                super::helpers::WriteStatement::Update(&mut update),
                &mut connector_select(id),
                OrionError::Storage,
                || connector_not_found(id),
            )
            .await?;
            self.open_row(row)
        })
        .await
    }

    async fn delete(&self, id: &str) -> Result<(), OrionError> {
        crate::metrics::timed_db_op("connectors.delete", async {
            // Read the name first: the OAuth2 runtime state (#268) keys on it,
            // and a deleted connector must not leave token state behind. A raw
            // scalar read, not `get_by_id` — decrypting the config has no
            // business gating a delete.
            let (sql, values) = build_sqlx(
                Query::select()
                    .column(Connectors::Name)
                    .from(Connectors::Table)
                    .and_where(Expr::col(Connectors::Id).eq(id)),
            );
            let name: String = self.pool.fetch_scalar(&sql, values).await.map_err(|_| {
                OrionError::NotFound(format!("Connector '{id}' not found"))
            })?;

            let (sql, values) = build_sqlx(
                Query::delete()
                    .from_table(Connectors::Table)
                    .and_where(Expr::col(Connectors::Id).eq(id)),
            );

            let rows_affected = self.pool.execute_query(&sql, values).await?;

            if rows_affected == 0 {
                return Err(OrionError::NotFound(format!("Connector '{id}' not found")));
            }

            self.delete_oauth_state(&name).await?;
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

    async fn snapshot(&self, filter: &ConnectorFilter) -> Result<Vec<Connector>, OrionError> {
        crate::metrics::timed_db_op("connectors.snapshot", async {
            let rows: Vec<Connector> = super::helpers::snapshot_pages(
                &self.pool,
                super::helpers::EXPORT_PAGE_SIZE,
                |limit, offset| {
                    Query::select()
                        .column(Asterisk)
                        .from(Connectors::Table)
                        .cond_where(build_condition(filter))
                        // `name` is unique — a total order by itself.
                        .order_by(Connectors::Name, Order::Asc)
                        .limit(limit as u64)
                        .offset(offset as u64)
                        .to_owned()
                },
            )
            .await?;
            rows.into_iter().map(|row| self.open_row(row)).collect()
        })
        .await
    }

    async fn get_by_name(&self, name: &str) -> Result<Connector, OrionError> {
        crate::metrics::timed_db_op("connectors.get_by_name", async {
            let (sql, values) = build_sqlx(
                Query::select()
                    .column(Asterisk)
                    .from(Connectors::Table)
                    .and_where(Expr::col(Connectors::Name).eq(name)),
            );

            super::helpers::fetch_required::<Connector>(&self.pool, &sql, values, || {
                OrionError::NotFound(format!("Connector '{name}' not found"))
            })
            .await
            .and_then(|row| self.open_row(row))
        })
        .await
    }

    async fn get_oauth_state(
        &self,
        connector_name: &str,
    ) -> Result<Option<crate::storage::models::ConnectorOauthStateRow>, OrionError> {
        use crate::storage::schema::ConnectorOauthState as S;
        crate::metrics::timed_db_op("connectors.get_oauth_state", async {
            let (sql, values) = build_sqlx(
                Query::select()
                    .columns([S::Fingerprint, S::StateJson])
                    .from(S::Table)
                    .and_where(Expr::col(S::ConnectorName).eq(connector_name)),
            );
            let row = self
                .pool
                .fetch_optional_as::<crate::storage::models::ConnectorOauthStateRow>(&sql, values)
                .await?;
            match row {
                None => Ok(None),
                Some(mut row) => {
                    // The same at-rest contract as `config_json`: decrypt when
                    // the key is set; an encrypted row with no key is a loud
                    // error, never the literal envelope served as state.
                    use crate::storage::config_encryption::ConfigCipher;
                    row.state_json = match &self.cipher {
                        Some(cipher) => cipher.decrypt(&row.state_json)?,
                        None if ConfigCipher::is_encrypted(&row.state_json) => {
                            return Err(OrionError::internal(format!(
                                "oauth state for connector '{connector_name}' is encrypted \
                                 at rest but storage.connector_encryption_key is not set"
                            )));
                        }
                        None => row.state_json,
                    };
                    Ok(Some(row))
                }
            }
        })
        .await
    }

    async fn put_oauth_state(
        &self,
        connector_name: &str,
        fingerprint: &str,
        state_json: &str,
    ) -> Result<(), OrionError> {
        use crate::storage::schema::ConnectorOauthState as S;
        crate::metrics::timed_db_op("connectors.put_oauth_state", async {
            let stored = self.store_form(state_json)?;
            let now = Expr::cust(super::helpers::sql_now(crate::storage::get_backend()));
            let mut insert = Query::insert()
                .into_table(S::Table)
                .columns([S::ConnectorName, S::Fingerprint, S::StateJson, S::UpdatedAt])
                .values_panic([
                    connector_name.into(),
                    fingerprint.into(),
                    stored.into(),
                    now,
                ])
                .to_owned();
            insert.on_conflict(
                sea_query::OnConflict::column(S::ConnectorName)
                    .update_columns([S::Fingerprint, S::StateJson, S::UpdatedAt])
                    .to_owned(),
            );
            let (sql, values) = build_sqlx(&mut insert);
            self.pool.execute_query(&sql, values).await?;
            Ok(())
        })
        .await
    }

    async fn delete_oauth_state(&self, connector_name: &str) -> Result<(), OrionError> {
        use crate::storage::schema::ConnectorOauthState as S;
        crate::metrics::timed_db_op("connectors.delete_oauth_state", async {
            let (sql, values) = build_sqlx(
                Query::delete()
                    .from_table(S::Table)
                    .and_where(Expr::col(S::ConnectorName).eq(connector_name)),
            );
            self.pool.execute_query(&sql, values).await?;
            Ok(())
        })
        .await
    }
}
