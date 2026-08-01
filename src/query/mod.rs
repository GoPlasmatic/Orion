//! The common query dialect: one backend-neutral query (filter + envelope) that
//! renders to a real backend — SQL (SQLite/PostgreSQL/MySQL), MongoDB and
//! Elasticsearch, in identity or schema-mapped mode. The normative semantics
//! live in `docs/src/reference/data-dialect.md` (published at
//! <https://goplasmatic.github.io/Orion/reference/data-dialect.html>).
//!
//! Pipeline: [`spec::parse`] (envelope) → [`lower::lower_with`] (filter →
//! [`ir::Cond`]) → [`backend::sql::render`] (`Cond` → `sea_query::SelectStatement`)
//! → [`backend::sql::build_for`] (dialect-specific `(sql, values)` for `AnyPool`).

pub mod backend;
pub mod bulk;
pub mod error;
pub mod ir;
pub mod lower;
pub mod schema;
pub mod spec;
pub mod vocab;
pub mod write;

pub use backend::SqlDialect;
pub use error::QueryError;
pub use lower::Params;
pub use schema::EntityRegistry;

use crate::config::QueryConfig;
use ir::Cond;
use sea_query::SelectStatement;
use serde_json::Value as Json;

/// Parse the envelope, lower the filter (identity mode), and render a SQL
/// `SelectStatement` for `dialect`, enforcing the configured page bounds.
/// `params` are concrete (already message-resolved) values substituted for
/// `{"param": ..}` nodes. Test convenience — production goes through
/// [`plan_sql`], which additionally resolves `include`s.
#[cfg(test)]
pub fn translate_sql(
    query: &Json,
    params: &Params,
    dialect: SqlDialect,
    limits: &QueryConfig,
) -> Result<SelectStatement, QueryError> {
    translate_sql_with_schema(query, params, &EntityRegistry::identity(), dialect, limits)
}

/// Schema-aware variant: resolves fields and relations through `reg` (renames,
/// type hints, allowlist, and the relation declarations `some`/`all`/`none` need).
#[cfg(test)]
pub fn translate_sql_with_schema(
    query: &Json,
    params: &Params,
    reg: &EntityRegistry,
    dialect: SqlDialect,
    limits: &QueryConfig,
) -> Result<SelectStatement, QueryError> {
    let (spec, cond, root_table) = prepare(query, params, reg)?;
    backend::sql::render(&spec, &cond, &root_table, dialect, limits)
}

/// A rendered SQL query plus its `include` plan (related collections to nest).
#[derive(Debug, Clone)]
pub struct SqlPlan {
    pub main: SelectStatement,
    pub includes: Vec<IncludePlan>,
    /// Parent physical columns added only to group children — stripped from output.
    pub strip: Vec<String>,
}

/// One resolved `include`: the child relation to fetch and nest per parent.
#[derive(Debug, Clone)]
pub struct IncludePlan {
    /// Output field name (the relation name).
    pub field: String,
    pub target_table: String,
    /// Parent physical key column (grouping key on the parent side).
    pub local: String,
    /// Child physical foreign-key column (grouping key on the child side).
    pub foreign: String,
    /// Child fields to select (physical); empty = all.
    pub fields: Vec<String>,
    /// Ordering within each parent's children (physical names). Never empty —
    /// the per-parent page is cut in SQL, so it needs a deterministic key (F27);
    /// [`plan_sql`] refuses an include that does not state one.
    pub sort: Vec<spec::SortKey>,
    /// Resolved per-parent row cap, already bounded by `query.max_limit`.
    pub limit: u64,
}

impl IncludePlan {
    /// Physical child columns the include's sub-select must project.
    ///
    /// The requested `fields`, **plus** the foreign key (the handler groups on
    /// it) **plus** every sort key. The sort keys are not optional plumbing: the
    /// per-parent page is cut by ranking rows inside a sub-select and filtering
    /// the rank outside it, so the outer `ORDER BY` names columns of the
    /// *subquery's output*. A sort key that was not projected does not exist
    /// there — PostgreSQL says `column "…" does not exist`, MySQL says `Unknown
    /// column '…' in 'order clause'`, and SQLite quietly reads the quoted name
    /// as a string literal, making the whole `ORDER BY` a constant and handing
    /// back rows in window order. That last one is exactly the undefined
    /// per-parent ordering F27 exists to remove.
    ///
    /// Empty means "project everything" (`SELECT *`), which already has them.
    pub fn projection(&self) -> Vec<String> {
        if self.fields.is_empty() {
            return Vec::new();
        }
        let mut cols = self.fields.clone();
        for extra in std::iter::once(&self.foreign).chain(self.sort.iter().map(|k| &k.field)) {
            if !cols.contains(extra) {
                cols.push(extra.clone());
            }
        }
        cols
    }

    /// The part of [`projection`](Self::projection) the caller did not ask for —
    /// the grouping key and any sort-only column. Dropped from each nested child
    /// object after grouping, the same way the window's rank column is.
    pub fn strip(&self) -> Vec<String> {
        self.projection()
            .into_iter()
            .filter(|c| !self.fields.contains(c))
            .collect()
    }
}

/// A parent/child join-key value, compared by *type and value* rather than by
/// its `serde_json` text (W14).
///
/// Include hydration groups children by their foreign key and looks each parent
/// up by its local key. Those two values arrive from two different queries and
/// therefore two different columns, and the driver renders a column's value from
/// its SQL type: a key stored `TEXT` on one side and `BIGINT` on the other came
/// back as `"7"` and `7`, whose JSON text differs, so every child array silently
/// came back empty. Normalising integral values — a JSON integer, an integral
/// float, or a decimal string that round-trips — onto one variant makes the two
/// renderings the same key, while anything else keeps its own identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GroupKey {
    Bool(bool),
    /// Any integral key, however the driver rendered it (`7`, `7.0`, `"7"`).
    Int(i64),
    /// A non-integral number, keyed on its bit pattern (JSON has no NaN).
    Float(u64),
    Str(String),
}

impl GroupKey {
    /// The grouping key for a JSON scalar, or `None` for a value that cannot
    /// join anything (null, array, object) — mirroring
    /// [`backend::sql::json_key_to_sea`], which skips exactly those.
    pub fn from_json(v: &Json) -> Option<Self> {
        match v {
            Json::Bool(b) => Some(GroupKey::Bool(*b)),
            Json::Number(n) => match n.as_i64() {
                Some(i) => Some(GroupKey::Int(i)),
                None => n.as_f64().map(Self::from_f64),
            },
            // A key column read back as text still joins an integer key on the
            // other side, but only when the text *is* that integer: "007" and
            // " 7" are their own keys, because they are not what `7` renders as.
            Json::String(s) => Some(match s.parse::<i64>() {
                Ok(i) if i.to_string() == *s => GroupKey::Int(i),
                _ => GroupKey::Str(s.clone()),
            }),
            _ => None,
        }
    }

    fn from_f64(f: f64) -> Self {
        if f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
            GroupKey::Int(f as i64)
        } else {
            GroupKey::Float(f.to_bits())
        }
    }
}

/// The shared prologue of every translation: parse the envelope, resolve the
/// entity, lower the filter, and resolve the remaining logical names. Returns the
/// resolved spec, the lowered condition, and the physical table / collection /
/// index.
///
/// The order is the point, and it is the same for every backend.
///
/// F24: the entity gate runs *first*, before the filter is lowered and before
/// any field or relation is resolved. Every one of those steps also fails on an
/// undeclared entity — with `invalid field reference 'age'` or `unknown relation
/// 'orders'`, neither of which names the `schema` key that is actually missing.
/// Resolving the table first means the one error a 0.x workflow hits is the one
/// that says how to migrate it, whatever else the query happens to mention.
///
/// W2: projection and sort then go through the same allowlist / rename /
/// identifier gate as the filter, before any backend sees the spec.
fn prepare(
    query: &Json,
    params: &Params,
    reg: &EntityRegistry,
) -> Result<(spec::QuerySpec, Cond, String), QueryError> {
    let spec = spec::parse(query)?;
    let table = reg.physical_table(&spec.source)?;
    let cond = match &spec.filter {
        Some(f) => lower::lower_with(f, params, reg, &spec.source)?,
        None => Cond::True,
    };
    let spec = spec.resolve_names(reg)?;
    Ok((spec, cond, table))
}

/// Plan a SQL query with `include`s: render the main `SelectStatement` (with any
/// parent keys needed for grouping added) and resolve each include to an
/// [`IncludePlan`] the handler hydrates with a per-relation child query.
pub fn plan_sql(
    query: &Json,
    params: &Params,
    reg: &EntityRegistry,
    dialect: SqlDialect,
    limits: &QueryConfig,
) -> Result<SqlPlan, QueryError> {
    let (mut spec, cond, root_table) = prepare(query, params, reg)?;

    // Each `include` becomes an [`IncludePlan`] the handler hydrates separately;
    // the SQL renderer never reads `include`, so take it out of the spec rather
    // than copying the whole spec to render from.
    let include_specs = std::mem::take(&mut spec.include);
    let mut includes = Vec::new();
    let mut strip: Vec<String> = Vec::new();
    for inc in &include_specs {
        let (rel, _target) = reg.resolve_relation(&spec.source, &inc.relation, "include")?;
        if rel.through.is_some() {
            return Err(QueryError::FeatureUnsupportedByTarget {
                feature: format!("many-to-many include '{}'", inc.relation),
                target: "sql".to_string(),
            });
        }
        // F27: the per-parent page is cut in SQL (`ROW_NUMBER() OVER (PARTITION
        // BY …)`), so an unordered include is a request for an arbitrary subset.
        // It used to fetch every child of every parent on the page and truncate
        // in memory — unbounded, and a different subset run to run.
        //
        // Checked here rather than in `spec::parse`, which every backend shares:
        // MongoDB and Elasticsearch cannot answer an include at all, and their
        // capability error is the useful answer for a caller who wrote one.
        if inc.sort.is_empty() {
            return Err(QueryError::InvalidEnvelope(format!(
                "include.{} requires a 'sort' — the per-parent page needs a \
                 deterministic order key (e.g. \"sort\": [{{\"id\": \"asc\"}}])",
                inc.relation
            )));
        }
        // Ensure the parent key is projected so children can be grouped back.
        if !spec.fields.is_empty() && !spec.fields.iter().any(|f| f == &rel.local) {
            spec.fields.push(rel.local.clone());
            if !strip.contains(&rel.local) {
                strip.push(rel.local.clone());
            }
        }
        // F27: the per-parent cap is the envelope's own page policy, applied per
        // parent — default when absent, rejected (never clamped) when over the
        // maximum. Hydration used to be unbounded: every child of every parent
        // on the page was materialised and then truncated in memory.
        let limit = backend::resolve_limit(inc.limit, limits)?;
        includes.push(IncludePlan {
            field: inc.relation.clone(),
            target_table: rel.target_table,
            local: rel.local,
            foreign: rel.foreign,
            fields: inc.fields.clone(),
            sort: inc.sort.clone(),
            limit,
        });
    }

    let main = backend::sql::render(&spec, &cond, &root_table, dialect, limits)?;
    Ok(SqlPlan {
        main,
        includes,
        strip,
    })
}

/// Parse the envelope, lower the filter, and render a MongoDB `find` query
/// (collection + `$match` filter + projection/sort/skip/limit), enforcing the
/// configured page bounds.
pub fn translate_mongo(
    query: &Json,
    params: &Params,
    reg: &EntityRegistry,
    limits: &QueryConfig,
) -> Result<backend::mongo::MongoQuery, QueryError> {
    let (spec, cond, collection) = prepare(query, params, reg)?;
    backend::mongo::render(&spec, &cond, &collection, limits)
}

/// Parse the envelope, lower the filter, and render an Elasticsearch search
/// (index + query DSL body in filter context), enforcing the page bounds and
/// the deep-pagination cap.
pub fn translate_es(
    query: &Json,
    params: &Params,
    reg: &EntityRegistry,
    limits: &QueryConfig,
) -> Result<backend::es::EsQuery, QueryError> {
    let (spec, cond, index) = prepare(query, params, reg)?;
    backend::es::render(&spec, &cond, &index, limits)
}

#[cfg(test)]
mod group_key_tests {
    use super::GroupKey;
    use serde_json::json;

    /// W14: the parent's key and the child's foreign key come from two
    /// different columns, and the driver renders a value from its SQL type.
    /// Grouping by the key's `serde_json` *text* meant a `TEXT` key on one side
    /// and a `BIGINT` key on the other never matched — every child array came
    /// back empty, with no error anywhere.
    #[test]
    fn integral_keys_group_regardless_of_how_the_driver_rendered_them() {
        let expected = Some(GroupKey::Int(7));
        assert_eq!(GroupKey::from_json(&json!(7)), expected);
        assert_eq!(GroupKey::from_json(&json!(7.0)), expected);
        assert_eq!(GroupKey::from_json(&json!("7")), expected);
        assert_eq!(GroupKey::from_json(&json!(-7)), Some(GroupKey::Int(-7)));
        assert_eq!(GroupKey::from_json(&json!("-7")), Some(GroupKey::Int(-7)));
    }

    /// Only text that *is* the integer's rendering collapses onto it: a
    /// zero-padded or space-padded key is a different key, not the same one.
    #[test]
    fn text_that_merely_parses_as_a_number_keeps_its_own_identity() {
        assert_eq!(
            GroupKey::from_json(&json!("007")),
            Some(GroupKey::Str("007".into()))
        );
        assert_eq!(
            GroupKey::from_json(&json!(" 7")),
            Some(GroupKey::Str(" 7".into()))
        );
        assert_eq!(
            GroupKey::from_json(&json!("+7")),
            Some(GroupKey::Str("+7".into()))
        );
        assert_ne!(
            GroupKey::from_json(&json!("u1")),
            GroupKey::from_json(&json!("u2"))
        );
    }

    #[test]
    fn non_joinable_values_have_no_key() {
        for v in [json!(null), json!([1]), json!({ "a": 1 })] {
            assert_eq!(GroupKey::from_json(&v), None, "{v}");
        }
    }

    #[test]
    fn booleans_and_fractional_numbers_keep_their_own_variants() {
        assert_eq!(
            GroupKey::from_json(&json!(true)),
            Some(GroupKey::Bool(true))
        );
        assert_ne!(
            GroupKey::from_json(&json!(1.5)),
            GroupKey::from_json(&json!(1))
        );
        assert_eq!(
            GroupKey::from_json(&json!(1.5)),
            GroupKey::from_json(&json!(1.5))
        );
    }
}
