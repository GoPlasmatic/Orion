//! Backend renderers over the same `Cond` IR: SQL (`sql`), MongoDB (`mongo`), and
//! Elasticsearch (`es`), plus the planning they share: page-size and skip
//! resolution, the projection rule, the sort rule (direction + nulls
//! placement), and the document-store upsert split are decided once here and
//! mapped to each backend's native form.

pub mod es;
pub mod mongo;
pub mod sql;

use crate::config::QueryConfig;
use crate::query::error::QueryError;
use crate::query::ir::{self, RelRef};
use crate::query::spec::{QuerySpec, SortDir, SortKey};
use crate::query::write::{ConflictAction, ResolvedConflict, ResolvedWrite, WriteError};
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

/// Which of a document store's upsert body shapes applies — decided once from
/// the conflict action and whether an explicit `set` was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpsertMode {
    /// `action: update` with no explicit `set` — the incoming row replaces the
    /// existing document's non-key columns (ES: `doc_as_upsert`).
    Replace,
    /// `action: update` with an explicit `set`: `set` applies on conflict, the
    /// remaining inserted columns only on first insert.
    SetWithInsertDefaults,
    /// `action: nothing` — insert if absent, never touch an existing document
    /// (ES: `op_type=create`).
    InsertOnly,
}

/// The planned single-document upsert both document stores consume: the one
/// row, the body-shape mode, and the assignments split into an on-conflict half
/// and an insert-only half. How each backend renders the split (per-column
/// `$set`/`$setOnInsert` vs whole-row merge) stays with that backend.
pub(crate) struct UpsertPlan<'a> {
    pub row: &'a [ir::Value],
    pub mode: UpsertMode,
    /// Assignments applied when the row/document already exists.
    pub on_conflict: Vec<(&'a str, &'a ir::Value)>,
    /// Assignments applied only when it is first inserted.
    pub insert_only: Vec<(&'a str, &'a ir::Value)>,
}

/// Plan a document-store upsert: enforce the single-row limit (bulk upsert is a
/// capability error naming `target_backend`) and compute the set / set-on-insert
/// split from the conflict action.
pub(crate) fn plan_upsert<'a>(
    columns: &'a [String],
    rows: &'a [Vec<ir::Value>],
    set: &'a [(String, ir::Value)],
    conflict: &'a ResolvedConflict,
    target_backend: &str,
) -> Result<UpsertPlan<'a>, WriteError> {
    // A single-document upsert keyed on the conflict target. Bulk upsert would
    // need one write per row; deferred (fail loudly, don't guess).
    if rows.len() != 1 {
        return Err(QueryError::FeatureUnsupportedByTarget {
            feature: "bulk upsert".to_string(),
            target: target_backend.to_string(),
        }
        .into());
    }
    let row = &rows[0];
    let non_target = |col: &String| !conflict.targets.contains(col);

    let (mode, on_conflict, insert_only) = match conflict.action {
        ConflictAction::Update if set.is_empty() => {
            // Overwrite every non-target column on conflict.
            let on_conflict = columns
                .iter()
                .zip(row)
                .filter(|(c, _)| non_target(c))
                .map(|(c, v)| (c.as_str(), v))
                .collect();
            (UpsertMode::Replace, on_conflict, Vec::new())
        }
        ConflictAction::Update => {
            // `set` applies on conflict; inserted columns not in `set` (and not
            // targets) apply only on insert.
            let on_conflict = set.iter().map(|(c, v)| (c.as_str(), v)).collect();
            let insert_only = columns
                .iter()
                .zip(row)
                .filter(|(c, _)| non_target(c) && !set.iter().any(|(s, _)| s == *c))
                .map(|(c, v)| (c.as_str(), v))
                .collect();
            (UpsertMode::SetWithInsertDefaults, on_conflict, insert_only)
        }
        ConflictAction::Nothing => {
            // Insert the row if absent; leave an existing one untouched.
            let insert_only = columns
                .iter()
                .zip(row)
                .filter(|(c, _)| non_target(c))
                .map(|(c, v)| (c.as_str(), v))
                .collect();
            (UpsertMode::InsertOnly, Vec::new(), insert_only)
        }
    };

    Ok(UpsertPlan {
        row,
        mode,
        on_conflict,
        insert_only,
    })
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

/// Refuse `returning` on a target that cannot read rows back from a write.
///
/// The third member of a family whose other two exist for exactly this reason.
/// Mongo's `render_write` destructured every `ResolvedWrite` with `..` and never
/// looked at `returning`, so `data_write` with `"returning": ["email"]` answered
/// a **successful write with no `returning` key and no error** — the caller
/// could not tell the field had been ignored. ES and SQL each refused it with
/// their own hand-written copy of this check, and nothing made a backend answer
/// the capability question, so a fourth would have repeated the silence.
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
