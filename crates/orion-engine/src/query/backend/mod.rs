//! The backend renderer over the `Cond` IR — SQL (`sql`) — plus the planning
//! it shares with keyset paging and include hydration: page-size and skip
//! resolution, the projection rule and the sort rule (direction + nulls
//! placement) are decided once here and mapped to the native form.

pub mod sql;

use crate::config::QueryConfig;
use crate::query::error::QueryError;
use crate::query::spec::{SortDir, SortKey};
use crate::query::write::ResolvedWrite;
use crate::storage::DbBackend;

/// Resolve the effective page size: an explicit `limit` above `max_limit` is
/// an error, no `limit` means the default clamped to the max. The top-level
/// select and include hydration share this so page-size policy cannot differ
/// between them.
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
/// [`resolve_limit`]'s reject-never-clamp rule (W12).
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

/// The projection plan: `None` = every column/field (an empty `fields` list
/// means "everything" — identity mode only, since a declared schema's
/// `resolve_names` injects the entity's queryable columns), `Some` = exactly
/// the named fields.
pub(crate) fn plan_projection(fields: &[String]) -> Option<&[String]> {
    if fields.is_empty() {
        None
    } else {
        Some(fields)
    }
}

/// One planned sort key: direction plus nulls placement, in backend-neutral
/// terms. The renderer maps it to its native form.
pub(crate) struct SortPlan<'a> {
    pub field: &'a str,
    pub ascending: bool,
    /// Where rows without a meaningful value sort. **The rule (W8): a null
    /// sorts as the smallest value** — nulls first on `asc`, last on `desc`.
    /// That is already the native ordering of SQLite and MySQL; PostgreSQL
    /// (which sorts nulls as the largest value) has to be told.
    pub nulls_first: bool,
}

/// Plan the sort keys once, applying the W8 null-ordering rule. Deciding
/// placement here is what keeps the renderer and keyset paging from restating
/// (and eventually skewing) the rule — each only maps `nulls_first` to its
/// native form, or documents why the native order already realises it.
pub(crate) fn plan_sort(sort: &[SortKey]) -> Vec<SortPlan<'_>> {
    sort.iter()
        .map(|k| {
            let ascending = matches!(k.dir, SortDir::Asc);
            SortPlan {
                field: &k.field,
                ascending,
                nulls_first: ascending,
            }
        })
        .collect()
}

/// Refuse `returning` on a target that cannot read rows back from a write
/// (MySQL has no `RETURNING`). Refused rather than ignored: a write that drops
/// the field answers success with no `returning` key, and the caller cannot
/// tell the field had been ignored.
pub(crate) fn reject_returning(w: &ResolvedWrite, target: &str) -> Result<(), QueryError> {
    if w.returning().is_empty() {
        return Ok(());
    }
    Err(QueryError::FeatureUnsupportedByTarget {
        feature: "returning".to_string(),
        target: target.to_string(),
    })
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
