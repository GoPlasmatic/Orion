//! Shared testcontainers harness for exercising the portable `data_query` /
//! `data_write` dialects against every backend they support.
//!
//! The same round-trip scenario (see `data_roundtrip_test.rs`) is written once
//! and run against each backend — only the connector / connection string
//! differs. SQLite runs in-memory (no container); Postgres and MySQL spin up an
//! ephemeral testcontainer with a random host port, torn down when the
//! [`BackendHarness`] is dropped (RAII).
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
    /// connector and channel names.
    pub fn label(&self) -> &'static str {
        match self {
            Backend::Sqlite => "sqlite",
            Backend::Postgres => "postgres",
            Backend::Mysql => "mysql",
        }
    }

    /// The `driver` a db connector in a definition set names.
    pub fn driver(&self) -> &'static str {
        match self {
            Backend::Sqlite => "sqlite",
            Backend::Postgres => "postgresql",
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
    /// What the connector config carries in place of `connection_string`:
    /// the string itself for SQLite, an `env://` reference for a container,
    /// whose URL carries credentials a connection's public config refuses.
    pub connection_reference: String,
    _container: RunningContainer,
}

/// Publish `url` as an environment variable unique to this container and
/// answer the `env://` reference that reads it.
///
/// The fixed fixtures elsewhere in this binary are set pre-main, the one point
/// where `set_var` cannot race a reader. A container's port is only known once
/// it runs, so this one is set at runtime. std serialises its own environment
/// reads and writes, and `env://` resolution reads through std; the residual
/// risk is a libc `getenv` on another thread, which only the container-gated
/// modules that call this take on.
fn env_reference(name: &str, port: u16, url: &str) -> String {
    let var = format!(
        "ORION_SECRET_TEST_DB_{}_{port}",
        name.to_uppercase().replace(['-', '.'], "_")
    );
    // SAFETY: see above; the name is unique to this container, so no reader
    // can observe this variable changing.
    #[expect(unsafe_code)]
    unsafe {
        std::env::set_var(&var, url)
    };
    format!("env://{var}")
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
                "driver": self.backend.driver(),
                "connection_string": self.connection_reference,
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
            connection_reference: format!("sqlite:file:{name}?mode=memory&cache=shared"),
            _container: RunningContainer::None,
        },
        Backend::Postgres => {
            let container = Postgres::default().start().await.unwrap();
            let port = container.get_host_port_ipv4(5432).await.unwrap();
            let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
            BackendHarness {
                backend,
                connector_name: name.to_string(),
                connection_reference: env_reference(name, port, &url),
                connection_string: url,
                _container: RunningContainer::Postgres(container),
            }
        }
        Backend::Mysql => {
            let container = Mysql::default().start().await.unwrap();
            let port = container.get_host_port_ipv4(3306).await.unwrap();
            let url = format!("mysql://root@127.0.0.1:{port}/test");
            BackendHarness {
                backend,
                connector_name: name.to_string(),
                connection_reference: env_reference(name, port, &url),
                connection_string: url,
                _container: RunningContainer::Mysql(container),
            }
        }
    }
}
