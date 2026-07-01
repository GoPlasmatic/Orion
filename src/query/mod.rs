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
