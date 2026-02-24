pub mod models;
pub mod repositories;

use std::str::FromStr;
use std::time::Duration;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use crate::errors::OrionError;

/// Initialize SQLite connection pool with WAL mode and run embedded migrations.
pub async fn init_pool(db_path: &str, max_connections: u32) -> Result<SqlitePool, OrionError> {
    let options = SqliteConnectOptions::from_str(&format!("sqlite:{}", db_path))
        .map_err(|e| OrionError::Internal(format!("Invalid DB path: {}", e)))?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .pragma("foreign_keys", "ON")
        .pragma("busy_timeout", "5000")
        .pragma("synchronous", "NORMAL")
        .pragma("cache_size", "-20000");

    let pool = SqlitePoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options)
        .await
        .map_err(|e| OrionError::Internal(format!("Failed to connect to database: {}", e)))?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .map_err(|e| OrionError::Internal(format!("Failed to run migrations: {}", e)))?;

    Ok(pool)
}
