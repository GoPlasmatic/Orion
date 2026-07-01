//! The common query dialect: one backend-neutral query (filter + envelope) that
//! renders to a real backend. Phase 1 is scalar SQL in identity mode; see
//! `proposals/query-dialect.md` for the full design and later phases.
//!
//! Pipeline: [`spec::parse`] (envelope) → [`lower::lower`] (filter → [`ir::Cond`])
//! → [`backend::sql::render`] (`Cond` → `sea_query::SelectStatement`) →
//! [`backend::sql::build_for`] (dialect-specific `(sql, values)` for `AnyPool`).

pub mod backend;
pub mod error;
pub mod ir;
pub mod lower;
pub mod schema;
pub mod spec;
pub mod vocab;

pub use backend::SqlDialect;
pub use error::QueryError;
pub use lower::Params;
pub use schema::EntityRegistry;
pub use spec::QuerySpec;

use ir::Cond;
use sea_query::SelectStatement;
use serde_json::Value as Json;

/// Parse the envelope, lower the filter (identity mode), and render a SQL
/// `SelectStatement` for `dialect`, enforcing the configured page-size bounds.
/// `params` are concrete (already message-resolved) values substituted for
/// `{"param": ..}` nodes.
pub fn translate_sql(
    query: &Json,
    params: &Params,
    dialect: SqlDialect,
    default_limit: u64,
    max_limit: u64,
) -> Result<SelectStatement, QueryError> {
    translate_sql_with_schema(
        query,
        params,
        &EntityRegistry::default(),
        dialect,
        default_limit,
        max_limit,
    )
}

/// Schema-aware variant: resolves fields and relations through `reg` (renames,
/// type hints, allowlist, and the relation declarations `some`/`all`/`none` need).
pub fn translate_sql_with_schema(
    query: &Json,
    params: &Params,
    reg: &EntityRegistry,
    dialect: SqlDialect,
    default_limit: u64,
    max_limit: u64,
) -> Result<SelectStatement, QueryError> {
    let spec = spec::parse(query)?;
    let cond = match &spec.filter {
        Some(f) => lower::lower_with(f, params, reg, &spec.source)?,
        None => Cond::True,
    };
    let root_table = reg.physical_table(&spec.source);
    backend::sql::render(&spec, &cond, &root_table, dialect, default_limit, max_limit)
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
    pub limit: Option<u64>,
}

/// Plan a SQL query with `include`s: render the main `SelectStatement` (with any
/// parent keys needed for grouping added) and resolve each include to an
/// [`IncludePlan`] the handler hydrates with a per-relation child query.
pub fn plan_sql(
    query: &Json,
    params: &Params,
    reg: &EntityRegistry,
    dialect: SqlDialect,
    default_limit: u64,
    max_limit: u64,
) -> Result<SqlPlan, QueryError> {
    let spec = spec::parse(query)?;
    let cond = match &spec.filter {
        Some(f) => lower::lower_with(f, params, reg, &spec.source)?,
        None => Cond::True,
    };

    let mut main_spec = spec.clone();
    let mut includes = Vec::new();
    let mut strip: Vec<String> = Vec::new();
    for inc in &spec.include {
        let (rel, _target) = reg.resolve_relation(&spec.source, &inc.relation, "include")?;
        if rel.through.is_some() {
            return Err(QueryError::FeatureUnsupportedByTarget {
                feature: format!("many-to-many include '{}'", inc.relation),
                target: "sql".to_string(),
            });
        }
        // Ensure the parent key is projected so children can be grouped back.
        if !main_spec.fields.is_empty() && !main_spec.fields.iter().any(|f| f == &rel.local) {
            main_spec.fields.push(rel.local.clone());
            if !strip.contains(&rel.local) {
                strip.push(rel.local.clone());
            }
        }
        includes.push(IncludePlan {
            field: inc.relation.clone(),
            target_table: rel.target_table,
            local: rel.local,
            foreign: rel.foreign,
            fields: inc.fields.clone(),
            limit: inc.limit,
        });
    }

    let root_table = reg.physical_table(&spec.source);
    let main = backend::sql::render(
        &main_spec,
        &cond,
        &root_table,
        dialect,
        default_limit,
        max_limit,
    )?;
    Ok(SqlPlan {
        main,
        includes,
        strip,
    })
}

/// Parse the envelope, lower the filter, and render a MongoDB `find` query
/// (collection + `$match` filter + projection/sort/skip/limit), enforcing the
/// configured page-size bounds.
pub fn translate_mongo(
    query: &Json,
    params: &Params,
    reg: &EntityRegistry,
    default_limit: u64,
    max_limit: u64,
) -> Result<backend::mongo::MongoQuery, QueryError> {
    let spec = spec::parse(query)?;
    let cond = match &spec.filter {
        Some(f) => lower::lower_with(f, params, reg, &spec.source)?,
        None => Cond::True,
    };
    let collection = reg.physical_table(&spec.source);
    backend::mongo::render(&spec, &cond, &collection, default_limit, max_limit)
}

/// Validate a query against `dialect` without retaining the rendered output.
/// A query that validates clean cannot then fail in [`translate_sql`].
pub fn validate_sql(
    query: &Json,
    params: &Params,
    dialect: SqlDialect,
    default_limit: u64,
    max_limit: u64,
) -> Result<(), QueryError> {
    translate_sql(query, params, dialect, default_limit, max_limit).map(|_| ())
}
