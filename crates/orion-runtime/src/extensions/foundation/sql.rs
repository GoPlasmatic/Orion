use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::{
    AttemptContext, ConnectorError, ConnectorOutput, DRIVER_MYSQL, DRIVER_POSTGRESQL,
    DRIVER_SQLITE, EffectStatus, EffectiveConnectionIdentity, ImplementationKind, ProviderBackend,
    SqlLimits, VersionedSessionPool, codes, governed_connection, governed_connection_string,
    refuse_input_target,
};
use crate::extensions::ErrorOrigin;
use crate::extensions::deployment_store::ImmutableDeploymentStore;
use crate::extensions::package_identity::sha256_digest;

/// The input keys a SQL provider reads its statement from. They must be
/// authored as literal strings (`connector_call` refuses an expression at
/// load), so a caller's values reach the database only through `params`,
/// bound beside the statement.
pub(super) const SQL_STATEMENT_KEYS: &[&str] = &["sql", "source", "target"];

/// The statement of a SQL operation: the first of `keys` present, as a string.
fn sql_statement<'a>(
    input: &'a Value,
    keys: &[&str],
    driver: &str,
) -> Result<&'a str, ConnectorError> {
    keys.iter()
        .find_map(|key| input.get(*key))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ConnectorError::invalid_input(
                format!(
                    "{driver} operation requires a string '{}'",
                    keys.join("' or '")
                ),
                Some(json!({"pointer": format!("/{}", keys[0])})),
            )
        })
}

/// The values bound to the statement's placeholders: `input.params`, an array.
fn sql_params(input: &Value) -> Result<Vec<Value>, ConnectorError> {
    match input.get("params") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items.clone()),
        Some(_) => Err(ConnectorError::invalid_input(
            "'params' must be an array of the values bound to the statement's placeholders",
            Some(json!({"pointer": "/params"})),
        )),
    }
}

/// A read or a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SqlOperation {
    Query,
    Execute,
}

impl SqlOperation {
    fn parse(operation: &str) -> Option<Self> {
        match operation {
            "query" | "data_query" => Some(Self::Query),
            "execute" | "data_write" => Some(Self::Execute),
            _ => None,
        }
    }

    /// The input keys the statement is read from.
    fn statement_keys(self) -> &'static [&'static str] {
        match self {
            Self::Query => &["sql", "source"],
            Self::Execute => &["sql", "target"],
        }
    }
}

/// Whether the database rolled the statement back and asked for it to be
/// retried: SQLSTATE `40001` (a serialization failure; MySQL's deadlock,
/// error 1213, reports it too) or PostgreSQL's deadlock `40P01`, and MySQL's
/// lock wait timeout (error 1205), which reports only the generic `HY000`.
fn database_asks_for_retry(db: &dyn sqlx::error::DatabaseError) -> bool {
    matches!(db.code().as_deref(), Some("40001" | "40P01"))
        || db
            .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
            .is_some_and(|my| matches!(my.number(), 1205 | 1213))
}

/// A driver failure, classified the way `db_read` classifies one. A
/// transport failure is retryable; so is a serialization failure, a
/// deadlock or a lock wait timeout, which is the database asking for the
/// retry. Anything else the database raised is about the statement, and
/// retrying will not change it. `in_flight` is what the call may have done
/// when the transport failed.
fn sql_error(driver: &str, e: sqlx::Error, in_flight: EffectStatus) -> ConnectorError {
    match &e {
        sqlx::Error::Database(db) if database_asks_for_retry(db.as_ref()) => {
            ConnectorError::unavailable(format!("{driver}: {db}"), EffectStatus::NotStarted)
        }
        sqlx::Error::Database(db) => {
            let mut err = ConnectorError::invalid_input(format!("{driver}: {db}"), None);
            err.origin = ErrorOrigin::provider("statement");
            err
        }
        sqlx::Error::Io(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::Protocol(_)
        | sqlx::Error::PoolTimedOut
        | sqlx::Error::PoolClosed
        | sqlx::Error::WorkerCrashed => {
            ConnectorError::unavailable(format!("{driver}: {e}"), in_flight)
        }
        _ => ConnectorError::invalid_input(format!("{driver}: {e}"), None),
    }
}

/// Run one statement on `pool` with `params` bound beside it, through the
/// same binders and row decoders `db_read` uses. A query's rows are counted
/// and sized as they stream, and the call fails as soon as either cap is
/// crossed, before the rest is held in memory.
async fn run_sql(
    pool: &crate::connector::pool_cache::SqlPool,
    driver: &str,
    operation: SqlOperation,
    statement: &str,
    params: &[Value],
    limits: SqlLimits,
) -> Result<Value, ConnectorError> {
    use crate::connector::sql_encode::{Bound, Scalar};

    let in_flight = match operation {
        SqlOperation::Query => EffectStatus::NotStarted,
        SqlOperation::Execute => EffectStatus::Unknown,
    };
    let scalars: Vec<Scalar> = params.iter().map(Into::into).collect();
    crate::connector::pool_cache::dispatch_sql_pool!(
        pool, p, rows_to_json, bind, typed_args, write_result => {
            let mut conn = p
                .acquire()
                .await
                .map_err(|e| sql_error(driver, e, EffectStatus::NotStarted))?;
            let bound = typed_args(&mut conn, statement, Some(&scalars))
                .await
                .map_err(|e| {
                    ConnectorError::invalid_input(
                        e.message(driver),
                        Some(json!({"pointer": format!("/params/{}", e.param)})),
                    )
                })?;
            // The statement is authored in the definition (`connector_call`
            // refuses one built from the message at load); the message's
            // values travel as bind parameters beside it.
            let query = match bound {
                Bound::Typed(args) => sqlx::query_with(sqlx::AssertSqlSafe(statement), args),
                Bound::Fallback { cache } => {
                    bind(sqlx::query(sqlx::AssertSqlSafe(statement)), params).persistent(cache)
                }
            };
            match operation {
                SqlOperation::Query => {
                    use futures::TryStreamExt;
                    let mut stream = query.fetch(&mut *conn);
                    let mut rows = Vec::new();
                    let mut total_bytes = 0usize;
                    while let Some(row) = stream
                        .try_next()
                        .await
                        .map_err(|e| sql_error(driver, e, in_flight))?
                    {
                        if rows.len() >= limits.max_rows {
                            return Err(ConnectorError::invalid_input(
                                format!(
                                    "{driver}: result exceeds {} rows; add a LIMIT to the query",
                                    limits.max_rows
                                ),
                                None,
                            ));
                        }
                        let Some(decoded) = rows_to_json(
                            std::slice::from_ref(&row),
                            crate::connector::sql_decode::RowFormat::default(),
                        )
                        .map_err(|e| ConnectorError::invalid_input(e.message(driver), None))?
                        .pop() else {
                            continue;
                        };
                        if limits.max_result_bytes > 0 {
                            total_bytes = total_bytes.saturating_add(
                                crate::connector::sql_decode::approx_json_value_bytes(&decoded),
                            );
                            if total_bytes > limits.max_result_bytes {
                                return Err(ConnectorError::invalid_input(
                                    format!(
                                        "{driver}: result exceeds {} bytes; select fewer \
                                         columns or add a LIMIT",
                                        limits.max_result_bytes
                                    ),
                                    None,
                                ));
                            }
                        }
                        rows.push(decoded);
                    }
                    Ok(json!({
                        "count": rows.len(),
                        "rows": rows,
                        "statement": statement,
                    }))
                }
                SqlOperation::Execute => {
                    let result = query
                        .execute(&mut *conn)
                        .await
                        .map_err(|e| sql_error(driver, e, in_flight))?;
                    let (rows_affected, last_insert_id) = write_result(&result);
                    let mut out = json!({
                        "rows_affected": rows_affected,
                        "statement": statement,
                    });
                    if let Some(id) = last_insert_id {
                        out["last_insert_id"] = json!(id);
                    }
                    Ok(out)
                }
            }
        }
    )
}

#[doc(hidden)]
#[async_trait::async_trait]
pub trait RemoteSqlDialect: Send + Sync + 'static {
    const DRIVER_ID: &'static str;
    const DISPLAY_NAME: &'static str;
    const SESSION_PREFIX: &'static str;
    const CACHE_NAME: &'static str;
    const SCHEMES: &'static [&'static str];
    const SCHEME_ERR: &'static str;
    type Pool: Clone + Send + Sync + 'static;

    fn close_pool(pool: Self::Pool);
    async fn connect_pool(url: &str) -> Result<Self::Pool, sqlx::Error>;
    fn wrap_pool(pool: Self::Pool) -> crate::connector::pool_cache::SqlPool;
}

#[doc(hidden)]
pub struct PostgresDialect;

#[async_trait::async_trait]
impl RemoteSqlDialect for PostgresDialect {
    const DRIVER_ID: &'static str = DRIVER_POSTGRESQL;
    const DISPLAY_NAME: &'static str = "PostgreSQL";
    const SESSION_PREFIX: &'static str = "pg-session";
    const CACHE_NAME: &'static str = "v2_postgres";
    const SCHEMES: &'static [&'static str] = &["postgres://", "postgresql://"];
    const SCHEME_ERR: &'static str =
        "org.orion.postgresql requires a postgres:// or postgresql:// connection_string";
    type Pool = sqlx::PgPool;

    fn close_pool(pool: Self::Pool) {
        tokio::spawn(async move {
            pool.close().await;
        });
    }

    async fn connect_pool(url: &str) -> Result<Self::Pool, sqlx::Error> {
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(5))
            .connect(url)
            .await
    }

    fn wrap_pool(pool: Self::Pool) -> crate::connector::pool_cache::SqlPool {
        crate::connector::pool_cache::SqlPool::Postgres(pool)
    }
}

#[doc(hidden)]
pub struct MysqlDialect;

#[async_trait::async_trait]
impl RemoteSqlDialect for MysqlDialect {
    const DRIVER_ID: &'static str = DRIVER_MYSQL;
    const DISPLAY_NAME: &'static str = "MySQL";
    const SESSION_PREFIX: &'static str = "mysql-session";
    const CACHE_NAME: &'static str = "v2_mysql";
    const SCHEMES: &'static [&'static str] = &["mysql://", "mariadb://"];
    const SCHEME_ERR: &'static str =
        "org.orion.mysql requires a mysql:// or mariadb:// connection_string";
    type Pool = sqlx::MySqlPool;

    fn close_pool(pool: Self::Pool) {
        tokio::spawn(async move {
            pool.close().await;
        });
    }

    async fn connect_pool(url: &str) -> Result<Self::Pool, sqlx::Error> {
        sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(5))
            .connect(url)
            .await
    }

    fn wrap_pool(pool: Self::Pool) -> crate::connector::pool_cache::SqlPool {
        crate::connector::pool_cache::SqlPool::MySql(pool)
    }
}

/// Shared Direct Rust SQL connector backend for network-dialled SQL engines
/// ([`PostgresProviderBackend`] and [`MysqlProviderBackend`]).
pub struct RemoteSqlProviderBackend<D: RemoteSqlDialect> {
    /// Admission, not pooling. The cached value is only a label (it is what an
    /// invocation reports as its `session` metadata); what this layer does is
    /// hold each invocation to the connection's capacity ceiling, shared
    /// across its revisions and rotated secrets, and refuse an
    /// identity once `dispose` has invalidated it.
    sessions: Arc<VersionedSessionPool<String>>,
    /// The connections themselves, keyed by identity and a digest of the
    /// resolved target, closed when evicted.
    live_pools: crate::connector::lru_cache::LruCache<D::Pool>,
    deployments: Option<ImmutableDeploymentStore>,
    limits: SqlLimits,
}

impl<D: RemoteSqlDialect> RemoteSqlProviderBackend<D> {
    pub fn new(max_capacity_per_connection: usize) -> Self {
        Self {
            sessions: Arc::new(VersionedSessionPool::new(
                D::DRIVER_ID,
                max_capacity_per_connection,
            )),
            live_pools: crate::connector::lru_cache::LruCache::with_evict_handler(
                max_capacity_per_connection.max(4),
                D::CACHE_NAME,
                D::close_pool,
            ),
            deployments: None,
            limits: SqlLimits::default(),
        }
    }

    pub fn with_deployment_store(
        max_capacity_per_connection: usize,
        deployments: ImmutableDeploymentStore,
    ) -> Self {
        let mut backend = Self::new(max_capacity_per_connection);
        backend.deployments = Some(deployments);
        backend
    }

    /// The same provider, holding every query to `limits`.
    pub fn with_limits(mut self, limits: SqlLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn session_pool(&self) -> Arc<VersionedSessionPool<String>> {
        self.sessions.clone()
    }

    async fn resolve_connection_target(
        &self,
        ctx: &AttemptContext,
    ) -> Result<(String, bool), ConnectorError> {
        let identity = ctx.effective_identity();
        let allow_private_urls = ctx
            .public_config()
            .and_then(|cfg| cfg.get("allow_private_urls").and_then(Value::as_bool))
            .or_else(|| {
                governed_connection(self.deployments.as_ref(), identity).and_then(|conn| {
                    conn.public_config
                        .get("allow_private_urls")
                        .and_then(Value::as_bool)
                })
            })
            .unwrap_or(false);
        let from_ctx = match ctx.resolve_secret("connection_string").await? {
            Some(secret) => Some(secret.expose_secret().to_string()),
            None => ctx
                .public_config()
                .and_then(|cfg| cfg.get("connection_string").and_then(Value::as_str))
                .map(str::to_string),
        };
        let conn_str = match from_ctx {
            Some(s) => Some(s),
            None => {
                governed_connection_string(self.deployments.as_ref(), identity, D::DRIVER_ID)
                    .await?
            }
        };
        let Some(conn_str) = conn_str else {
            return Err(ConnectorError::invalid_input(
                format!(
                    "{} operation on '{}' requires a configured 'connection_string'",
                    D::DISPLAY_NAME,
                    identity.connection_id
                ),
                Some(json!({"pointer": "/connection_string"})),
            ));
        };
        self.validate_config(&json!({"connection_string": conn_str}))?;
        Ok((conn_str, allow_private_urls))
    }

    async fn execute_live(
        &self,
        identity: &EffectiveConnectionIdentity,
        conn_str: &str,
        allow_private_urls: bool,
        operation: SqlOperation,
        statement: &str,
        params: &[Value],
    ) -> Result<Value, ConnectorError> {
        // Keyed by connector name with a version digest of revision + target:
        // a resolved connection string carries its credential, so only its
        // digest is stored, while `evict(connection_id)` closes all revisions.
        let version_key = format!(
            "r{}::{}::{allow_private_urls}",
            identity.revision,
            sha256_digest(conn_str.as_bytes())
        );
        let conn_id = identity.connection_id.to_string();
        let pool = self
            .live_pools
            .get_or_create_with_version(identity.connection_id.as_str(), &version_key, || {
                let target_url = conn_str.to_string();
                async move {
                    crate::connector::endpoints::check_db_connection_string(
                        &conn_id,
                        &target_url,
                        allow_private_urls,
                    )
                    .await
                    .map_err(|e| {
                        ConnectorError::forbidden(
                            codes::CAPABILITY_INVALID,
                            "ssrf_validation",
                            format!(
                                "Outbound {} target blocked by SSRF guard: {e}",
                                D::DISPLAY_NAME
                            ),
                            EffectStatus::NotStarted,
                        )
                    })?;
                    D::connect_pool(&target_url)
                        .await
                        .map_err(|e| sql_error(D::DRIVER_ID, e, EffectStatus::NotStarted))
                }
            })
            .await?;
        run_sql(
            &D::wrap_pool(pool),
            D::DRIVER_ID,
            operation,
            statement,
            params,
            self.limits,
        )
        .await
    }
}

impl<D: RemoteSqlDialect> Default for RemoteSqlProviderBackend<D> {
    fn default() -> Self {
        Self::new(16)
    }
}

#[async_trait::async_trait]
impl<D: RemoteSqlDialect> orion_extension_api::ConnectionPoolCache for RemoteSqlProviderBackend<D> {
    fn cache_id(&self) -> &str {
        D::DRIVER_ID
    }

    async fn evict(&self, connector_name: &str) {
        self.sessions.evict(connector_name).await;
        self.live_pools.evict(connector_name).await;
    }

    async fn evict_all(&self) {
        self.sessions.evict_all().await;
        self.live_pools.evict_all().await;
    }
}

#[async_trait::async_trait]
impl<D: RemoteSqlDialect> ProviderBackend for RemoteSqlProviderBackend<D> {
    fn kind(&self) -> ImplementationKind {
        ImplementationKind::Rust
    }

    fn driver_id(&self) -> &str {
        D::DRIVER_ID
    }

    fn validate_config(&self, config: &Value) -> Result<(), ConnectorError> {
        let Some(conn_str) = config.get("connection_string").and_then(Value::as_str) else {
            return Err(ConnectorError::invalid_input(
                format!("{} connector requires 'connection_string'", D::DISPLAY_NAME),
                Some(json!({"pointer": "/connection_string"})),
            ));
        };
        let lower = conn_str.trim().to_ascii_lowercase();
        if !D::SCHEMES.iter().any(|scheme| lower.starts_with(scheme)) {
            return Err(ConnectorError::invalid_input(
                D::SCHEME_ERR,
                Some(json!({"pointer": "/connection_string"})),
            ));
        }
        Ok(())
    }

    async fn invoke(
        &self,
        ctx: &AttemptContext,
        operation: &str,
        input: Value,
    ) -> Result<ConnectorOutput, ConnectorError> {
        ctx.check_live()?;

        let identity = ctx.effective_identity();
        let handle = self
            .sessions
            .acquire(identity, || async move {
                Ok(format!(
                    "{}:{}:r{}",
                    D::SESSION_PREFIX,
                    identity.connection_id,
                    identity.revision
                ))
            })
            .await?;

        let Some(sql_op) = SqlOperation::parse(operation) else {
            return Err(ConnectorError::forbidden(
                codes::OPERATION_FORBIDDEN,
                "dispatch",
                format!("Unsupported {} operation '{operation}'", D::DISPLAY_NAME),
                ctx.effect_status(),
            ));
        };
        refuse_input_target(&input, &["connection_string"])?;
        let statement = sql_statement(&input, sql_op.statement_keys(), D::DRIVER_ID)?;
        let params = sql_params(&input)?;
        let (conn_str, allow_private_urls) = self.resolve_connection_target(ctx).await?;
        ctx.record_effect(EffectStatus::Unknown);
        let data = self
            .execute_live(
                identity,
                &conn_str,
                allow_private_urls,
                sql_op,
                statement,
                &params,
            )
            .await?;
        ctx.record_effect(EffectStatus::Completed);
        let mut metadata = BTreeMap::new();
        metadata.insert("driver".to_string(), json!(D::DRIVER_ID));
        metadata.insert("session".to_string(), json!(handle.session()));
        Ok(ConnectorOutput::with_metadata(data, metadata))
    }

    fn literal_input_keys(&self, _operation: &str) -> Vec<String> {
        SQL_STATEMENT_KEYS
            .iter()
            .map(|k| (*k).to_string())
            .collect()
    }

    async fn dispose(&self, identity: &EffectiveConnectionIdentity) {
        self.sessions.invalidate(identity, None);
        self.live_pools.evict(identity.connection_id.as_str()).await;
    }
}

/// Community foundation PostgreSQL connector provider (`org.orion.postgresql`).
pub type PostgresProviderBackend = RemoteSqlProviderBackend<PostgresDialect>;

/// Community foundation MySQL and MariaDB connector provider (`org.orion.mysql`).
///
/// The same operations as [`PostgresProviderBackend`]: `query` and `execute`,
/// a literal statement with `params` bound to its `?` placeholders, and a
/// target taken only from the connection revision.
pub type MysqlProviderBackend = RemoteSqlProviderBackend<MysqlDialect>;

/// Community foundation scoped SQLite connector provider (`org.orion.sqlite`).
///
/// Enforces strict root-directory sandboxing:
/// - `sqlite::memory:` is always permitted (isolated per connection identity).
/// - File-backed SQLite URLs (`sqlite://...` or `sqlite:...`) must resolve beneath
///   `approved_root`, containing no `..` traversal components or symlink escapes,
///   and must not target Orion's own state database filenames.
pub struct SqliteProviderBackend {
    approved_root: PathBuf,
    state_db: Option<PathBuf>,
    sessions: Arc<VersionedSessionPool<String>>,
    live_pools: crate::connector::lru_cache::LruCache<sqlx::SqlitePool>,
    deployments: Option<ImmutableDeploymentStore>,
    limits: SqlLimits,
}

impl SqliteProviderBackend {
    pub fn new(approved_root: impl Into<PathBuf>, max_capacity_per_connection: usize) -> Self {
        Self {
            approved_root: approved_root.into(),
            state_db: None,
            sessions: Arc::new(VersionedSessionPool::new(
                DRIVER_SQLITE,
                max_capacity_per_connection,
            )),
            live_pools: crate::connector::lru_cache::LruCache::with_evict_handler(
                max_capacity_per_connection.max(4),
                "v2_sqlite",
                |pool: sqlx::SqlitePool| {
                    tokio::spawn(async move {
                        pool.close().await;
                    });
                },
            ),
            deployments: None,
            limits: SqlLimits::default(),
        }
    }

    /// The same provider, holding every query to `limits`.
    pub fn with_limits(mut self, limits: SqlLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Record Orion's own SQLite state-database path so a SQLite connection
    /// cannot open it or its WAL/SHM/journal sidecars even under a custom name.
    pub fn with_state_db(mut self, state_db: Option<PathBuf>) -> Self {
        self.state_db = state_db;
        self
    }

    pub fn with_deployment_store(
        approved_root: impl Into<PathBuf>,
        max_capacity_per_connection: usize,
        deployments: ImmutableDeploymentStore,
    ) -> Self {
        let mut backend = Self::new(approved_root, max_capacity_per_connection);
        backend.deployments = Some(deployments);
        backend
    }

    /// The governed revision's connection string, or an in-memory database
    /// private to the connection when no revision names one.
    async fn resolve_connection_string(
        &self,
        ctx: &AttemptContext,
    ) -> Result<String, ConnectorError> {
        let identity = ctx.effective_identity();
        if let Some(secret) = ctx.resolve_secret("connection_string").await? {
            return Ok(secret.expose_secret().to_string());
        }
        if let Some(conn_str) = ctx
            .public_config()
            .and_then(|cfg| cfg.get("connection_string").and_then(Value::as_str))
        {
            return Ok(conn_str.to_string());
        }
        Ok(
            governed_connection_string(self.deployments.as_ref(), identity, DRIVER_SQLITE)
                .await?
                .unwrap_or_else(|| "sqlite::memory:".to_string()),
        )
    }

    pub fn session_pool(&self) -> Arc<VersionedSessionPool<String>> {
        self.sessions.clone()
    }

    /// Resolve a confined filename without serializing and parsing another URL.
    pub fn resolve_sandboxed_target(
        &self,
        conn_str: &str,
    ) -> Result<sqlx::sqlite::SqliteConnectOptions, ConnectorError> {
        use crate::connector::sqlite_target::{SqliteTargetError, confined_options_with_state_db};
        confined_options_with_state_db(&self.approved_root, conn_str, self.state_db.as_deref())
            .map_err(|e| match e {
                SqliteTargetError::Boundary(_) => ConnectorError::forbidden(
                    codes::CAPABILITY_INVALID,
                    "validation",
                    e.to_string(),
                    EffectStatus::NotStarted,
                ),
                _ => ConnectorError::invalid_input(
                    e.to_string(),
                    Some(json!({"pointer": "/connection_string"})),
                ),
            })
    }

    /// Execute a real SQL statement against a sandboxed SQLite target.
    #[cfg(test)]
    pub async fn execute_live_sqlite(
        &self,
        conn_str: &str,
        operation: &str,
        statement: &str,
    ) -> Result<Value, ConnectorError> {
        self.execute_live_sqlite_for_connection(None, conn_str, operation, statement, &[])
            .await
    }

    /// Execute a real SQL statement against a sandboxed SQLite target, with
    /// `params` bound to its placeholders, optionally partitioning
    /// `sqlite::memory:` pools by `connection_key` so tables persist across
    /// statements on the same connection.
    pub async fn execute_live_sqlite_for_connection(
        &self,
        connection_key: Option<&str>,
        conn_str: &str,
        operation: &str,
        statement: &str,
        params: &[Value],
    ) -> Result<Value, ConnectorError> {
        let Some(sql_op) = SqlOperation::parse(operation) else {
            return Err(ConnectorError::forbidden(
                codes::OPERATION_FORBIDDEN,
                "dispatch",
                format!("Unsupported SQLite operation '{operation}'"),
                EffectStatus::NotStarted,
            ));
        };
        let options = self.resolve_sandboxed_target(conn_str)?;
        let in_memory = crate::connector::sqlite_target::is_memory_url(conn_str);
        // Include the original URL options and avoid re-encoding filenames for
        // identity: a literal percent escape and a decoded character differ.
        let target = if in_memory {
            conn_str.trim().to_string()
        } else {
            format!("{:?}", (options.get_filename(), conn_str.trim()))
        };
        let connector_name = connection_key
            .map(|k| k.split_once(':').map_or(k, |(id, _)| id))
            .unwrap_or("<sqlite>");
        let version_key = match connection_key {
            Some(key) if in_memory => format!("{key}::{target}"),
            _ => target,
        };
        let pool = self
            .live_pools
            .get_or_create_with_version(connector_name, &version_key, || {
                let target_label = connection_key.unwrap_or("<sqlite>").to_string();
                async move {
                    let builder = if in_memory {
                        sqlx::sqlite::SqlitePoolOptions::new().max_connections(1)
                    } else {
                        sqlx::sqlite::SqlitePoolOptions::new().max_connections(4)
                    };
                    builder.connect_with(options).await.map_err(|e| {
                        ConnectorError::invalid_input(
                            format!(
                                "Failed to connect to sandboxed SQLite target '{target_label}': {e}"
                            ),
                            None,
                        )
                    })
                }
            })
            .await?;
        run_sql(
            &crate::connector::pool_cache::SqlPool::Sqlite(pool),
            DRIVER_SQLITE,
            sql_op,
            statement,
            params,
            self.limits,
        )
        .await
    }
}

impl Default for SqliteProviderBackend {
    fn default() -> Self {
        Self::new("./data/connectors/sqlite", 8)
    }
}

#[async_trait::async_trait]
impl orion_extension_api::ConnectionPoolCache for SqliteProviderBackend {
    fn cache_id(&self) -> &str {
        DRIVER_SQLITE
    }

    async fn evict(&self, connector_name: &str) {
        self.sessions.evict(connector_name).await;
        self.live_pools.evict(connector_name).await;
    }

    async fn evict_all(&self) {
        self.sessions.evict_all().await;
        self.live_pools.evict_all().await;
    }
}

#[async_trait::async_trait]
impl ProviderBackend for SqliteProviderBackend {
    fn kind(&self) -> ImplementationKind {
        ImplementationKind::Rust
    }

    fn driver_id(&self) -> &str {
        DRIVER_SQLITE
    }

    fn validate_config(&self, config: &Value) -> Result<(), ConnectorError> {
        let Some(conn_str) = config.get("connection_string").and_then(Value::as_str) else {
            return Err(ConnectorError::invalid_input(
                "SQLite connector requires 'connection_string'",
                Some(json!({"pointer": "/connection_string"})),
            ));
        };
        let _ = self.resolve_sandboxed_target(conn_str)?;
        Ok(())
    }

    async fn invoke(
        &self,
        ctx: &AttemptContext,
        operation: &str,
        input: Value,
    ) -> Result<ConnectorOutput, ConnectorError> {
        ctx.check_live()?;

        let identity = ctx.effective_identity();
        let handle = self
            .sessions
            .acquire(identity, || async move {
                Ok(format!(
                    "sqlite-session:{}:r{}",
                    identity.connection_id, identity.revision
                ))
            })
            .await?;

        let conn_key = format!("{}:r{}", identity.connection_id, identity.revision);
        let Some(sql_op) = SqlOperation::parse(operation) else {
            return Err(ConnectorError::forbidden(
                codes::OPERATION_FORBIDDEN,
                "dispatch",
                format!("Unsupported SQLite operation '{operation}'"),
                ctx.effect_status(),
            ));
        };
        refuse_input_target(&input, &["connection_string"])?;
        let statement = sql_statement(&input, sql_op.statement_keys(), DRIVER_SQLITE)?;
        let params = sql_params(&input)?;
        let target_conn = self.resolve_connection_string(ctx).await?;
        ctx.record_effect(EffectStatus::Unknown);
        let data = self
            .execute_live_sqlite_for_connection(
                Some(&conn_key),
                &target_conn,
                operation,
                statement,
                &params,
            )
            .await?;
        ctx.record_effect(EffectStatus::Completed);
        let mut metadata = BTreeMap::new();
        metadata.insert("driver".to_string(), json!(DRIVER_SQLITE));
        metadata.insert("session".to_string(), json!(handle.session()));
        Ok(ConnectorOutput::with_metadata(data, metadata))
    }

    fn literal_input_keys(&self, _operation: &str) -> Vec<String> {
        SQL_STATEMENT_KEYS
            .iter()
            .map(|k| (*k).to_string())
            .collect()
    }

    async fn dispose(&self, identity: &EffectiveConnectionIdentity) {
        self.sessions.invalidate(identity, None);
        self.live_pools.evict(identity.connection_id.as_str()).await;
    }
}
