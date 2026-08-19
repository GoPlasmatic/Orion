//! SQL rendering over sea-query.
//!
//! Walks a [`Cond`] + [`QuerySpec`] into a `sea_query::SelectStatement`, then
//! [`build_for`] produces the dialect-specific `(sql, values)` bound to the
//! external connector's `sqlx::AnyPool`. Identifiers are dynamic
//! (`sea_query::Alias`) and every literal is a bound value, so the output is
//! injection-safe and quoted per dialect.

use sea_query::{
    Alias, Asterisk, Condition, Expr, ExprTrait, Func, LikeExpr, MysqlQueryBuilder, NullOrdering,
    OnConflict, Order, OrderedStatement, PostgresQueryBuilder, Query, SelectStatement, SimpleExpr,
    SqliteQueryBuilder, Value as SeaValue, WindowStatement,
};
use sea_query_sqlx::{SqlxBinder, SqlxValues};

use crate::config::QueryConfig;
use crate::query::IncludePlan;
use crate::query::backend::SqlDialect;
use crate::query::error::QueryError;
use crate::query::ir::{CmpOp, Cond, FieldRef, Quant, RelRef, TextOp, Value};
use crate::query::spec::{QuerySpec, SortKey};
use crate::query::write::{ConflictAction, ResolvedConflict, ResolvedWrite, WriteError};

/// Column the include's window function writes its per-parent row number into.
/// Prefixed so it cannot collide with a real column; stripped from the output.
pub const INCLUDE_RANK_COLUMN: &str = "__orion_include_rank";

/// Alias for the include's inner (windowed) sub-select. A window function is not
/// usable in `WHERE`, so the rank is computed in a subquery and filtered outside.
const INCLUDE_SUBQUERY_ALIAS: &str = "__orion_include";

/// Build a `SelectStatement` from the envelope and lowered condition, enforcing
/// the page bounds (`LimitExceeded` / `SkipExceeded` when over the configured
/// caps). `root_table` is the physical table the query selects from (and the
/// correlation base for any relation subqueries).
pub fn render(
    spec: &QuerySpec,
    cond: &Cond,
    root_table: &str,
    dialect: SqlDialect,
    limits: &QueryConfig,
) -> Result<SelectStatement, QueryError> {
    let limit = resolve_limit(spec.limit, limits)?;
    let skip = resolve_skip(spec.skip, limits)?;

    let mut stmt = Query::select();
    match super::plan_projection(&spec.fields) {
        None => {
            stmt.column(Asterisk);
        }
        Some(fields) => {
            for f in fields {
                stmt.column(Alias::new(f.as_str()));
            }
        }
    }
    stmt.from(Alias::new(root_table));
    // Skip the WHERE clause entirely when the filter is unconditionally true, so
    // a filterless query does not render a redundant `WHERE TRUE`.
    if !matches!(cond, Cond::True) {
        stmt.cond_where(render_expr(cond, root_table)?);
    }
    apply_sort_keys(&mut stmt, &spec.sort, dialect);
    stmt.limit(limit);
    if let Some(skip) = skip {
        stmt.offset(skip);
    }
    Ok(stmt)
}

/// Produce the dialect-specific SQL string + bound values for execution on an
/// `AnyPool`. The dialect must match the connector's real driver (see
/// [`SqlDialect`]).
pub fn build_for(dialect: SqlDialect, stmt: &SelectStatement) -> (String, SqlxValues) {
    match dialect {
        SqlDialect::Postgres => stmt.build_sqlx(PostgresQueryBuilder),
        SqlDialect::Mysql => stmt.build_sqlx(MysqlQueryBuilder),
        SqlDialect::Sqlite => stmt.build_sqlx(SqliteQueryBuilder),
    }
}

/// Build the child query for an `include`, with the per-parent page cut **in
/// SQL** (F27):
///
/// ```sql
/// SELECT <projection> FROM (
///   SELECT <projection>,
///          ROW_NUMBER() OVER (PARTITION BY <foreign> ORDER BY <sort>) AS <rank>
///   FROM <target> WHERE <foreign> IN (<keys>)
/// ) AS <alias>
/// WHERE <rank> <= <limit>
/// ORDER BY <sort>
/// ```
///
/// This used to be a bare `SELECT … WHERE fk IN (…)` with no `LIMIT` and no
/// `ORDER BY`, and the handler truncated afterwards: 1000 parents × 10 000
/// children materialised 10M rows to return 5 each, and because nothing ordered
/// them, `include.limit` returned an *arbitrary* subset that could differ run to
/// run. The window function is supported by every backend the dialect renders
/// for (SQLite ≥ 3.25 — the bundled build is far newer, PostgreSQL ≥ 8.4,
/// MySQL ≥ 8.0).
///
/// `<projection>` is [`IncludePlan::projection`] on **both** levels: the
/// requested fields plus the foreign key (for grouping) plus the sort keys,
/// because the outer `ORDER BY` can only name columns the sub-select emits. The
/// handler drops the extras again ([`IncludePlan::strip`]). Callers must skip
/// this when `keys` is empty (an empty `IN` is not built here).
pub fn build_include_select(
    inc: &IncludePlan,
    keys: &[SeaValue],
    dialect: SqlDialect,
) -> (String, SqlxValues) {
    let foreign = inc.foreign.as_str();
    let projection = inc.projection();

    // Inner: the child rows for this parent page, ranked within each parent.
    let mut inner = Query::select();
    project_child(&mut inner, &projection);
    let mut window = WindowStatement::partition_by(Alias::new(foreign));
    apply_sort_keys(&mut window, &inc.sort, dialect);
    inner.expr_window_as(
        Func::cust(Alias::new("ROW_NUMBER")),
        window,
        Alias::new(INCLUDE_RANK_COLUMN),
    );
    inner.from(Alias::new(inc.target_table.as_str()));
    inner.cond_where(Expr::col(Alias::new(foreign)).is_in(keys.to_vec()));

    // Outer: keep the first `limit` per parent, in the requested order.
    let mut stmt = Query::select();
    project_child(&mut stmt, &projection);
    stmt.from_subquery(inner, Alias::new(INCLUDE_SUBQUERY_ALIAS));
    // Bound as `i64`: the `sqlx-any` binder converts a `BigUnsigned` with an
    // unchecked `try_from`, which panics above `i64::MAX`.
    let cap = i64::try_from(inc.limit).unwrap_or(i64::MAX);
    stmt.cond_where(Expr::col(Alias::new(INCLUDE_RANK_COLUMN)).lte(cap));
    apply_sort_keys(&mut stmt, &inc.sort, dialect);
    build_for(dialect, &stmt)
}

/// Project a child row from [`IncludePlan::projection`]. An empty projection is
/// the unprojected case (`include` with no `fields`) and means every column —
/// the same [`super::plan_projection`] rule the top-level select follows.
fn project_child(stmt: &mut SelectStatement, projection: &[String]) {
    match super::plan_projection(projection) {
        None => {
            stmt.column(Asterisk);
        }
        Some(fields) => {
            for f in fields {
                stmt.column(Alias::new(f.as_str()));
            }
        }
    }
}

/// Convert a JSON scalar (a parent key value) into a bound `sea_query::Value` for
/// an `IN` list. Returns `None` for null / non-scalar values (skipped as keys).
pub fn json_key_to_sea(v: &serde_json::Value) -> Option<SeaValue> {
    match v {
        serde_json::Value::String(s) => Some(s.clone().into()),
        serde_json::Value::Bool(b) => Some((*b).into()),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(Into::into)
            .or_else(|| n.as_f64().map(Into::into)),
        _ => None,
    }
}

use super::{resolve_limit, resolve_skip};

/// Render a `Cond` as a single boolean `SimpleExpr`. Composing on `SimpleExpr`
/// (rather than `Condition`) keeps `and`/`or`/`not`, EXISTS subqueries, and the
/// the `all` null rule uniformly expressible. `current_table` is the physical
/// table the bare columns belong to (the correlation base for relations).
fn render_expr(cond: &Cond, current_table: &str) -> Result<SimpleExpr, QueryError> {
    Ok(match cond {
        Cond::True => Expr::val(1).eq(1),
        Cond::False => Expr::val(1).eq(0),
        Cond::And(cs) => fold_bool(cs, current_table, true)?,
        Cond::Or(cs) => fold_bool(cs, current_table, false)?,
        Cond::Not(inner) => render_expr(inner, current_table)?.not(),
        Cond::Compare { field, op, value } => compare_expr(field, *op, value)?,
        Cond::In {
            field,
            values,
            negated,
        } => in_expr(field, values, *negated)?,
        Cond::IsNull { field, negated } => {
            let col = col_expr(field);
            if *negated {
                col.is_not_null()
            } else {
                col.is_null()
            }
        }
        Cond::Between {
            field,
            low,
            high,
            low_incl,
            high_incl,
            negated,
        } => between_expr(field, low, high, *low_incl, *high_incl, *negated)?,
        Cond::Text { field, op, pattern } => text_expr(field, *op, pattern),
        Cond::Rel { quant, rel, cond } => render_rel(*quant, rel, cond, current_table)?,
    })
}

/// Fold a non-empty list of conditions with AND (`and = true`) or OR.
fn fold_bool(cs: &[Cond], current_table: &str, and: bool) -> Result<SimpleExpr, QueryError> {
    let mut iter = cs.iter();
    let mut acc = match iter.next() {
        Some(first) => render_expr(first, current_table)?,
        // Empty groups are folded to True/False at lowering; be defensive anyway.
        None => return Ok(Expr::val(1).eq(if and { 1 } else { 0 })),
    };
    for c in iter {
        let e = render_expr(c, current_table)?;
        acc = if and { acc.and(e) } else { acc.or(e) };
    }
    Ok(acc)
}

/// Render a relation predicate as an EXISTS-style semi/anti join.
fn render_rel(
    quant: Quant,
    rel: &RelRef,
    inner: &Cond,
    current_table: &str,
) -> Result<SimpleExpr, QueryError> {
    // Inner columns belong to the target entity's table.
    let target = rel.target_table.as_str();
    match quant {
        Quant::Any => {
            let inner_e = render_expr(inner, target)?;
            Ok(Expr::exists(rel_subquery(
                rel,
                current_table,
                Some(inner_e),
            )?))
        }
        Quant::None => {
            let inner_e = render_expr(inner, target)?;
            Ok(Expr::exists(rel_subquery(rel, current_table, Some(inner_e))?).not())
        }
        Quant::All => {
            // Reference semantics: false on an empty relation, and a null inner
            // predicate counts as a counterexample. Rendered as
            //   EXISTS(rel) AND NOT EXISTS(rel WHERE NOT c OR c IS NULL)
            //   (the `all` null rule)
            let nonempty = Expr::exists(rel_subquery(rel, current_table, None)?);
            let ie = render_expr(inner, target)?;
            let violates = ie.clone().not().or(Expr::expr(ie).is_null());
            let no_violation =
                Expr::exists(rel_subquery(rel, current_table, Some(violates))?).not();
            Ok(nonempty.and(no_violation))
        }
    }
}

/// Build `SELECT 1 FROM <rel> WHERE <correlation> [AND <inner>]`, joining through
/// the junction for a many-to-many relation.
fn rel_subquery(
    rel: &RelRef,
    current_table: &str,
    inner: Option<SimpleExpr>,
) -> Result<SelectStatement, QueryError> {
    let mut sub = Query::select();
    sub.expr(Expr::val(1));
    let mut where_cond = Condition::all();
    match &rel.through {
        None => {
            // Direct: target.foreign = current.local
            sub.from(Alias::new(rel.target_table.as_str()));
            where_cond = where_cond.add(
                Expr::col((
                    Alias::new(rel.target_table.as_str()),
                    Alias::new(rel.foreign.as_str()),
                ))
                .equals((Alias::new(current_table), Alias::new(rel.local.as_str()))),
            );
        }
        Some(j) => {
            // M:M: junction.local = current.local, junction.foreign = target.foreign
            sub.from(Alias::new(j.table.as_str()));
            sub.inner_join(
                Alias::new(rel.target_table.as_str()),
                Expr::col((Alias::new(j.table.as_str()), Alias::new(j.foreign.as_str()))).equals((
                    Alias::new(rel.target_table.as_str()),
                    Alias::new(rel.foreign.as_str()),
                )),
            );
            where_cond = where_cond.add(
                Expr::col((Alias::new(j.table.as_str()), Alias::new(j.local.as_str())))
                    .equals((Alias::new(current_table), Alias::new(rel.local.as_str()))),
            );
        }
    }
    if let Some(e) = inner {
        where_cond = where_cond.add(e);
    }
    sub.cond_where(where_cond);
    Ok(sub)
}

fn compare_expr(field: &FieldRef, op: CmpOp, value: &Value) -> Result<SimpleExpr, QueryError> {
    let col = col_expr(field);
    let v = to_sea_value(value)?;
    Ok(match op {
        CmpOp::Eq => col.eq(v),
        CmpOp::Ne => col.ne(v),
        CmpOp::Lt => col.lt(v),
        CmpOp::Le => col.lte(v),
        CmpOp::Gt => col.gt(v),
        CmpOp::Ge => col.gte(v),
    })
}

fn in_expr(field: &FieldRef, values: &[Value], negated: bool) -> Result<SimpleExpr, QueryError> {
    let col = col_expr(field);
    let vals: Vec<SeaValue> = values.iter().map(to_sea_value).collect::<Result<_, _>>()?;
    Ok(if negated {
        col.is_not_in(vals)
    } else {
        col.is_in(vals)
    })
}

fn between_expr(
    field: &FieldRef,
    low: &Value,
    high: &Value,
    low_incl: bool,
    high_incl: bool,
    negated: bool,
) -> Result<SimpleExpr, QueryError> {
    let lo = to_sea_value(low)?;
    let hi = to_sea_value(high)?;
    // Native BETWEEN is inclusive-only; use it only when both bounds are
    // inclusive, else render explicit per-bound comparisons (the
    // chained-range rule).
    let e = if low_incl && high_incl {
        col_expr(field).between(lo, hi)
    } else {
        let lo_e = if low_incl {
            col_expr(field).gte(lo)
        } else {
            col_expr(field).gt(lo)
        };
        let hi_e = if high_incl {
            col_expr(field).lte(hi)
        } else {
            col_expr(field).lt(hi)
        };
        lo_e.and(hi_e)
    };
    Ok(if negated { e.not() } else { e })
}

fn text_expr(field: &FieldRef, op: TextOp, pattern: &str) -> SimpleExpr {
    let escaped = escape_like(pattern);
    let like = match op {
        TextOp::StartsWith => format!("{escaped}%"),
        TextOp::EndsWith => format!("%{escaped}"),
        TextOp::Contains => format!("%{escaped}%"),
    };
    // W13: `LIKE` with `\` as the escape character. Case sensitivity is the one
    // thing the dialect does **not** normalise: it is a property of the stored
    // data (a SQL collation, an ES analyzer), not of the query, and no query-time
    // flag can restore case-sensitive matching against an Elasticsearch `text`
    // field whose analyzer already folded the tokens. So the per-backend truth is
    // stated in the parity table of `docs/src/reference/data-dialect.md`
    // ("Text-match case sensitivity") rather than papered over here: PostgreSQL
    // `LIKE` is case-sensitive, SQLite's folds ASCII, MySQL's follows the
    // column's collation (`_ci` by default).
    col_expr(field).like(LikeExpr::new(like).escape('\\'))
}

/// Escape the LIKE metacharacters in user-provided text so they match literally.
fn escape_like(pattern: &str) -> String {
    pattern
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Apply the shared sort plan ([`super::plan_sort`], which owns the W8
/// null-ordering rule) to anything that takes an `ORDER BY` — the statement
/// itself, or the `OVER (…)` clause of the include's window (F27).
///
/// Only MySQL is special-cased here: it has no `NULLS FIRST`/`NULLS LAST`
/// syntax and needs none, because its native ordering already places nulls
/// first on ASC and last on DESC — exactly the planned placement. SQLite's
/// native order agrees too but accepts the clause, so it gets it, redundantly
/// but harmlessly — one rendering path rather than a second exception to keep
/// in step. PostgreSQL (which sorts nulls as the largest value) is the backend
/// the explicit clause exists for.
fn apply_sort_keys<S: OrderedStatement>(stmt: &mut S, sort: &[SortKey], dialect: SqlDialect) {
    for p in super::plan_sort(sort) {
        let order = if p.ascending { Order::Asc } else { Order::Desc };
        let nulls = if p.nulls_first {
            NullOrdering::First
        } else {
            NullOrdering::Last
        };
        match dialect {
            SqlDialect::Mysql => {
                stmt.order_by(Alias::new(p.field), order);
            }
            _ => {
                stmt.order_by_with_nulls(Alias::new(p.field), order, nulls);
            }
        }
    }
}

fn col_expr(field: &FieldRef) -> Expr {
    Expr::col(Alias::new(field.physical.as_str()))
}

/// Convert an IR value to a `sea_query::Value`, restricted to the variants the
/// `sqlx-any` binder accepts (Bool/BigInt/Double/String) — never Decimal/Json,
/// which panic under `AnyArguments`. The tagged BSON values (#263) have no SQL
/// binding and refuse with the dialect's standard capability error —
/// parity-or-error, never a bind that compares differently than on MongoDB.
fn to_sea_value(v: &Value) -> Result<SeaValue, QueryError> {
    Ok(match v {
        // A scalar null is already lowered to `IsNull`; this placeholder only
        // arises for an explicit null inside a list, binding as SQL NULL.
        Value::Null => SeaValue::String(None),
        Value::Bool(b) => (*b).into(),
        Value::Int(i) => (*i).into(),
        Value::Float(f) => (*f).into(),
        Value::Str(s) => s.clone().into(),
        Value::ObjectId(_) => return Err(sql_unsupported("an ObjectId ($oid) value")),
        Value::DateTime(_) => return Err(sql_unsupported("a typed date ($date) value")),
    })
}

/// The capability error for a construct no SQL dialect can bind (all three
/// refuse identically, so the target is the backend family).
fn sql_unsupported(feature: &str) -> QueryError {
    QueryError::FeatureUnsupportedByTarget {
        feature: feature.to_string(),
        target: "sql".to_string(),
    }
}

// ---- Write rendering (INSERT / UPDATE / DELETE / upsert) ----

/// Render a resolved mutation into `(sql, values)` for `dialect`, bound to the
/// external connector's `AnyPool`. The `filter` of an update/delete reuses the
/// query dialect's [`render_expr`]; every value is a bound parameter.
pub fn render_write(
    w: &ResolvedWrite,
    dialect: SqlDialect,
) -> Result<(String, SqlxValues), WriteError> {
    // MySQL cannot express RETURNING; surface it rather than emitting invalid SQL.
    if !w.returning().is_empty() && dialect == SqlDialect::Mysql {
        return Err(WriteError::Query(QueryError::FeatureUnsupportedByTarget {
            feature: "returning".to_string(),
            target: "mysql".to_string(),
        }));
    }
    match w {
        ResolvedWrite::Insert {
            table,
            columns,
            rows,
            returning,
        } => render_insert(table, columns, rows, None, returning, dialect),
        ResolvedWrite::Upsert {
            table,
            columns,
            rows,
            set,
            conflict,
            returning,
        } => render_insert(
            table,
            columns,
            rows,
            Some((conflict, set)),
            returning,
            dialect,
        ),
        ResolvedWrite::Update {
            table,
            set,
            cond,
            returning,
        } => render_update(table, set, cond, returning, dialect),
        ResolvedWrite::Delete {
            table,
            cond,
            returning,
        } => render_delete(table, cond, returning, dialect),
    }
}

/// Render an INSERT, with `upsert` carrying the conflict clause and on-conflict
/// assignments when the mutation is an upsert.
fn render_insert(
    table: &str,
    columns: &[String],
    rows: &[Vec<Value>],
    upsert: Option<(&ResolvedConflict, &[(String, Value)])>,
    returning: &[String],
    dialect: SqlDialect,
) -> Result<(String, SqlxValues), WriteError> {
    let mut stmt = Query::insert();
    stmt.into_table(Alias::new(table));
    stmt.columns(columns.iter().map(|c| Alias::new(c.as_str())));
    for row in rows {
        let vals: Vec<SimpleExpr> = row
            .iter()
            .map(value_expr)
            .collect::<Result<_, _>>()
            .map_err(WriteError::Query)?;
        stmt.values(vals)
            .map_err(|e| WriteError::Query(QueryError::InvalidEnvelope(e.to_string())))?;
    }

    if let Some((c, set)) = upsert {
        let mut oc = OnConflict::columns(c.targets.iter().map(|t| Alias::new(t.as_str())));
        match c.action {
            ConflictAction::Nothing => {
                oc.do_nothing();
            }
            ConflictAction::Update => {
                if !set.is_empty() {
                    for (col, v) in set {
                        oc.value(
                            Alias::new(col.as_str()),
                            value_expr(v).map_err(WriteError::Query)?,
                        );
                    }
                } else {
                    // Default: overwrite every inserted column except the conflict keys.
                    let upd: Vec<Alias> = columns
                        .iter()
                        .filter(|c2| !c.targets.contains(c2))
                        .map(|c2| Alias::new(c2.as_str()))
                        .collect();
                    if upd.is_empty() {
                        oc.do_nothing();
                    } else {
                        oc.update_columns(upd);
                    }
                }
            }
        }
        stmt.on_conflict(oc);
    }

    if !returning.is_empty() {
        stmt.returning(
            Query::returning().columns(returning.iter().map(|c| Alias::new(c.as_str()))),
        );
    }
    Ok(build_write_for(dialect, &stmt))
}

fn render_update(
    table: &str,
    set: &[(String, Value)],
    cond: &Option<Cond>,
    returning: &[String],
    dialect: SqlDialect,
) -> Result<(String, SqlxValues), WriteError> {
    let mut stmt = Query::update();
    stmt.table(Alias::new(table));
    for (col, v) in set {
        stmt.value(
            Alias::new(col.as_str()),
            value_expr(v).map_err(WriteError::Query)?,
        );
    }
    if let Some(cond) = cond
        && !matches!(cond, Cond::True)
    {
        stmt.cond_where(render_expr(cond, table).map_err(WriteError::from)?);
    }
    if !returning.is_empty() {
        stmt.returning(
            Query::returning().columns(returning.iter().map(|c| Alias::new(c.as_str()))),
        );
    }
    Ok(build_write_for(dialect, &stmt))
}

fn render_delete(
    table: &str,
    cond: &Option<Cond>,
    returning: &[String],
    dialect: SqlDialect,
) -> Result<(String, SqlxValues), WriteError> {
    let mut stmt = Query::delete();
    stmt.from_table(Alias::new(table));
    if let Some(cond) = cond
        && !matches!(cond, Cond::True)
    {
        stmt.cond_where(render_expr(cond, table).map_err(WriteError::from)?);
    }
    if !returning.is_empty() {
        stmt.returning(
            Query::returning().columns(returning.iter().map(|c| Alias::new(c.as_str()))),
        );
    }
    Ok(build_write_for(dialect, &stmt))
}

/// An IR value as a bound `SimpleExpr` (a NULL binds as SQL NULL).
fn value_expr(v: &Value) -> Result<SimpleExpr, QueryError> {
    Ok(Expr::val(to_sea_value(v)?))
}

/// Dialect-specific `(sql, values)` for any write statement (Insert/Update/Delete).
fn build_write_for<S: SqlxBinder>(dialect: SqlDialect, stmt: &S) -> (String, SqlxValues) {
    match dialect {
        SqlDialect::Postgres => stmt.build_sqlx(PostgresQueryBuilder),
        SqlDialect::Mysql => stmt.build_sqlx(MysqlQueryBuilder),
        SqlDialect::Sqlite => stmt.build_sqlx(SqliteQueryBuilder),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{EntityRegistry, plan_sql};
    use serde_json::{Value as Json, json};

    /// The default page bounds used by these goldens (limit 100/1000).
    fn limits() -> QueryConfig {
        QueryConfig::default()
    }

    /// Plan `query` through the production entry point ([`plan_sql`]) and
    /// return the main `SelectStatement`, so the goldens cannot drift from
    /// the path the handler actually takes (W23).
    fn plan_stmt(
        query: &Json,
        reg: &EntityRegistry,
        dialect: SqlDialect,
        limits: &QueryConfig,
    ) -> Result<SelectStatement, QueryError> {
        plan_sql(query, &serde_json::Map::new(), reg, dialect, limits).map(|plan| plan.main)
    }

    /// Render `query` for `dialect` with values inlined, for golden assertions.
    fn sql_for(query: Json, dialect: SqlDialect) -> String {
        let stmt = plan_stmt(&query, &EntityRegistry::identity(), dialect, &limits())
            .expect("translation should succeed");
        match dialect {
            SqlDialect::Sqlite => stmt.to_string(SqliteQueryBuilder),
            SqlDialect::Postgres => stmt.to_string(PostgresQueryBuilder),
            SqlDialect::Mysql => stmt.to_string(MysqlQueryBuilder),
        }
    }

    fn sqlite(query: Json) -> String {
        sql_for(query, SqlDialect::Sqlite)
    }

    /// #263: the tagged BSON values (`$oid`/`$date`) have no SQL binding and
    /// refuse with the standard capability error on every dialect —
    /// parity-or-error, never a bind that compares differently than on Mongo.
    #[test]
    fn tagged_values_are_a_capability_error_on_every_sql_dialect() {
        for dialect in [SqlDialect::Sqlite, SqlDialect::Postgres, SqlDialect::Mysql] {
            let err = plan_stmt(
                &json!({
                    "source": "t",
                    "filter": { "==": [{"field": "ref"}, { "$oid": "665f1f77bcf86cd799439011" }] }
                }),
                &EntityRegistry::identity(),
                dialect,
                &limits(),
            )
            .expect_err("no SQL binding for an ObjectId");
            assert!(
                matches!(&err, QueryError::FeatureUnsupportedByTarget { target, .. } if target == "sql"),
                "{err:?}"
            );
            assert!(err.to_string().contains("$oid"), "{err}");
        }
    }

    /// users→orders (has_many) and users↔tags (many-to-many via user_tags).
    fn rel_schema() -> EntityRegistry {
        EntityRegistry::from_json(&json!({
            // These tests are about relation *rendering*, not the allowlist:
            // they declare relations and let the inner predicate's columns
            // resolve by identity, which F24 now makes an explicit opt-in.
            "unmapped": "identity",
            "entities": {
                "users": {
                    "relations": {
                        "orders": { "to": "orders", "kind": "has_many", "local": "id", "foreign": "user_id" },
                        "tags": {
                            "to": "tags", "kind": "many_to_many", "local": "id", "foreign": "id",
                            "through": { "table": "user_tags", "local": "user_id", "foreign": "tag_id" }
                        }
                    }
                }
            }
        }))
        .expect("valid schema")
    }

    fn sqlite_schema(query: Json) -> String {
        let stmt = plan_stmt(&query, &rel_schema(), SqlDialect::Sqlite, &limits())
            .expect("translation should succeed");
        stmt.to_string(SqliteQueryBuilder)
    }

    #[test]
    fn test_select_all_no_filter() {
        let sql = sqlite(json!({ "source": "users" }));
        assert_eq!(sql, r#"SELECT * FROM "users" LIMIT 100"#);
    }

    #[test]
    fn test_projection_and_comparison() {
        let sql = sqlite(json!({
            "source": "users",
            "fields": ["id", "name"],
            "filter": { ">": [{"field": "age"}, 18] }
        }));
        assert_eq!(
            sql,
            r#"SELECT "id", "name" FROM "users" WHERE "age" > 18 LIMIT 100"#
        );
    }

    #[test]
    fn test_and_or() {
        let sql = sqlite(json!({
            "source": "t",
            "filter": { "and": [
                { "==": [{"field": "a"}, 1] },
                { "or": [ { "==": [{"field": "b"}, 2] }, { "==": [{"field": "c"}, 3] } ] }
            ] }
        }));
        assert_eq!(
            sql,
            r#"SELECT * FROM "t" WHERE "a" = 1 AND ("b" = 2 OR "c" = 3) LIMIT 100"#
        );
    }

    #[test]
    fn test_membership() {
        let sql = sqlite(json!({
            "source": "t",
            "filter": { "in": [{"field": "status"}, ["a", "b"]] }
        }));
        assert_eq!(
            sql,
            r#"SELECT * FROM "t" WHERE "status" IN ('a', 'b') LIMIT 100"#
        );
    }

    #[test]
    fn test_empty_membership_is_false() {
        let sql = sqlite(json!({
            "source": "t",
            "filter": { "in": [{"field": "status"}, []] }
        }));
        assert_eq!(sql, r#"SELECT * FROM "t" WHERE 1 = 0 LIMIT 100"#);
    }

    #[test]
    fn test_is_null() {
        let sql = sqlite(json!({
            "source": "t",
            "filter": { "==": [{"field": "email"}, null] }
        }));
        assert_eq!(sql, r#"SELECT * FROM "t" WHERE "email" IS NULL LIMIT 100"#);
    }

    #[test]
    fn test_range_strict_is_not_between() {
        // Chained `<` is strict → explicit > AND <, NOT inclusive BETWEEN.
        let sql = sqlite(json!({
            "source": "t",
            "filter": { "<": [1, {"field": "x"}, 10] }
        }));
        assert_eq!(
            sql,
            r#"SELECT * FROM "t" WHERE "x" > 1 AND "x" < 10 LIMIT 100"#
        );
    }

    #[test]
    fn test_range_inclusive_is_between() {
        // Chained `<=` is inclusive → native BETWEEN.
        let sql = sqlite(json!({
            "source": "t",
            "filter": { "<=": [1, {"field": "x"}, 10] }
        }));
        assert_eq!(
            sql,
            r#"SELECT * FROM "t" WHERE "x" BETWEEN 1 AND 10 LIMIT 100"#
        );
    }

    #[test]
    fn test_text_contains_escapes_wildcards() {
        let sql = sqlite(json!({
            "source": "t",
            "filter": { "in": ["50%_off", {"field": "name"}] }
        }));
        assert_eq!(
            sql,
            r#"SELECT * FROM "t" WHERE "name" LIKE '%50\%\_off%' ESCAPE '\' LIMIT 100"#
        );
    }

    #[test]
    fn test_starts_with() {
        let sql = sqlite(json!({
            "source": "t",
            "filter": { "starts_with": [{"field": "name"}, "sm"] }
        }));
        assert_eq!(
            sql,
            r#"SELECT * FROM "t" WHERE "name" LIKE 'sm%' ESCAPE '\' LIMIT 100"#
        );
    }

    #[test]
    fn test_sort_and_paging() {
        let sql = sqlite(json!({
            "source": "t",
            "sort": [ { "created_at": "desc" } ],
            "limit": 20,
            "skip": 40
        }));
        assert_eq!(
            sql,
            r#"SELECT * FROM "t" ORDER BY "created_at" DESC NULLS LAST LIMIT 20 OFFSET 40"#
        );
    }

    /// W8: the shared rule is "a null sorts as the smallest value" — nulls
    /// first on `asc`, last on `desc`. It used to be the inverse, which no
    /// MongoDB `find` can express, so Mongo silently disagreed with SQL and ES
    /// on the ordering of every page containing a null.
    #[test]
    fn test_null_ordering_is_nulls_smallest() {
        assert_eq!(
            sqlite(json!({ "source": "t", "sort": [ { "name": "asc" } ] })),
            r#"SELECT * FROM "t" ORDER BY "name" ASC NULLS FIRST LIMIT 100"#
        );
        let stmt = plan_stmt(
            &json!({ "source": "t", "sort": [ { "name": "asc" } ] }),
            &EntityRegistry::identity(),
            SqlDialect::Postgres,
            &limits(),
        )
        .expect("ok");
        assert_eq!(
            stmt.to_string(PostgresQueryBuilder),
            r#"SELECT * FROM "t" ORDER BY "name" ASC NULLS FIRST LIMIT 100"#
        );
    }

    #[test]
    fn test_postgres_placeholders_via_build() {
        // Bound-parameter form (execution path): Postgres uses $1 placeholders.
        let stmt = plan_stmt(
            &json!({ "source": "users", "filter": { "==": [{"field": "id"}, 7] } }),
            &EntityRegistry::identity(),
            SqlDialect::Postgres,
            &limits(),
        )
        .expect("ok");
        let (sql, _values) = build_for(SqlDialect::Postgres, &stmt);
        // In the bound-parameter path, LIMIT is also a placeholder ($2).
        assert_eq!(sql, r#"SELECT * FROM "users" WHERE "id" = $1 LIMIT $2"#);
    }

    /// W8: MySQL has no `NULLS FIRST/LAST` clause and now needs none — its
    /// native ordering already puts nulls first on `ASC` and last on `DESC`,
    /// which is the rule. The `IS NULL` prefix key this replaced was a second,
    /// invisible sort key emitted to emulate the old inverse rule.
    #[test]
    fn test_mysql_needs_no_null_ordering_emulation() {
        let stmt = plan_stmt(
            &json!({ "source": "t", "sort": [ { "name": "asc" } ] }),
            &EntityRegistry::identity(),
            SqlDialect::Mysql,
            &limits(),
        )
        .expect("ok");
        assert_eq!(
            stmt.to_string(MysqlQueryBuilder),
            "SELECT * FROM `t` ORDER BY `name` ASC LIMIT 100"
        );
    }

    #[test]
    fn test_limit_default_applied() {
        let stmt = plan_stmt(
            &json!({ "source": "t" }),
            &EntityRegistry::identity(),
            SqlDialect::Sqlite,
            &QueryConfig {
                default_limit: 50,
                ..QueryConfig::default()
            },
        )
        .expect("ok");
        assert_eq!(
            stmt.to_string(SqliteQueryBuilder),
            r#"SELECT * FROM "t" LIMIT 50"#
        );
    }

    #[test]
    fn test_limit_exceeds_max_rejected() {
        let err = plan_stmt(
            &json!({ "source": "t", "limit": 5000 }),
            &EntityRegistry::identity(),
            SqlDialect::Sqlite,
            &limits(),
        )
        .expect_err("over the cap");
        assert!(matches!(
            err,
            QueryError::LimitExceeded {
                requested: 5000,
                max: 1000
            }
        ));
    }

    /// W12: `skip` is bounded like `limit` — rejected over the cap, never
    /// clamped. The cap used to exist only on Elasticsearch.
    #[test]
    fn test_skip_exceeds_max_rejected() {
        let err = plan_stmt(
            &json!({ "source": "t", "skip": 51 }),
            &EntityRegistry::identity(),
            SqlDialect::Sqlite,
            &QueryConfig {
                max_skip: 50,
                ..QueryConfig::default()
            },
        )
        .expect_err("over the skip cap");
        assert!(matches!(
            err,
            QueryError::SkipExceeded {
                requested: 51,
                max: 50
            }
        ));
    }

    // ---- relations ----

    #[test]
    fn test_relation_some_exists() {
        let sql = sqlite_schema(json!({
            "source": "users",
            "filter": { "some": [{"field": "orders"}, {">": [{"field": "total"}, 100]}] }
        }));
        assert_eq!(
            sql,
            r#"SELECT * FROM "users" WHERE EXISTS(SELECT 1 FROM "orders" WHERE "orders"."user_id" = "users"."id" AND "total" > 100) LIMIT 100"#
        );
    }

    #[test]
    fn test_relation_none_not_exists() {
        let sql = sqlite_schema(json!({
            "source": "users",
            "filter": { "none": [{"field": "orders"}, {">": [{"field": "total"}, 100]}] }
        }));
        assert_eq!(
            sql,
            r#"SELECT * FROM "users" WHERE NOT EXISTS(SELECT 1 FROM "orders" WHERE "orders"."user_id" = "users"."id" AND "total" > 100) LIMIT 100"#
        );
    }

    #[test]
    fn test_relation_all_null_fix() {
        // all → EXISTS(rel) AND NOT EXISTS(rel WHERE NOT c OR c IS NULL)
        // (the `all` null rule)
        let sql = sqlite_schema(json!({
            "source": "users",
            "filter": { "all": [{"field": "orders"}, {">": [{"field": "total"}, 100]}] }
        }));
        assert_eq!(
            sql,
            r#"SELECT * FROM "users" WHERE EXISTS(SELECT 1 FROM "orders" WHERE "orders"."user_id" = "users"."id") AND (NOT EXISTS(SELECT 1 FROM "orders" WHERE "orders"."user_id" = "users"."id" AND ((NOT "total" > 100) OR ("total" > 100) IS NULL))) LIMIT 100"#
        );
    }

    #[test]
    fn test_relation_many_to_many_join() {
        let sql = sqlite_schema(json!({
            "source": "users",
            "filter": { "some": [{"field": "tags"}, {"==": [{"field": "label"}, "vip"]}] }
        }));
        assert_eq!(
            sql,
            r#"SELECT * FROM "users" WHERE EXISTS(SELECT 1 FROM "user_tags" INNER JOIN "tags" ON "user_tags"."tag_id" = "tags"."id" WHERE "user_tags"."user_id" = "users"."id" AND "label" = 'vip') LIMIT 100"#
        );
    }

    #[test]
    fn test_two_sibling_relations_are_independent() {
        // Two `some`s → two independent EXISTS, never a cross product.
        let sql = sqlite_schema(json!({
            "source": "users",
            "filter": { "and": [
                { "some": [{"field": "orders"}, {">": [{"field": "total"}, 100]}] },
                { "some": [{"field": "orders"}, {"==": [{"field": "user_id"}, 7]}] }
            ] }
        }));
        assert!(
            sql.matches("EXISTS(SELECT 1 FROM \"orders\"").count() == 2,
            "sql = {sql}"
        );
    }

    // ---- include planning ----

    /// Plan an include with the given selection, in identity mode over
    /// [`rel_schema`].
    fn plan_include(
        selection: Json,
        limits: &QueryConfig,
    ) -> Result<crate::query::SqlPlan, QueryError> {
        crate::query::plan_sql(
            &json!({
                "source": "users",
                "fields": ["name"],
                "include": { "orders": selection }
            }),
            &serde_json::Map::new(),
            &rel_schema(),
            SqlDialect::Sqlite,
            limits,
        )
    }

    #[test]
    fn test_plan_sql_augments_parent_key_and_plans_include() {
        let plan = plan_include(
            json!({ "fields": ["total"], "sort": [{ "id": "asc" }], "limit": 5 }),
            &limits(),
        )
        .expect("plan");

        // The parent key `id` is added to the main select (for grouping) and
        // marked for stripping from the output.
        assert_eq!(
            plan.main.to_string(SqliteQueryBuilder),
            r#"SELECT "name", "id" FROM "users" LIMIT 100"#
        );
        assert_eq!(plan.strip, vec!["id".to_string()]);
        assert_eq!(plan.includes.len(), 1);
        let inc = &plan.includes[0];
        assert_eq!(inc.field, "orders");
        assert_eq!(inc.target_table, "orders");
        assert_eq!(inc.local, "id");
        assert_eq!(inc.foreign, "user_id");
        assert_eq!(inc.limit, 5);
        assert_eq!(inc.sort.len(), 1);
    }

    /// F27: an include without a `limit` used to fetch *every* child of every
    /// parent on the page. It now takes the envelope's own page policy —
    /// `default_limit` per parent.
    #[test]
    fn test_include_without_limit_takes_the_default_page_size() {
        let plan = plan_include(json!({ "sort": [{ "id": "asc" }] }), &limits()).expect("plan");
        assert_eq!(plan.includes[0].limit, 100);
    }

    /// F27: and one over the cap is rejected, never clamped — the same rule the
    /// envelope's own `limit` gets.
    #[test]
    fn test_include_limit_over_the_cap_is_rejected() {
        let err = plan_include(
            json!({ "sort": [{ "id": "asc" }], "limit": 5000 }),
            &limits(),
        )
        .expect_err("over the cap");
        assert!(
            matches!(
                err,
                QueryError::LimitExceeded {
                    requested: 5000,
                    max: 1000
                }
            ),
            "{err}"
        );
    }

    /// F27: the per-parent page is cut in SQL by a partitioned `ROW_NUMBER()`,
    /// with the requested order key. The old query had neither `LIMIT` nor
    /// `ORDER BY`: it materialised every child of the whole parent page and the
    /// handler truncated an arbitrary — and run-to-run unstable — prefix.
    #[test]
    fn test_include_child_query_pages_per_parent_in_sql() {
        let plan = plan_include(
            json!({ "fields": ["total"], "sort": [{ "total": "desc" }], "limit": 2 }),
            &limits(),
        )
        .expect("plan");
        let keys = vec![SeaValue::from("u1"), SeaValue::from("u2")];
        let (sql, _v) = build_include_select(&plan.includes[0], &keys, SqlDialect::Sqlite);
        assert_eq!(
            sql,
            concat!(
                r#"SELECT "total", "user_id" FROM (SELECT "total", "user_id", "#,
                r#"ROW_NUMBER() OVER ( PARTITION BY "user_id" ORDER BY "total" DESC NULLS LAST ) "#,
                r#"AS "__orion_include_rank" FROM "orders" WHERE "user_id" IN (?, ?)) "#,
                r#"AS "__orion_include" WHERE "__orion_include_rank" <= ? "#,
                r#"ORDER BY "total" DESC NULLS LAST"#
            ),
            "sql = {sql}"
        );
    }

    /// The outer `ORDER BY` names columns of the *sub-select's output*, so a
    /// sort key that is not in `fields` has to be projected anyway — and then
    /// dropped from the response. Projecting only `fields` + the foreign key
    /// produced `ORDER BY "created_at"` over a subquery that emits neither:
    /// PostgreSQL `column "created_at" does not exist`, MySQL `Unknown column
    /// 'created_at' in 'order clause'`, and on SQLite no error at all — the
    /// quoted name degrades to a string literal, so the clause is a constant and
    /// the children come back in window order. That is the undefined per-parent
    /// ordering F27 removes, reintroduced on the backend the default job runs.
    #[test]
    fn test_include_projects_a_sort_key_it_was_not_asked_for() {
        let plan = plan_include(
            json!({ "fields": ["total"], "sort": [{ "created_at": "desc" }], "limit": 5 }),
            &limits(),
        )
        .expect("plan");
        let inc = &plan.includes[0];
        assert_eq!(inc.projection(), ["total", "user_id", "created_at"]);
        // …and both plumbing columns come back out of the nested object.
        assert_eq!(inc.strip(), ["user_id", "created_at"]);

        let keys = vec![SeaValue::from("u1")];
        let (sql, _v) = build_include_select(inc, &keys, SqlDialect::Sqlite);
        assert_eq!(
            sql,
            concat!(
                r#"SELECT "total", "user_id", "created_at" FROM "#,
                r#"(SELECT "total", "user_id", "created_at", "#,
                r#"ROW_NUMBER() OVER ( PARTITION BY "user_id" ORDER BY "created_at" DESC NULLS LAST ) "#,
                r#"AS "__orion_include_rank" FROM "orders" WHERE "user_id" IN (?)) "#,
                r#"AS "__orion_include" WHERE "__orion_include_rank" <= ? "#,
                r#"ORDER BY "created_at" DESC NULLS LAST"#
            ),
            "sql = {sql}"
        );
    }

    /// A sort key that *is* projected is not duplicated, and the foreign key
    /// named in `fields` stays in the output.
    #[test]
    fn test_include_projection_does_not_duplicate_or_over_strip() {
        let plan = plan_include(
            json!({ "fields": ["total", "user_id"], "sort": [{ "total": "asc" }] }),
            &limits(),
        )
        .expect("plan");
        let inc = &plan.includes[0];
        assert_eq!(inc.projection(), ["total", "user_id"]);
        assert!(inc.strip().is_empty(), "strip = {:?}", inc.strip());
    }

    /// With no `fields` the sub-select is `SELECT *`, which already carries the
    /// foreign key and every sort key — nothing extra to project or strip.
    #[test]
    fn test_include_without_fields_projects_everything() {
        let plan =
            plan_include(json!({ "sort": [{ "created_at": "desc" }] }), &limits()).expect("plan");
        let inc = &plan.includes[0];
        assert!(inc.projection().is_empty());
        assert!(inc.strip().is_empty());
        let keys = vec![SeaValue::from("u1")];
        let (sql, _v) = build_include_select(inc, &keys, SqlDialect::Sqlite);
        assert!(
            sql.starts_with(r#"SELECT * FROM (SELECT *, ROW_NUMBER() OVER ("#),
            "sql = {sql}"
        );
    }

    /// MySQL cannot take a `NULLS …` clause anywhere, including inside `OVER`.
    #[test]
    fn test_include_child_query_renders_for_mysql() {
        let plan = plan_include(
            json!({ "fields": ["total"], "sort": [{ "total": "asc" }], "limit": 2 }),
            &limits(),
        )
        .expect("plan");
        let keys = vec![SeaValue::from("u1")];
        let (sql, _v) = build_include_select(&plan.includes[0], &keys, SqlDialect::Mysql);
        assert!(!sql.contains("NULLS"), "sql = {sql}");
        assert!(
            sql.contains("ROW_NUMBER() OVER ( PARTITION BY `user_id` ORDER BY `total` ASC )"),
            "sql = {sql}"
        );
    }

    /// F27: without an order key "the first `n` children" is not a defined
    /// answer, so the envelope must name one.
    #[test]
    fn test_include_without_sort_is_rejected() {
        let err = plan_include(json!({ "limit": 5 }), &limits()).expect_err("no order key");
        assert!(matches!(err, QueryError::InvalidEnvelope(_)), "{err}");
        assert!(err.to_string().contains("include.orders"), "{err}");
        assert!(err.to_string().contains("sort"), "{err}");
    }

    #[test]
    fn test_m2m_include_rejected() {
        let err = crate::query::plan_sql(
            &json!({ "source": "users", "include": { "tags": { "sort": [{ "id": "asc" }] } } }),
            &serde_json::Map::new(),
            &rel_schema(),
            SqlDialect::Sqlite,
            &limits(),
        )
        .expect_err("m2m include not supported");
        assert!(matches!(err, QueryError::FeatureUnsupportedByTarget { .. }));
    }

    // ---- Write rendering (INSERT / UPDATE / DELETE / upsert) ----

    /// A config that lets every envelope shape through — the guards have
    /// their own tests in `write.rs`.
    fn permissive_writes() -> crate::config::WriteConfig {
        crate::config::WriteConfig {
            max_rows: 1000,
            allow_unfiltered: true,
        }
    }

    /// Resolve `input` (identity mode) and render the write SQL for `dialect`.
    fn write_sql(input: Json, dialect: SqlDialect) -> String {
        let resolved = crate::query::write::resolve_write(
            &input,
            &serde_json::Map::new(),
            &EntityRegistry::identity(),
            &permissive_writes(),
        )
        .expect("resolve_write should succeed");
        let (sql, _values) = render_write(&resolved, dialect).expect("render should succeed");
        sql
    }

    fn sqlite_w(input: Json) -> String {
        write_sql(input, SqlDialect::Sqlite)
    }

    #[test]
    fn test_insert_single() {
        let sql = sqlite_w(json!({
            "op": "insert",
            "target": "users",
            "values": { "id": "u1", "name": "Alice" }
        }));
        assert_eq!(sql, r#"INSERT INTO "users" ("id", "name") VALUES (?, ?)"#);
    }

    #[test]
    fn test_insert_bulk() {
        let sql = sqlite_w(json!({
            "op": "insert",
            "target": "users",
            "values": [ { "id": "u1", "name": "Alice" }, { "id": "u2", "name": "Bob" } ]
        }));
        assert_eq!(
            sql,
            r#"INSERT INTO "users" ("id", "name") VALUES (?, ?), (?, ?)"#
        );
    }

    #[test]
    fn test_insert_returning() {
        let sql = sqlite_w(json!({
            "op": "insert",
            "target": "users",
            "values": { "name": "Alice" },
            "returning": ["id", "name"]
        }));
        assert_eq!(
            sql,
            r#"INSERT INTO "users" ("name") VALUES (?) RETURNING "id", "name""#
        );
    }

    #[test]
    fn test_update_with_filter() {
        let sql = sqlite_w(json!({
            "op": "update",
            "target": "users",
            "set": { "status": "inactive" },
            "filter": { "==": [{ "field": "id" }, "u1"] }
        }));
        assert_eq!(sql, r#"UPDATE "users" SET "status" = ? WHERE "id" = ?"#);
    }

    #[test]
    fn test_update_relation_filter_reuses_query_dialect() {
        // The update WHERE is the query dialect's filter, EXISTS subquery and all.
        let input = json!({
            "op": "update",
            "target": "users",
            "set": { "flagged": true },
            "filter": { "some": [{ "field": "orders" }, { ">": [{ "field": "total" }, 100] }] }
        });
        let resolved = crate::query::write::resolve_write(
            &input,
            &serde_json::Map::new(),
            &rel_schema(),
            &permissive_writes(),
        )
        .expect("resolve");
        let (sql, _v) = render_write(&resolved, SqlDialect::Sqlite).expect("render");
        // In the bound-parameter path the constant `1` in `SELECT 1` binds as `?`.
        assert_eq!(
            sql,
            r#"UPDATE "users" SET "flagged" = ? WHERE EXISTS(SELECT ? FROM "orders" WHERE "orders"."user_id" = "users"."id" AND "total" > ?)"#
        );
    }

    #[test]
    fn test_delete_with_filter() {
        let sql = sqlite_w(json!({
            "op": "delete",
            "target": "sessions",
            "filter": { "<": [{ "field": "age" }, 0] }
        }));
        assert_eq!(sql, r#"DELETE FROM "sessions" WHERE "age" < ?"#);
    }

    #[test]
    fn test_upsert_do_update() {
        let sql = sqlite_w(json!({
            "op": "upsert",
            "target": "users",
            "values": { "email": "a@x.io", "name": "Ada" },
            "on_conflict": { "target": ["email"], "action": "update" }
        }));
        assert_eq!(
            sql,
            r#"INSERT INTO "users" ("email", "name") VALUES (?, ?) ON CONFLICT ("email") DO UPDATE SET "name" = "excluded"."name""#
        );
    }

    #[test]
    fn test_upsert_do_nothing() {
        let sql = sqlite_w(json!({
            "op": "upsert",
            "target": "users",
            "values": { "email": "a@x.io", "name": "Ada" },
            "on_conflict": { "target": ["email"], "action": "nothing" }
        }));
        assert_eq!(
            sql,
            r#"INSERT INTO "users" ("email", "name") VALUES (?, ?) ON CONFLICT ("email") DO NOTHING"#
        );
    }

    #[test]
    fn test_returning_on_mysql_rejected() {
        let input = json!({
            "op": "insert",
            "target": "users",
            "values": { "name": "Ada" },
            "returning": ["id"]
        });
        let resolved = crate::query::write::resolve_write(
            &input,
            &serde_json::Map::new(),
            &EntityRegistry::identity(),
            &permissive_writes(),
        )
        .expect("resolve");
        let err = render_write(&resolved, SqlDialect::Mysql).expect_err("no RETURNING on MySQL");
        assert!(matches!(
            err,
            crate::query::write::WriteError::Query(QueryError::FeatureUnsupportedByTarget { .. })
        ));
    }
}

#[cfg(test)]
mod prop_tests {
    use super::*;
    use crate::config::WriteConfig;
    use crate::query::schema::EntityRegistry;
    use crate::query::write::resolve_write;
    use proptest::prelude::*;
    use serde_json::json;

    fn permissive_writes() -> WriteConfig {
        WriteConfig {
            max_rows: 1000,
            allow_unfiltered: true,
        }
    }

    /// Render an update whose `set` values and `filter` comparison all carry
    /// `value`, plus an insert row carrying it — the three channels a user
    /// scalar travels through. Returns (sql texts, all bind values).
    fn rendered(value: &str, dialect: SqlDialect) -> (Vec<String>, Vec<sea_query::Value>) {
        let update = serde_json::json!({
            "op": "update",
            "target": "users",
            "set": { "name": value },
            "filter": { "==": [{"field": "name"}, value] }
        });
        let insert = serde_json::json!({
            "op": "insert",
            "target": "users",
            "values": { "name": value }
        });
        let mut sqls = Vec::new();
        let mut binds = Vec::new();
        for input in [update, insert] {
            let resolved = resolve_write(
                &input,
                &serde_json::Map::new(),
                &EntityRegistry::identity(),
                &permissive_writes(),
            )
            .expect("resolve");
            let (sql, values) = render_write(&resolved, dialect).expect("render");
            sqls.push(sql);
            binds.extend(values.0.0);
        }
        (sqls, binds)
    }

    /// Identifier candidates: arbitrary unicode, strings built around the
    /// quoting/escaping metacharacters, and plain well-formed names so the
    /// accept path stays exercised too.
    fn arb_ident() -> impl Strategy<Value = String> {
        prop_oneof![
            ".{0,12}",
            r#"[a-z]{0,3}["'`\\$.][a-z]{0,3}["'`\\$.]?"#,
            "[A-Za-z_][A-Za-z0-9_]{0,8}",
        ]
    }

    /// Whether `ident` could interfere with identifier quoting or change
    /// meaning on any backend — exactly the set `validate_identifier` rejects.
    fn is_hostile(ident: &str) -> bool {
        ident.is_empty()
            || ident.starts_with('$')
            || ident.contains('.')
            || ident
                .chars()
                .any(|c| c.is_control() || matches!(c, '"' | '\'' | '`' | '\\'))
    }

    /// The dialect's quoted form of an accepted identifier.
    fn quoted(ident: &str, dialect: SqlDialect) -> String {
        match dialect {
            SqlDialect::Mysql => format!("`{ident}`"),
            _ => format!("\"{ident}\""),
        }
    }

    proptest! {
        /// The injection-resistance invariant SECURITY.md puts in scope,
        /// stated as value-independence: the rendered SQL text is a function
        /// of the envelope SHAPE only. If any user scalar — quotes, SQL
        /// fragments, unicode, control bytes — ever leaked into the SQL
        /// string instead of the binds, the text would differ from the
        /// baseline rendering of the same shape.
        #[test]
        fn sql_text_is_independent_of_user_values(value in ".*") {
            for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite, SqlDialect::Mysql] {
                let (sqls, binds) = rendered(&value, dialect);
                let (baseline_sqls, _) = rendered("baseline", dialect);
                prop_assert_eq!(&sqls, &baseline_sqls, "dialect {:?}", dialect);
                // And the value must actually travel — via the binds.
                prop_assert!(
                    binds.iter().any(
                        |v| matches!(v, sea_query::Value::String(Some(s)) if s.as_str() == value)
                    ),
                    "value must appear among the binds for {:?}",
                    dialect
                );
            }
        }

        /// F25: identifiers cannot be bound parameters, so their safety must
        /// live at the resolution boundary (`validate_identifier`), not in
        /// sea-query's `Iden::quoted`. For an arbitrary identifier fed through
        /// the WRITE path's channels — insert's `target` and an inserted
        /// column, update's `set` column and a `returning` name — either
        /// resolution rejects it, or it was benign and renders inside dialect
        /// quotes it cannot close. (`on_conflict.target` is not fuzzed here;
        /// it crosses the same `resolve_write_column` boundary as `set`.)
        #[test]
        fn write_identifiers_are_rejected_or_safely_quoted(ident in arb_ident()) {
            let mut values = serde_json::Map::new();
            values.insert(ident.clone(), json!(1));
            // insert: the identifier as `target` and as an inserted column.
            let insert = json!({
                "op": "insert",
                "target": ident.clone(),
                "values": values.clone(),
            });
            // update: the identifier as a `set` column and a `returning` name
            // (`all` acknowledges the deliberately unfiltered mutation).
            let update = json!({
                "op": "update",
                "target": "users",
                "set": values,
                "returning": [ident.clone()],
                "all": true,
            });
            for input in [insert, update] {
                let resolved = resolve_write(
                    &input,
                    &serde_json::Map::new(),
                    &EntityRegistry::identity(),
                    &permissive_writes(),
                );
                match resolved {
                    Err(_) => {} // boundary rejection — the safe outcome
                    Ok(w) => {
                        prop_assert!(
                            !is_hostile(&ident),
                            "resolve_write accepted a hostile identifier {ident:?}"
                        );
                        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite, SqlDialect::Mysql] {
                            // MySQL refuses RETURNING by feature, not by
                            // identifier — drop it there so the SET-clause
                            // channel still renders.
                            let mut w = w.clone();
                            if dialect == SqlDialect::Mysql {
                                match &mut w {
                                    ResolvedWrite::Insert { returning, .. }
                                    | ResolvedWrite::Update { returning, .. }
                                    | ResolvedWrite::Delete { returning, .. }
                                    | ResolvedWrite::Upsert { returning, .. } => returning.clear(),
                                }
                            }
                            let (sql, _) = render_write(&w, dialect).expect("render");
                            prop_assert!(
                                sql.contains(&quoted(&ident, dialect)),
                                "identifier {ident:?} must render quoted in {sql:?}"
                            );
                        }
                    }
                }
            }
        }

        /// The same invariant for the READ path's identifier channels:
        /// `source`, `fields`, `sort`, and a `filter` field reference.
        #[test]
        fn query_identifiers_are_rejected_or_safely_quoted(ident in arb_ident()) {
            let mut sort_key = serde_json::Map::new();
            sort_key.insert(ident.clone(), json!("asc"));
            let query = json!({
                "source": ident.clone(),
                "fields": [ident.clone()],
                "sort": [sort_key],
                "filter": { "==": [{ "field": ident.clone() }, 1] },
            });
            for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite, SqlDialect::Mysql] {
                match crate::query::plan_sql(
                    &query,
                    &serde_json::Map::new(),
                    &EntityRegistry::identity(),
                    dialect,
                    &QueryConfig::default(),
                )
                .map(|plan| plan.main)
                {
                    Err(_) => {} // boundary rejection — the safe outcome
                    Ok(stmt) => {
                        prop_assert!(
                            !is_hostile(&ident),
                            "plan_sql accepted a hostile identifier {ident:?}"
                        );
                        let sql = match dialect {
                            SqlDialect::Sqlite => stmt.to_string(SqliteQueryBuilder),
                            SqlDialect::Postgres => stmt.to_string(PostgresQueryBuilder),
                            SqlDialect::Mysql => stmt.to_string(MysqlQueryBuilder),
                        };
                        prop_assert!(
                            sql.contains(&quoted(&ident, dialect)),
                            "identifier {ident:?} must render quoted in {sql:?}"
                        );
                    }
                }
            }
        }
    }
}
