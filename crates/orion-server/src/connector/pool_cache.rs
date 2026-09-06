//! Dynamic SQL connection pool cache for external database connectors.
//!
//! Lazily creates and caches [`SqlPool`] connections keyed by connector name.
//! The backend is selected at runtime from the connection-string URL scheme.
//!
//! These were `sqlx::AnyPool` until #309. `Any` maps a driver's type info
//! through a nine-variant whitelist, so a Postgres connector could decode nine
//! types and failed the task on `uuid`, `numeric`, `timestamptz`, `json`,
//! arrays and enums — per row, so a query passed against an empty table and
//! failed the first time production had data. Holding the concrete pool is what
//! lets [`crate::connector::sql_decode`] see a real type.
//!
//! The enum is not a new idea here: [`crate::storage::DbPool`] has wrapped the
//! same three pools behind a dispatch macro since 1.0, for Orion's own
//! database. This is the half that never got it.

use std::time::Duration;

use sqlx::mysql::MySqlPoolOptions;
use sqlx::postgres::PgPoolOptions;
use sqlx::sqlite::SqlitePoolOptions;

use super::lru_cache::LruCache;
use crate::connector::DbConnectorConfig;
use crate::errors::OrionError;

/// A connector's connection pool, on the driver it actually speaks.
#[derive(Clone)]
pub enum SqlPool {
    Postgres(sqlx::PgPool),
    MySql(sqlx::MySqlPool),
    Sqlite(sqlx::SqlitePool),
}

/// Dispatch an expression across all three pool variants.
///
/// `$p` binds the concrete pool, `$rows_to_json` the decoder that reads its
/// rows, `$bind` the value-shaped binder for its arguments and `$typed_args`
/// the one that asks the server what it declared first. The body is
/// type-checked independently per arm, which is what lets one expression cover
/// three unrelated row types — the same trick `storage::dispatch_pool!` uses,
/// and what lets `$typed_args` do real work on PostgreSQL and nothing on the
/// two backends that never had the problem.
macro_rules! dispatch_sql_pool {
    ($self:expr, $p:ident, $rows_to_json:ident, $bind:ident, $typed_args:ident, $write_result:ident
     => $body:expr) => {
        match $self {
            crate::connector::pool_cache::SqlPool::Postgres($p) => {
                let $rows_to_json = crate::connector::sql_decode::pg_rows_to_json;
                let $bind = crate::connector::pool_cache::bind_params::<sqlx::Postgres>;
                let $typed_args = crate::connector::sql_encode::pg_typed_args;
                let $write_result = crate::connector::pool_cache::pg_write_result;
                $body
            }
            crate::connector::pool_cache::SqlPool::MySql($p) => {
                let $rows_to_json = crate::connector::sql_decode::mysql_rows_to_json;
                let $bind = crate::connector::pool_cache::bind_params::<sqlx::MySql>;
                let $typed_args = crate::connector::sql_encode::mysql_typed_args;
                let $write_result = crate::connector::pool_cache::mysql_write_result;
                $body
            }
            crate::connector::pool_cache::SqlPool::Sqlite($p) => {
                let $rows_to_json = crate::connector::sql_decode::sqlite_rows_to_json;
                let $bind = crate::connector::pool_cache::bind_params::<sqlx::Sqlite>;
                let $typed_args = crate::connector::sql_encode::sqlite_typed_args;
                let $write_result = crate::connector::pool_cache::sqlite_write_result;
                $body
            }
        }
    };
    ($self:expr, $p:ident, $rows_to_json:ident, $bind:ident, $write_result:ident => $body:expr) => {
        crate::connector::pool_cache::dispatch_sql_pool!(
            $self, $p, $rows_to_json, $bind, _typed_args, $write_result => $body
        )
    };
    ($self:expr, $p:ident, $rows_to_json:ident, $bind:ident => $body:expr) => {
        crate::connector::pool_cache::dispatch_sql_pool!(
            $self, $p, $rows_to_json, $bind, _typed_args, _write_result => $body
        )
    };
}
pub(crate) use dispatch_sql_pool;

impl SqlPool {
    /// The dialect this pool speaks, for the portable query builder.
    pub fn dialect(&self) -> crate::query::SqlDialect {
        match self {
            SqlPool::Postgres(_) => crate::query::SqlDialect::Postgres,
            SqlPool::MySql(_) => crate::query::SqlDialect::Mysql,
            SqlPool::Sqlite(_) => crate::query::SqlDialect::Sqlite,
        }
    }

    /// Whether the driver reports an auto-increment id after an insert.
    ///
    /// MySQL and SQLite do; PostgreSQL does not, and never did — `AnyQueryResult`
    /// simply reported `0` there, so `last_insert_id` was a field that looked
    /// answered and was not. Postgres uses `RETURNING`, which the dialect
    /// already supports.
    pub fn reports_last_insert_id(&self) -> bool {
        !matches!(self, SqlPool::Postgres(_))
    }

    /// `SELECT 1` through the pool — the connectivity probe, and the one
    /// statement every backend spells identically.
    pub async fn ping(&self) -> Result<(), sqlx::Error> {
        dispatch_sql_pool!(self, p, _d, _b => sqlx::query("SELECT 1").execute(p).await.map(|_| ()))
    }

    /// Whether the pool has been closed — an evicted pool is closed on a
    /// detached task, and the cluster tests assert on that transition.
    pub fn is_closed(&self) -> bool {
        dispatch_sql_pool!(self, p, _d, _b => p.is_closed())
    }

    async fn close(self) {
        dispatch_sql_pool!(self, p, _d, _b => p.close().await)
    }
}

/// Lazily creates and caches SQL connection pools keyed by connector name.
/// Bounded by `max_entries` with LRU eviction.
pub struct SqlPoolCache {
    cache: LruCache<SqlPool>,
}

impl SqlPoolCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            // F17: close evicted pools on a detached task — new acquires on
            // the closed pool fail fast, in-flight queries finish, and the
            // TCP connections are returned instead of counting against the
            // remote DB's max_connections until the last Arc drops.
            cache: LruCache::with_evict_handler(max_entries, "sql_pool", |pool: SqlPool| {
                tokio::spawn(async move { pool.close().await });
            }),
        }
    }

    /// Get or lazily create a pool for the named connector.
    pub async fn get_pool(
        &self,
        connector_name: &str,
        config: &DbConnectorConfig,
    ) -> Result<SqlPool, OrionError> {
        let conn_str = config.connection_string.clone();
        let max_conns = config.max_connections.unwrap_or(5);
        let connect_timeout = config.connect_timeout_ms.unwrap_or(5000);

        self.cache
            .get_or_create(connector_name, || async move {
                // S6: refuse a private/internal target before dialling. Only on
                // the create path — a cached pool was checked when it was
                // opened, and re-resolving per query would put a DNS round trip
                // on the hot path.
                crate::validation::check_db_endpoint(connector_name, config).await?;

                let timeout = Duration::from_millis(connect_timeout);
                // The same classifier the server's own `[storage]` URL goes
                // through, so a connector and the state database agree on what
                // `postgres://` means.
                let pool = match crate::storage::detect_backend(&conn_str)? {
                    crate::storage::DbBackend::Postgres => SqlPool::Postgres(
                        PgPoolOptions::new()
                            .max_connections(max_conns)
                            .acquire_timeout(timeout)
                            .connect(&conn_str)
                            .await
                            .map_err(|e| connect_failed(connector_name, e))?,
                    ),
                    crate::storage::DbBackend::Mysql => SqlPool::MySql(
                        MySqlPoolOptions::new()
                            .max_connections(max_conns)
                            .acquire_timeout(timeout)
                            .connect(&conn_str)
                            .await
                            .map_err(|e| connect_failed(connector_name, e))?,
                    ),
                    crate::storage::DbBackend::Sqlite => SqlPool::Sqlite(
                        SqlitePoolOptions::new()
                            .max_connections(max_conns)
                            .acquire_timeout(timeout)
                            .connect(&conn_str)
                            .await
                            .map_err(|e| connect_failed(connector_name, e))?,
                    ),
                };
                Ok(pool)
            })
            .await
    }

    /// Evict a cached pool (e.g., when connector config changes).
    pub async fn evict(&self, connector_name: &str) {
        self.cache.evict(connector_name).await;
    }

    pub async fn evict_all(&self) {
        self.cache.evict_all().await;
    }
}

impl Default for SqlPoolCache {
    fn default() -> Self {
        Self::new(100)
    }
}

/// What a write reports back: rows affected, and an auto-increment id where the
/// driver has one.
///
/// The three `QueryResult` types share no trait carrying `rows_affected`, and
/// that is the honest shape of the difference rather than an inconvenience:
/// MySQL and SQLite hand back the id of the row they just inserted, and
/// PostgreSQL does not — it uses `RETURNING`. Under `sqlx::Any` this was
/// flattened into one method that answered `0` on Postgres, so
/// `last_insert_id` looked answered and was not.
pub(crate) fn pg_write_result(r: &sqlx::postgres::PgQueryResult) -> (u64, Option<i64>) {
    (r.rows_affected(), None)
}

pub(crate) fn mysql_write_result(r: &sqlx::mysql::MySqlQueryResult) -> (u64, Option<i64>) {
    // `u64` on the wire; the values that fit an `i64` are every id a real
    // table reaches, and JSON has no wider integer anyway.
    (r.rows_affected(), i64::try_from(r.last_insert_id()).ok())
}

pub(crate) fn sqlite_write_result(r: &sqlx::sqlite::SqliteQueryResult) -> (u64, Option<i64>) {
    (r.rows_affected(), Some(r.last_insert_rowid()))
}

fn connect_failed(connector_name: &str, e: sqlx::Error) -> OrionError {
    OrionError::Internal {
        context: format!("Failed to connect to external DB '{connector_name}'"),
        source: Some(Box::new(e)),
    }
}

/// Bind a workflow's JSON parameters to a prepared statement.
///
/// One implementation over all three drivers rather than three near-identical
/// ones. The `where` clause is long, but it is the same list `storage`'s typed
/// fetches carry and it buys the property that matters: a parameter is encoded
/// once, so the three backends cannot drift on what `null` or a large integer
/// binds as.
///
/// Note what this does **not** change. A JSON string still goes out as `text`,
/// so on PostgreSQL — whose parameters are typed, and which has no `text =
/// uuid` operator — a comparison against a `uuid`, `numeric` or `timestamptz`
/// column still needs `WHERE id = ($1)::uuid` in the query. That is a property
/// of PostgreSQL, not of the driver layer, and the alternative would be to
/// guess a parameter's SQL type from the shape of its JSON value: a string
/// that happens to look like a UUID would then bind as one and fail against a
/// `text` column. Guessing from the value is the data-dependent behaviour
/// #309 was filed about; it is not worth reintroducing on the other side.
pub(crate) fn bind_params<'q, DB>(
    mut query: sqlx::query::Query<'q, DB, <DB as sqlx::Database>::Arguments<'q>>,
    params: &'q [serde_json::Value],
) -> sqlx::query::Query<'q, DB, <DB as sqlx::Database>::Arguments<'q>>
where
    DB: sqlx::Database,
    &'q str: sqlx::Encode<'q, DB> + sqlx::Type<DB>,
    i64: sqlx::Encode<'q, DB> + sqlx::Type<DB>,
    f64: sqlx::Encode<'q, DB> + sqlx::Type<DB>,
    bool: sqlx::Encode<'q, DB> + sqlx::Type<DB>,
    String: sqlx::Encode<'q, DB> + sqlx::Type<DB>,
    Option<String>: sqlx::Encode<'q, DB> + sqlx::Type<DB>,
{
    for param in params {
        query = match param {
            serde_json::Value::String(s) => query.bind(s.as_str()),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    query.bind(i)
                } else if let Some(f) = n.as_f64() {
                    query.bind(f)
                } else {
                    // Unreachable for a `serde_json::Number`: `as_f64` answers
                    // `Some` for every one of them, including a `u64` above
                    // `i64::MAX` — which it rounds. Preserving those digits is
                    // what `sql_encode::Scalar::from` does, by asking `as_u64`
                    // before `as_f64`; this arm predates it and is kept only so
                    // the match stays total.
                    query.bind(n.to_string())
                }
            }
            serde_json::Value::Bool(b) => query.bind(*b),
            // Typed, so the driver sends a real NULL rather than the four
            // characters "null".
            serde_json::Value::Null => query.bind(None::<String>),
            // An object or array has no scalar column type. Sent as its JSON
            // text, which is what a `jsonb` column and a `text` column both
            // accept.
            other => query.bind(other.to_string()),
        };
    }
    query
}
