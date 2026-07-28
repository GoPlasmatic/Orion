//! Backend renderers over the same `Cond` IR: SQL (`sql`), MongoDB (`mongo`), and
//! Elasticsearch (`es`).

pub mod es;
pub mod mongo;
pub mod sql;

use crate::config::QueryConfig;
use crate::query::error::QueryError;
use crate::storage::DbBackend;

/// Resolve the effective page size: an explicit `limit` above `max_limit` is
/// an error, no `limit` means the default clamped to the max. All three
/// renderers share this so page-size policy cannot differ per backend.
pub(crate) fn resolve_limit(
    requested: Option<u64>,
    limits: &QueryConfig,
) -> Result<u64, QueryError> {
    match requested {
        Some(l) if l > limits.max_limit => Err(QueryError::LimitExceeded {
            requested: l,
            max: limits.max_limit,
        }),
        Some(l) => Ok(l),
        None => Ok(limits.default_limit.min(limits.max_limit)),
    }
}

/// Resolve the effective `skip` offset: above `max_skip` is an error, mirroring
/// [`resolve_limit`]'s reject-never-clamp rule. Shared by all three renderers —
/// the cap used to exist only on Elasticsearch (via its result window), so the
/// same envelope was bounded on one backend and unbounded on the others (W12).
pub(crate) fn resolve_skip(
    requested: Option<u64>,
    limits: &QueryConfig,
) -> Result<Option<u64>, QueryError> {
    match requested {
        Some(s) if s > limits.max_skip => Err(QueryError::SkipExceeded {
            requested: s,
            max: limits.max_skip,
        }),
        other => Ok(other),
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
