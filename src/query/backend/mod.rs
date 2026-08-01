//! Backend renderers over the same `Cond` IR: SQL (`sql`), MongoDB (`mongo`), and
//! Elasticsearch (`es`), plus the envelope planning they share: page-size and
//! skip resolution, the projection rule, and the sort rule (direction + nulls
//! placement) are decided once here and mapped to each backend's native form.

pub mod es;
pub mod mongo;
pub mod sql;

use crate::config::QueryConfig;
use crate::query::error::QueryError;
use crate::query::ir::RelRef;
use crate::query::spec::{QuerySpec, SortDir, SortKey};
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
/// terms. Each renderer maps it to its native form.
pub(crate) struct SortPlan<'a> {
    pub field: &'a str,
    pub ascending: bool,
    /// Where rows without a meaningful value sort. **The rule (W8): a null
    /// sorts as the smallest value** — nulls first on `asc`, last on `desc`.
    /// That is already the native ordering of SQLite, MySQL and MongoDB;
    /// PostgreSQL (which sorts nulls as the largest value) and Elasticsearch
    /// have to be told.
    pub nulls_first: bool,
}

/// Plan the sort keys once for every renderer, applying the W8 null-ordering
/// rule. Deciding placement here is what keeps the three renderers from
/// restating (and eventually skewing) the rule — a renderer only maps
/// `nulls_first` to its native syntax, or documents why its native order
/// already realises it.
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

/// Refuse an `include` on a document store. F26: `include` hydration exists only
/// on SQL. It used to be silently dropped by both non-SQL renderers — parents came
/// back with no children and no error, in direct violation of the never-approximate
/// rule. Shared so the two cannot drift apart.
pub(crate) fn reject_include(spec: &QuerySpec, target: &str) -> Result<(), QueryError> {
    match spec.include.first() {
        Some(inc) => Err(QueryError::FeatureUnsupportedByTarget {
            feature: format!("include '{}'", inc.relation),
            target: target.to_string(),
        }),
        None => Ok(()),
    }
}

/// Refuse a many-to-many relation predicate on a document store. W11: a
/// many-to-many relation needs a junction join, which neither a Mongo `find`
/// filter nor the ES query DSL can express. It used to render as a plain
/// `$elemMatch` / `nested` on the relation name — wrong results, no error — while
/// include planning correctly gated m2m.
pub(crate) fn reject_many_to_many(rel: &RelRef, target: &str) -> Result<(), QueryError> {
    if rel.through.is_some() {
        return Err(QueryError::FeatureUnsupportedByTarget {
            feature: format!("many-to-many relation '{}'", rel.name),
            target: target.to_string(),
        });
    }
    Ok(())
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
