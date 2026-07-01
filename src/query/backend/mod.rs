//! Backend renderers over the same `Cond` IR: SQL (`sql`), MongoDB (`mongo`), and
//! Elasticsearch (later).

pub mod mongo;
pub mod sql;

use crate::storage::DbBackend;

/// The SQL dialect to render for. Chosen from the *connector's* connection-string
/// scheme (via [`crate::storage::detect_backend`]) — never Orion's own storage
/// backend — so the rendered SQL matches the pool that will execute it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlDialect {
    Sqlite,
    Postgres,
    Mysql,
}

impl From<DbBackend> for SqlDialect {
    fn from(b: DbBackend) -> Self {
        match b {
            DbBackend::Sqlite => SqlDialect::Sqlite,
            DbBackend::Postgres => SqlDialect::Postgres,
            DbBackend::Mysql => SqlDialect::Mysql,
        }
    }
}
