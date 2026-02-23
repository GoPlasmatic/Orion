pub mod models;
pub mod repositories;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;

use crate::errors::OrionError;

/// Initialize SQLite connection pool with WAL mode and run embedded migrations.
pub async fn init_pool(db_path: &str) -> Result<SqlitePool, OrionError> {
    let options = SqliteConnectOptions::from_str(&format!("sqlite:{}", db_path))
        .map_err(|e| OrionError::Internal(format!("Invalid DB path: {}", e)))?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .map_err(|e| OrionError::Internal(format!("Failed to connect to database: {}", e)))?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .map_err(|e| OrionError::Internal(format!("Failed to run migrations: {}", e)))?;

    Ok(pool)
}
