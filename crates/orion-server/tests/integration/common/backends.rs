//! Shared testcontainers harness for exercising the portable `data_query` /
//! `data_write` dialects against every backend they support.
//!
//! The same round-trip scenario (see `data_roundtrip_test.rs`) is written once
//! and run against each backend — only the connector / connection string
//! differs. SQLite runs in-memory (no container); Postgres and MySQL spin up an
//! ephemeral testcontainer with a random host port, torn down when the
//! [`BackendHarness`] is dropped (RAII, mirroring `kafka_test.rs`).
//!
//! The dialect-specific quirks the harness encodes:
//! - DDL: the portable dialect has no DDL, so each backend needs a raw
//!   `db_write` `CREATE TABLE` first, in its own column types.
//! - `RETURNING`: supported on SQLite/Postgres, not MySQL.

#![allow(dead_code)]

use serde_json::{Value, json};
use testcontainers::ContainerAsync;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::mysql::Mysql;
use testcontainers_modules::postgres::Postgres;

/// A backend that `data_query` / `data_write` can target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Sqlite,
    Postgres,
    Mysql,
}

impl Backend {
    /// Short stable name for this backend, used to build unique per-backend
    /// connector and channel names. (It used to be written into the connector
    /// config as `driver`, a field removed in 1.0 — the backend comes from the
    /// connection-string scheme.)
    pub fn label(&self) -> &'static str {
        match self {
            Backend::Sqlite => "sqlite",
            Backend::Postgres => "postgres",
            Backend::Mysql => "mysql",
        }
    }

    /// Whether the SQL renderer emits `RETURNING` for this backend.
    pub fn supports_returning(&self) -> bool {
        matches!(self, Backend::Sqlite | Backend::Postgres)
    }
}

/// Holds the live container so it is dropped (and removed) at the end of the
/// test. SQLite has no container.
enum RunningContainer {
    None,
    Postgres(ContainerAsync<Postgres>),
    Mysql(ContainerAsync<Mysql>),
}

/// A running backend plus everything needed to register a connector against it.
pub struct BackendHarness {
    pub backend: Backend,
    pub connector_name: String,
    pub connection_string: String,
    _container: RunningContainer,
}

impl BackendHarness {
    /// Admin `POST /api/v2/admin/connectors` body pointing at this backend.
    /// Mirrors the shape of `db_connector_sqlite` in `common/mod.rs`.
    pub fn connector_json(&self) -> Value {
        json!({
            "id": self.connector_name,
            "name": self.connector_name,
            "connector_type": "db",
            "config": {
                "type": "db",
                "connection_string": self.connection_string,
                "max_connections": 2,
                "query_timeout_ms": 10000,
                // Testcontainers publish on localhost, which the S6 endpoint
                // check would otherwise refuse. Harmless for the sqlite
                // backend, whose connection string has no host at all.
                "allow_private_urls": true
            }
        })
    }

    /// Dialect-specific `CREATE TABLE users(...)`.
    pub fn ddl_users(&self) -> String {
        let sql = match self.backend {
            Backend::Mysql => {
                "CREATE TABLE IF NOT EXISTS users (id VARCHAR(64) PRIMARY KEY, name VARCHAR(255), nickname VARCHAR(255), age INT, status VARCHAR(64))"
            }
            Backend::Sqlite | Backend::Postgres => {
                "CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY, name TEXT, nickname TEXT, age INTEGER, status TEXT)"
            }
        };
        sql.to_string()
    }

    /// `orders` and the `tags` / `user_tags` junction the relation rows of the
    /// parity matrix need.
    pub fn ddl_relations(&self) -> Vec<String> {
        let (orders, tags, user_tags) = match self.backend {
            Backend::Mysql => (
                "CREATE TABLE IF NOT EXISTS orders (id VARCHAR(64) PRIMARY KEY, user_id VARCHAR(64), total INT)",
                "CREATE TABLE IF NOT EXISTS tags (id VARCHAR(64) PRIMARY KEY, label VARCHAR(64))",
                "CREATE TABLE IF NOT EXISTS user_tags (user_id VARCHAR(64), tag_id VARCHAR(64))",
            ),
            Backend::Sqlite | Backend::Postgres => (
                "CREATE TABLE IF NOT EXISTS orders (id TEXT PRIMARY KEY, user_id TEXT, total INTEGER)",
                "CREATE TABLE IF NOT EXISTS tags (id TEXT PRIMARY KEY, label TEXT)",
                "CREATE TABLE IF NOT EXISTS user_tags (user_id TEXT, tag_id TEXT)",
            ),
        };
        vec![orders.to_string(), tags.to_string(), user_tags.to_string()]
    }

    /// Dialect-specific auto-increment `items` table (for the RETURNING /
    /// `last_insert_id` check).
    pub fn ddl_items(&self) -> String {
        let sql = match self.backend {
            Backend::Sqlite => {
                "CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT)"
            }
            Backend::Postgres => {
                "CREATE TABLE IF NOT EXISTS items (id SERIAL PRIMARY KEY, name TEXT)"
            }
            Backend::Mysql => {
                "CREATE TABLE IF NOT EXISTS items (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(255))"
            }
        };
        sql.to_string()
    }

    // -- write-result shape assertions -----------------------------------------

    pub fn assert_insert_count(&self, w: &Value, expected: u64) {
        assert_eq!(
            w["rows_affected"].as_u64(),
            Some(expected),
            "sql rows_affected: {w}"
        );
    }

    pub fn assert_update_count(&self, w: &Value, expected: u64) {
        assert_eq!(
            w["rows_affected"].as_u64(),
            Some(expected),
            "sql rows_affected: {w}"
        );
    }

    pub fn assert_delete_count(&self, w: &Value, expected: u64) {
        assert_eq!(
            w["rows_affected"].as_u64(),
            Some(expected),
            "sql rows_affected: {w}"
        );
    }
}

/// Start a backend. SQLite uses a shared named in-memory DB (no container); the
/// others spin up an ephemeral testcontainer and connect over its mapped port.
///
/// `name` is used as both the connector name and (for SQLite) the in-memory DB
/// name, so pass a value unique within the test process.
pub async fn start(backend: Backend, name: &str) -> BackendHarness {
    match backend {
        Backend::Sqlite => BackendHarness {
            backend,
            connector_name: name.to_string(),
            connection_string: format!("sqlite:file:{name}?mode=memory&cache=shared"),
            _container: RunningContainer::None,
        },
        Backend::Postgres => {
            let container = Postgres::default().start().await.unwrap();
            let port = container.get_host_port_ipv4(5432).await.unwrap();
            BackendHarness {
                backend,
                connector_name: name.to_string(),
                connection_string: format!(
                    "postgres://postgres:postgres@127.0.0.1:{port}/postgres"
                ),
                _container: RunningContainer::Postgres(container),
            }
        }
        Backend::Mysql => {
            let container = Mysql::default().start().await.unwrap();
            let port = container.get_host_port_ipv4(3306).await.unwrap();
            BackendHarness {
                backend,
                connector_name: name.to_string(),
                connection_string: format!("mysql://root@127.0.0.1:{port}/test"),
                _container: RunningContainer::Mysql(container),
            }
        }
    }
}
