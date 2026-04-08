pub mod migration_gen;
pub mod models;
pub mod repositories;
pub mod schema;

use std::sync::OnceLock;
use std::time::Duration;

use sea_query_binder::SqlxBinder;

use crate::config::StorageConfig;
use crate::errors::OrionError;

// ============================================================
// Database backend detection — determined at runtime from URL
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbBackend {
    Sqlite,
    Postgres,
    Mysql,
}

impl std::fmt::Display for DbBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite => write!(f, "sqlite"),
            Self::Postgres => write!(f, "postgres"),
            Self::Mysql => write!(f, "mysql"),
        }
    }
}

static DB_BACKEND: OnceLock<DbBackend> = OnceLock::new();

/// Detect the database backend from a connection URL.
pub fn detect_backend(url: &str) -> Result<DbBackend, OrionError> {
    if url.starts_with("sqlite:") || url.starts_with("file:") {
        Ok(DbBackend::Sqlite)
    } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        Ok(DbBackend::Postgres)
    } else if url.starts_with("mysql://") || url.starts_with("mariadb://") {
        Ok(DbBackend::Mysql)
    } else {
        Err(OrionError::Config {
            message: format!(
                "Unsupported database URL scheme: {url}. Expected sqlite:, postgres://, or mysql://"
            ),
        })
    }
}

/// Get the active database backend. Panics if not yet initialized.
pub fn get_backend() -> DbBackend {
    *DB_BACKEND
        .get()
        .expect("Database backend not initialized. Call init_pool() first.")
}

/// Set the database backend for unit tests that call `build_sqlx` without a pool.
#[cfg(test)]
pub fn set_backend_for_test(backend: DbBackend) {
    DB_BACKEND.set(backend).ok();
}

// ============================================================
// DbPool — enum wrapping concrete pool types
// ============================================================

/// Database connection pool that wraps the concrete sqlx pool type,
/// selected at runtime based on the connection URL.
#[derive(Clone)]
pub enum DbPool {
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
    Mysql(sqlx::MySqlPool),
}

/// Dispatch a query expression across all pool variants.
///
/// Each arm binds the inner pool to `$p` and evaluates `$body`.
/// The expression is type-checked independently per arm, so the pool type
/// is correctly inferred by sqlx in each case.
macro_rules! dispatch_pool {
    ($self:expr, $p:ident => $body:expr) => {
        match $self {
            DbPool::Sqlite($p) => $body,
            DbPool::Postgres($p) => $body,
            DbPool::Mysql($p) => $body,
        }
    };
}

impl DbPool {
    pub fn size(&self) -> u32 {
        dispatch_pool!(self, p => p.size())
    }

    pub fn num_idle(&self) -> usize {
        dispatch_pool!(self, p => p.num_idle())
    }

    pub async fn fetch_all_as<T>(
        &self,
        sql: &str,
        values: sea_query_binder::SqlxValues,
    ) -> Result<Vec<T>, sqlx::Error>
    where
        T: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> + Send + Unpin,
        T: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>,
        T: for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow>,
    {
        dispatch_pool!(self, p => sqlx::query_as_with::<_, T, _>(sql, values).fetch_all(p).await)
    }

    pub async fn fetch_one_as<T>(
        &self,
        sql: &str,
        values: sea_query_binder::SqlxValues,
    ) -> Result<T, sqlx::Error>
    where
        T: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> + Send + Unpin,
        T: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>,
        T: for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow>,
    {
        dispatch_pool!(self, p => sqlx::query_as_with::<_, T, _>(sql, values).fetch_one(p).await)
    }

    pub async fn fetch_optional_as<T>(
        &self,
        sql: &str,
        values: sea_query_binder::SqlxValues,
    ) -> Result<Option<T>, sqlx::Error>
    where
        T: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> + Send + Unpin,
        T: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>,
        T: for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow>,
    {
        dispatch_pool!(self, p => sqlx::query_as_with::<_, T, _>(sql, values).fetch_optional(p).await)
    }

    pub async fn execute_query(
        &self,
        sql: &str,
        values: sea_query_binder::SqlxValues,
    ) -> Result<u64, sqlx::Error> {
        dispatch_pool!(self, p => {
            let r = sqlx::query_with(sql, values).execute(p).await?;
            Ok(r.rows_affected())
        })
    }

    pub async fn fetch_scalar<T>(
        &self,
        sql: &str,
        values: sea_query_binder::SqlxValues,
    ) -> Result<T, sqlx::Error>
    where
        T: Send + Unpin + 'static,
        T: sqlx::Type<sqlx::Sqlite> + for<'r> sqlx::Decode<'r, sqlx::Sqlite>,
        T: sqlx::Type<sqlx::Postgres> + for<'r> sqlx::Decode<'r, sqlx::Postgres>,
        T: sqlx::Type<sqlx::MySql> + for<'r> sqlx::Decode<'r, sqlx::MySql>,
        (T,): for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow>,
        (T,): for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>,
        (T,): for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow>,
    {
        dispatch_pool!(self, p => sqlx::query_scalar_with::<_, T, _>(sql, values).fetch_one(p).await)
    }

    pub async fn begin_tx(&self) -> Result<DbTransaction, sqlx::Error> {
        match self {
            DbPool::Sqlite(p) => Ok(DbTransaction::Sqlite(p.begin().await?)),
            DbPool::Postgres(p) => Ok(DbTransaction::Postgres(p.begin().await?)),
            DbPool::Mysql(p) => Ok(DbTransaction::Mysql(p.begin().await?)),
        }
    }
}

// ============================================================
// DbTransaction — enum wrapping concrete transaction types
// ============================================================

pub enum DbTransaction {
    Sqlite(sqlx::Transaction<'static, sqlx::Sqlite>),
    Postgres(sqlx::Transaction<'static, sqlx::Postgres>),
    Mysql(sqlx::Transaction<'static, sqlx::MySql>),
}

macro_rules! dispatch_tx {
    ($self:expr, $tx:ident => $body:expr) => {
        match $self {
            DbTransaction::Sqlite($tx) => $body,
            DbTransaction::Postgres($tx) => $body,
            DbTransaction::Mysql($tx) => $body,
        }
    };
}

impl DbTransaction {
    pub async fn commit(self) -> Result<(), sqlx::Error> {
        match self {
            DbTransaction::Sqlite(tx) => tx.commit().await,
            DbTransaction::Postgres(tx) => tx.commit().await,
            DbTransaction::Mysql(tx) => tx.commit().await,
        }
    }

    pub async fn fetch_all_as<T>(
        &mut self,
        sql: &str,
        values: sea_query_binder::SqlxValues,
    ) -> Result<Vec<T>, sqlx::Error>
    where
        T: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> + Send + Unpin,
        T: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>,
        T: for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow>,
    {
        dispatch_tx!(self, tx => sqlx::query_as_with::<_, T, _>(sql, values).fetch_all(&mut **tx).await)
    }

    pub async fn fetch_optional_as<T>(
        &mut self,
        sql: &str,
        values: sea_query_binder::SqlxValues,
    ) -> Result<Option<T>, sqlx::Error>
    where
        T: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> + Send + Unpin,
        T: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>,
        T: for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow>,
    {
        dispatch_tx!(self, tx => sqlx::query_as_with::<_, T, _>(sql, values).fetch_optional(&mut **tx).await)
    }

    pub async fn execute_query(
        &mut self,
        sql: &str,
        values: sea_query_binder::SqlxValues,
    ) -> Result<u64, sqlx::Error> {
        dispatch_tx!(self, tx => {
            let r = sqlx::query_with(sql, values).execute(&mut **tx).await?;
            Ok(r.rows_affected())
        })
    }
}

// ============================================================
// Query builder helper — builds SQL + bound values
// ============================================================

/// Build a SQL string and bound values using the runtime-detected backend.
pub fn build_sqlx<S: SqlxBinder>(stmt: &mut S) -> (String, sea_query_binder::SqlxValues) {
    match get_backend() {
        DbBackend::Sqlite => stmt.build_sqlx(sea_query::SqliteQueryBuilder),
        DbBackend::Postgres => stmt.build_sqlx(sea_query::PostgresQueryBuilder),
        DbBackend::Mysql => stmt.build_sqlx(sea_query::MysqlQueryBuilder),
    }
}

// ============================================================
// Embedded migrations — all backends compiled in, selected at runtime
// ============================================================

static MIGRATOR_SQLITE: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/sqlite");
static MIGRATOR_POSTGRES: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/postgres");
static MIGRATOR_MYSQL: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/mysql");

fn migrator() -> &'static sqlx::migrate::Migrator {
    match get_backend() {
        DbBackend::Sqlite => &MIGRATOR_SQLITE,
        DbBackend::Postgres => &MIGRATOR_POSTGRES,
        DbBackend::Mysql => &MIGRATOR_MYSQL,
    }
}

// ============================================================
// Pool initialization
// ============================================================

/// Initialize the database connection pool and run migrations.
pub async fn init_pool(config: &StorageConfig) -> Result<DbPool, OrionError> {
    let pool = init_pool_no_migrate(config).await?;
    run_migrations(&pool).await?;
    Ok(pool)
}

/// Initialize the database connection pool without running migrations.
pub async fn init_pool_no_migrate(config: &StorageConfig) -> Result<DbPool, OrionError> {
    let backend = detect_backend(&config.url)?;
    DB_BACKEND.set(backend).ok(); // Ignore if already set (e.g. tests)

    match backend {
        DbBackend::Sqlite => init_sqlite_pool(config).await,
        DbBackend::Postgres => init_postgres_pool(config).await,
        DbBackend::Mysql => init_mysql_pool(config).await,
    }
}

/// Run pending database migrations.
pub async fn run_migrations(pool: &DbPool) -> Result<(), OrionError> {
    let m = migrator();
    match pool {
        DbPool::Sqlite(p) => m.run(p).await,
        DbPool::Postgres(p) => m.run(p).await,
        DbPool::Mysql(p) => m.run(p).await,
    }
    .map_err(|e| OrionError::InternalSource {
        context: "Failed to run migrations".to_string(),
        source: Box::new(e),
    })
}

/// List pending migrations that have not yet been applied.
pub async fn pending_migrations(pool: &DbPool) -> Result<Vec<(i64, String)>, OrionError> {
    let applied: std::collections::HashSet<i64> = {
        let sql = "SELECT version FROM _sqlx_migrations ORDER BY version";
        let result: Result<Vec<i64>, _> = match pool {
            DbPool::Sqlite(p) => sqlx::query_scalar::<_, i64>(sql).fetch_all(p).await,
            DbPool::Postgres(p) => sqlx::query_scalar::<_, i64>(sql).fetch_all(p).await,
            DbPool::Mysql(p) => sqlx::query_scalar::<_, i64>(sql).fetch_all(p).await,
        };
        match result {
            Ok(versions) => versions.into_iter().collect(),
            Err(_) => std::collections::HashSet::new(),
        }
    };

    let pending: Vec<(i64, String)> = migrator()
        .iter()
        .filter(|m| !applied.contains(&m.version))
        .map(|m| (m.version, m.description.to_string()))
        .collect();

    Ok(pending)
}

// ============================================================
// Backend-specific pool initialization
// ============================================================

async fn init_sqlite_pool(config: &StorageConfig) -> Result<DbPool, OrionError> {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    let busy_timeout = config.busy_timeout_ms.to_string();
    let options = SqliteConnectOptions::from_str(&config.url)
        .map_err(|e| OrionError::InternalSource {
            context: "Invalid DB path".to_string(),
            source: Box::new(e),
        })?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .pragma("foreign_keys", "ON")
        .pragma("busy_timeout", busy_timeout)
        .pragma("synchronous", "NORMAL")
        .pragma("cache_size", "-20000");

    let mut pool_opts = SqlitePoolOptions::new()
        .max_connections(config.max_connections)
        .min_connections(config.min_connections)
        .acquire_timeout(Duration::from_secs(config.acquire_timeout_secs));
    if config.idle_timeout_secs > 0 {
        pool_opts = pool_opts.idle_timeout(Duration::from_secs(config.idle_timeout_secs));
    }
    let pool = pool_opts
        .connect_with(options)
        .await
        .map_err(|e| OrionError::InternalSource {
            context: "Failed to connect to database".to_string(),
            source: Box::new(e),
        })?;
    Ok(DbPool::Sqlite(pool))
}

async fn init_postgres_pool(config: &StorageConfig) -> Result<DbPool, OrionError> {
    use sqlx::postgres::PgPoolOptions;

    let mut pool_opts = PgPoolOptions::new()
        .max_connections(config.max_connections)
        .min_connections(config.min_connections)
        .acquire_timeout(Duration::from_secs(config.acquire_timeout_secs));
    if config.idle_timeout_secs > 0 {
        pool_opts = pool_opts.idle_timeout(Duration::from_secs(config.idle_timeout_secs));
    }
    let pool = pool_opts
        .connect(&config.url)
        .await
        .map_err(|e| OrionError::InternalSource {
            context: "Failed to connect to database".to_string(),
            source: Box::new(e),
        })?;
    Ok(DbPool::Postgres(pool))
}

async fn init_mysql_pool(config: &StorageConfig) -> Result<DbPool, OrionError> {
    use sqlx::mysql::MySqlPoolOptions;

    let mut pool_opts = MySqlPoolOptions::new()
        .max_connections(config.max_connections)
        .min_connections(config.min_connections)
        .acquire_timeout(Duration::from_secs(config.acquire_timeout_secs));
    if config.idle_timeout_secs > 0 {
        pool_opts = pool_opts.idle_timeout(Duration::from_secs(config.idle_timeout_secs));
    }
    let pool = pool_opts
        .connect(&config.url)
        .await
        .map_err(|e| OrionError::InternalSource {
            context: "Failed to connect to database".to_string(),
            source: Box::new(e),
        })?;
    Ok(DbPool::Mysql(pool))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_backend_sqlite() {
        assert_eq!(
            detect_backend("sqlite:orion.db").unwrap(),
            DbBackend::Sqlite
        );
        assert_eq!(
            detect_backend("sqlite::memory:").unwrap(),
            DbBackend::Sqlite
        );
    }

    #[test]
    fn test_detect_backend_postgres() {
        assert_eq!(
            detect_backend("postgres://user:pass@localhost/db").unwrap(),
            DbBackend::Postgres
        );
        assert_eq!(
            detect_backend("postgresql://user:pass@localhost/db").unwrap(),
            DbBackend::Postgres
        );
    }

    #[test]
    fn test_detect_backend_mysql() {
        assert_eq!(
            detect_backend("mysql://user:pass@localhost/db").unwrap(),
            DbBackend::Mysql
        );
    }

    #[test]
    fn test_detect_backend_unsupported() {
        assert!(detect_backend("mssql://localhost").is_err());
    }
}
