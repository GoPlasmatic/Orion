//! Backend renderers over the same `Cond` IR: SQL (`sql`), MongoDB (`mongo`), and
//! Elasticsearch (`es`).

pub mod es;
pub mod mongo;
pub mod sql;

use crate::query::error::QueryError;
use crate::storage::DbBackend;

/// Resolve the effective page size: an explicit `limit` above `max_limit` is
/// an error, no `limit` means the default clamped to the max. All three
/// renderers share this so page-size policy cannot differ per backend.
pub(crate) fn resolve_limit(
    requested: Option<u64>,
    default_limit: u64,
    max_limit: u64,
) -> Result<u64, QueryError> {
    match requested {
        Some(l) if l > max_limit => Err(QueryError::LimitExceeded {
            requested: l,
            max: max_limit,
        }),
        Some(l) => Ok(l),
        None => Ok(default_limit.min(max_limit)),
    }
}

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
