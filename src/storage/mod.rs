pub mod models;
pub mod repositories;

use std::str::FromStr;
use std::time::Duration;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use crate::config::StorageConfig;
use crate::errors::OrionError;

/// Initialize SQLite connection pool with WAL mode and run embedded migrations.
pub async fn init_pool(config: &StorageConfig) -> Result<SqlitePool, OrionError> {
    let busy_timeout = config.busy_timeout_ms.to_string();
    let options = SqliteConnectOptions::from_str(&format!("sqlite:{}", config.path))
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

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .map_err(|e| OrionError::InternalSource {
            context: "Failed to run migrations".to_string(),
            source: Box::new(e),
        })?;

    Ok(pool)
}
