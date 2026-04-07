pub mod migration_gen;
pub mod models;
pub mod repositories;
pub mod schema;

use std::time::Duration;

use crate::config::StorageConfig;
use crate::errors::OrionError;

// ============================================================
// Pool type alias — switches based on compile-time feature flag
// ============================================================

#[cfg(feature = "db-sqlite")]
pub type DbPool = sqlx::SqlitePool;
#[cfg(feature = "db-postgres")]
pub type DbPool = sqlx::PgPool;
#[cfg(feature = "db-mysql")]
pub type DbPool = sqlx::MySqlPool;

// ============================================================
// Query builder selector — returns the correct sea-query backend
// ============================================================

#[cfg(feature = "db-sqlite")]
pub fn query_builder() -> sea_query::SqliteQueryBuilder {
    sea_query::SqliteQueryBuilder
}

#[cfg(feature = "db-postgres")]
pub fn query_builder() -> sea_query::PostgresQueryBuilder {
    sea_query::PostgresQueryBuilder
}

#[cfg(feature = "db-mysql")]
pub fn query_builder() -> sea_query::MysqlQueryBuilder {
    sea_query::MysqlQueryBuilder
}

// ============================================================
// Embedded migrations — selected per backend at compile time
// ============================================================

#[cfg(feature = "db-sqlite")]
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/sqlite");
#[cfg(feature = "db-postgres")]
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/postgres");
#[cfg(feature = "db-mysql")]
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/mysql");

// ============================================================
// Pool initialization — backend-specific connection setup
// ============================================================

/// Initialize the database connection pool and run migrations.
pub async fn init_pool(config: &StorageConfig) -> Result<DbPool, OrionError> {
    #[cfg(feature = "db-sqlite")]
    {
        init_sqlite_pool(config).await
    }
    #[cfg(feature = "db-postgres")]
    {
        init_postgres_pool(config).await
    }
    #[cfg(feature = "db-mysql")]
    {
        init_mysql_pool(config).await
    }
}

#[cfg(feature = "db-sqlite")]
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

    let pool = SqlitePoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(Duration::from_secs(config.acquire_timeout_secs))
        .connect_with(options)
        .await
        .map_err(|e| OrionError::InternalSource {
            context: "Failed to connect to database".to_string(),
            source: Box::new(e),
        })?;

    MIGRATOR
        .run(&pool)
        .await
        .map_err(|e| OrionError::InternalSource {
            context: "Failed to run migrations".to_string(),
            source: Box::new(e),
        })?;

    Ok(pool)
}

#[cfg(feature = "db-postgres")]
async fn init_postgres_pool(config: &StorageConfig) -> Result<DbPool, OrionError> {
    use sqlx::postgres::PgPoolOptions;

    let pool = PgPoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(Duration::from_secs(config.acquire_timeout_secs))
        .connect(&config.url)
        .await
        .map_err(|e| OrionError::InternalSource {
            context: "Failed to connect to database".to_string(),
            source: Box::new(e),
        })?;

    MIGRATOR
        .run(&pool)
        .await
        .map_err(|e| OrionError::InternalSource {
            context: "Failed to run migrations".to_string(),
            source: Box::new(e),
        })?;

    Ok(pool)
}

#[cfg(feature = "db-mysql")]
async fn init_mysql_pool(config: &StorageConfig) -> Result<DbPool, OrionError> {
    use sqlx::mysql::MySqlPoolOptions;

    let pool = MySqlPoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(Duration::from_secs(config.acquire_timeout_secs))
        .connect(&config.url)
        .await
        .map_err(|e| OrionError::InternalSource {
            context: "Failed to connect to database".to_string(),
            source: Box::new(e),
        })?;

    MIGRATOR
        .run(&pool)
        .await
        .map_err(|e| OrionError::InternalSource {
            context: "Failed to run migrations".to_string(),
            source: Box::new(e),
        })?;

    Ok(pool)
}
