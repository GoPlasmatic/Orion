//! Shared testcontainers harness for exercising the portable `data_query` /
//! `data_write` dialects against every backend they support.
//!
//! The same round-trip scenario (see `data_roundtrip_test.rs`) is written once
//! and run against each backend — only the connector / connection string
//! differs. SQLite runs in-memory (no container); Postgres, MySQL, MongoDB, and
//! Elasticsearch spin up an ephemeral testcontainer with a random host port,
//! torn down when the [`BackendHarness`] is dropped (RAII, mirroring
//! `kafka_test.rs`).
//!
//! The dialect-specific quirks the harness encodes:
//! - DDL: the portable dialect has no DDL, so SQL backends need a raw `db_write`
//!   `CREATE TABLE` first; MongoDB is schemaless; Elasticsearch gets an explicit
//!   index mapping via [`BackendHarness::prepare`] (dynamic mapping would make
//!   `term` filters and sorts on text fields unreliable).
//! - `database`: the Mongo path of both handlers requires a `database` field.
//! - `_id`: Elasticsearch keys documents on metadata `_id`, so the envelopes
//!   carry an inline schema renaming the logical `id` column
//!   ([`BackendHarness::schema_json`]); Mongo maps `id` → `_id` implicitly.
//! - Write-result shape: SQL returns `rows_affected`; the doc stores (Mongo,
//!   ES) return `inserted` / `modified` / `deleted`.
//! - `RETURNING`: supported on SQLite/Postgres, not MySQL/Mongo/ES.

#![allow(dead_code)]

use std::time::Duration;

use serde_json::{Value, json};
use testcontainers::ContainerAsync;
use testcontainers::core::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::elastic_search::ElasticSearch;
use testcontainers_modules::mongo::Mongo;
use testcontainers_modules::mysql::Mysql;
use testcontainers_modules::postgres::Postgres;

/// A backend that `data_query` / `data_write` can target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Sqlite,
    Postgres,
    Mysql,
    Mongo,
    Es,
}

impl Backend {
    /// The `driver` field written into the connector config (label only for ES,
    /// whose connector carries a URL instead).
    pub fn driver(&self) -> &'static str {
        match self {
            Backend::Sqlite => "sqlite",
            Backend::Postgres => "postgres",
            Backend::Mysql => "mysql",
            Backend::Mongo => "mongodb",
            Backend::Es => "es",
        }
    }

    pub fn is_mongo(&self) -> bool {
        matches!(self, Backend::Mongo)
    }

    pub fn is_es(&self) -> bool {
        matches!(self, Backend::Es)
    }

    /// Document stores normalise write results to the same keys
    /// (`inserted` / `modified` / `deleted`) instead of SQL's `rows_affected`.
    pub fn is_doc_store(&self) -> bool {
        matches!(self, Backend::Mongo | Backend::Es)
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
    Mongo(ContainerAsync<Mongo>),
    Es(ContainerAsync<ElasticSearch>),
}

/// A running backend plus everything needed to register a connector against it.
pub struct BackendHarness {
    pub backend: Backend,
    pub connector_name: String,
    pub connection_string: String,
    /// `Some` only for Mongo — the Mongo handlers require a `database` field.
    pub database: Option<String>,
    _container: RunningContainer,
}

impl BackendHarness {
    /// Admin `POST /api/v1/admin/connectors` body pointing at this backend.
    /// Mirrors the shape of `db_connector_sqlite` in `common/mod.rs` (SQL/Mongo)
    /// and the `es` connector in `es_test.rs` (Elasticsearch).
    pub fn connector_json(&self) -> Value {
        if self.backend.is_es() {
            return json!({
                "id": self.connector_name,
                "name": self.connector_name,
                "connector_type": "es",
                "config": {
                    "type": "es",
                    "url": self.connection_string,
                    "request_timeout_ms": 10000,
                    // Testcontainers publish on localhost, which SSRF
                    // validation would otherwise block.
                    "allow_private_urls": true
                }
            });
        }
        json!({
            "id": self.connector_name,
            "name": self.connector_name,
            "connector_type": "db",
            "config": {
                "type": "db",
                "connection_string": self.connection_string,
                "driver": self.backend.driver(),
                "max_connections": 2,
                "query_timeout_ms": 10000
            }
        })
    }

    /// Dialect-specific `CREATE TABLE users(...)`; `None` for the schemaless doc
    /// stores (Mongo; ES gets its mapping from [`prepare`](Self::prepare)).
    pub fn ddl_users(&self) -> Option<String> {
        let sql = match self.backend {
            Backend::Mysql => {
                "CREATE TABLE IF NOT EXISTS users (id VARCHAR(64) PRIMARY KEY, name VARCHAR(255), age INT, status VARCHAR(64))"
            }
            Backend::Sqlite | Backend::Postgres => {
                "CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY, name TEXT, age INTEGER, status TEXT)"
            }
            Backend::Mongo | Backend::Es => return None,
        };
        Some(sql.to_string())
    }

    /// Dialect-specific auto-increment `items` table (for the RETURNING /
    /// `last_insert_id` check); `None` for the doc stores.
    pub fn ddl_items(&self) -> Option<String> {
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
            Backend::Mongo | Backend::Es => return None,
        };
        Some(sql.to_string())
    }

    /// Backend-specific setup before the pipeline runs. Elasticsearch: create the
    /// `users` index with an explicit mapping — dynamic mapping would type the
    /// string fields as `text`, making `term` filters and sorts unreliable.
    /// (`items` relies on index auto-creation; its step only checks counts/ids.)
    /// A no-op elsewhere.
    pub async fn prepare(&self) {
        if self.backend.is_es() {
            reqwest::Client::new()
                .put(format!("{}/users", self.connection_string))
                .json(&json!({ "mappings": { "properties": {
                    "name":   { "type": "keyword" },
                    "age":    { "type": "integer" },
                    "status": { "type": "keyword" }
                } } }))
                .send()
                .await
                .expect("create users index")
                .error_for_status()
                .expect("users mapping accepted");
        }
    }

    /// Inline schema the envelopes need, if any. Elasticsearch keys documents on
    /// the metadata `_id`, so the logical `id` column renames to `_id` — that
    /// makes inserts carry the id in the bulk action and upsert-on-`id` legal.
    pub fn schema_json(&self) -> Option<Value> {
        self.backend.is_es().then(|| {
            json!({ "entities": { "users": {
                "columns": { "id": { "name": "_id" } }
            } } })
        })
    }

    /// Inject the `database` field into a `data_write` / `data_query` input when
    /// the backend requires it (Mongo). A no-op for SQL backends and ES.
    pub fn with_db(&self, mut input: Value) -> Value {
        if let Some(db) = &self.database {
            input["database"] = json!(db);
        }
        input
    }

    // -- write-result shape assertions (SQL vs doc stores) -------------------

    pub fn assert_insert_count(&self, w: &Value, expected: u64) {
        if self.backend.is_doc_store() {
            assert_eq!(
                w["inserted"].as_u64(),
                Some(expected),
                "doc-store insert count: {w}"
            );
        } else {
            assert_eq!(
                w["rows_affected"].as_u64(),
                Some(expected),
                "sql rows_affected: {w}"
            );
        }
    }

    pub fn assert_update_count(&self, w: &Value, expected: u64) {
        if self.backend.is_doc_store() {
            assert_eq!(
                w["modified"].as_u64(),
                Some(expected),
                "doc-store modified: {w}"
            );
        } else {
            assert_eq!(
                w["rows_affected"].as_u64(),
                Some(expected),
                "sql rows_affected: {w}"
            );
        }
    }

    pub fn assert_delete_count(&self, w: &Value, expected: u64) {
        if self.backend.is_doc_store() {
            assert_eq!(
                w["deleted"].as_u64(),
                Some(expected),
                "doc-store deleted: {w}"
            );
        } else {
            assert_eq!(
                w["rows_affected"].as_u64(),
                Some(expected),
                "sql rows_affected: {w}"
            );
        }
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
            database: None,
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
                database: None,
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
                database: None,
                _container: RunningContainer::Mysql(container),
            }
        }
        Backend::Mongo => {
            let container = Mongo::default().start().await.unwrap();
            let port = container.get_host_port_ipv4(27017).await.unwrap();
            BackendHarness {
                backend,
                connector_name: name.to_string(),
                connection_string: format!("mongodb://127.0.0.1:{port}"),
                database: Some("orion_test".to_string()),
                _container: RunningContainer::Mongo(container),
            }
        }
        Backend::Es => {
            // ES boots in 20-40s: the module's WaitFor blocks on the
            // "[YELLOW] to [GREEN]" health log line; give it 120s (default 60s)
            // and a small fixed heap so CI containers coexist.
            let container = ElasticSearch::default()
                .with_env_var("ES_JAVA_OPTS", "-Xms512m -Xmx512m")
                .with_startup_timeout(Duration::from_secs(120))
                .start()
                .await
                .unwrap();
            let port = container.get_host_port_ipv4(9200).await.unwrap();
            BackendHarness {
                backend,
                connector_name: name.to_string(),
                connection_string: format!("http://127.0.0.1:{port}"),
                database: None,
                _container: RunningContainer::Es(container),
            }
        }
    }
}
