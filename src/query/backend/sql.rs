//! SQL rendering over sea-query.
//!
//! Walks a [`Cond`] + [`QuerySpec`] into a `sea_query::SelectStatement`, then
//! [`build_for`] produces the dialect-specific `(sql, values)` bound to the
//! external connector's `sqlx::AnyPool`. Identifiers are dynamic
//! (`sea_query::Alias`) and every literal is a bound value, so the output is
//! injection-safe and quoted per dialect.

use sea_query::{
    Alias, Asterisk, Condition, Expr, LikeExpr, MysqlQueryBuilder, NullOrdering, OnConflict, Order,
    PostgresQueryBuilder, Query, SelectStatement, SimpleExpr, SqliteQueryBuilder,
    Value as SeaValue,
};
use sea_query_binder::{SqlxBinder, SqlxValues};

use crate::query::backend::SqlDialect;
use crate::query::error::QueryError;
use crate::query::ir::{CmpOp, Cond, FieldRef, Quant, RelRef, TextOp, Value};
use crate::query::spec::{QuerySpec, SortDir, SortKey};
use crate::query::write::{ConflictAction, ResolvedWrite, WriteError, WriteOp};

/// Build a `SelectStatement` from the envelope and lowered condition, enforcing
/// the page-size bounds (`LimitExceeded` when `limit` > `max_limit`). `root_table`
/// is the physical table the query selects from (and the correlation base for any
/// relation subqueries).
pub fn render(
    spec: &QuerySpec,
    cond: &Cond,
    root_table: &str,
    dialect: SqlDialect,
    default_limit: u64,
    max_limit: u64,
) -> Result<SelectStatement, QueryError> {
    let limit = resolve_limit(spec.limit, default_limit, max_limit)?;

    let mut stmt = Query::select();
    if spec.fields.is_empty() {
        stmt.column(Asterisk);
    } else {
        for f in &spec.fields {
            stmt.column(Alias::new(f.as_str()));
        }
    }
    stmt.from(Alias::new(root_table));
    // Skip the WHERE clause entirely when the filter is unconditionally true, so
    // a filterless query does not render a redundant `WHERE TRUE`.
    if !matches!(cond, Cond::True) {
        stmt.cond_where(render_expr(cond, root_table)?);
    }
    apply_sort(&mut stmt, &spec.sort, dialect);
    stmt.limit(limit);
    if let Some(skip) = spec.skip {
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

/// Build the child query for an `include`: `SELECT <fields>, <foreign> FROM
/// <target> WHERE <foreign> IN (<keys>)`. The foreign key is always selected so
/// results can be grouped back to their parents. Callers must skip this when
/// `keys` is empty (an empty `IN` is not built here).
pub fn build_include_select(
    target_table: &str,
    foreign: &str,
    fields: &[String],
    keys: &[SeaValue],
    dialect: SqlDialect,
) -> (String, SqlxValues) {
    let mut stmt = Query::select();
    if fields.is_empty() {
        stmt.column(Asterisk);
    } else {
        for f in fields {
            stmt.column(Alias::new(f.as_str()));
        }
        if !fields.iter().any(|f| f == foreign) {
            stmt.column(Alias::new(foreign));
        }
    }
    stmt.from(Alias::new(target_table));
    stmt.cond_where(Expr::col(Alias::new(foreign)).is_in(keys.to_vec()));
    build_for(dialect, &stmt)
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

fn resolve_limit(
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

/// Render a `Cond` as a single boolean `SimpleExpr`. Composing on `SimpleExpr`
/// (rather than `Condition`) keeps `and`/`or`/`not`, EXISTS subqueries, and the
/// §5.6 `all` null-fix uniformly expressible. `current_table` is the physical
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
        Cond::Text {
            field,
            op,
            pattern,
            ci: _,
        } => text_expr(field, *op, pattern),
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
            //   EXISTS(rel) AND NOT EXISTS(rel WHERE NOT c OR c IS NULL)   (§5.6)
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
    let mut vals = Vec::with_capacity(values.len());
    for v in values {
        vals.push(to_sea_value(v)?);
    }
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
    // inclusive, else render explicit per-bound comparisons (§5.11).
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
    // Case-sensitive LIKE in Phase 1; the escaped user text uses `\` as the
    // escape char (note: MySQL's default collation is case-insensitive — §5.4).
    col_expr(field).like(LikeExpr::new(like).escape('\\'))
}

/// Escape the LIKE metacharacters in user-provided text so they match literally.
fn escape_like(pattern: &str) -> String {
    pattern
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn apply_sort(stmt: &mut SelectStatement, sort: &[SortKey], dialect: SqlDialect) {
    for k in sort {
        let (order, nulls) = match k.dir {
            // Documented default: nulls last on asc, nulls first on desc (§5.7).
            SortDir::Asc => (Order::Asc, NullOrdering::Last),
            SortDir::Desc => (Order::Desc, NullOrdering::First),
        };
        match dialect {
            SqlDialect::Mysql => {
                // MySQL has no NULLS FIRST/LAST; emulate with an `IS NULL` prefix
                // key that preserves the same default ordering.
                let is_null = Expr::col(Alias::new(k.field.as_str())).is_null();
                stmt.order_by_expr(is_null, order.clone());
                stmt.order_by(Alias::new(k.field.as_str()), order);
            }
            _ => {
                stmt.order_by_with_nulls(Alias::new(k.field.as_str()), order, nulls);
            }
        }
    }
}

fn col_expr(field: &FieldRef) -> Expr {
    Expr::col(Alias::new(field.physical.as_str()))
}

/// Convert an IR value to a `sea_query::Value`, restricted to the variants the
/// `sqlx-any` binder accepts (Bool/BigInt/Double/String) — never Decimal/Json,
/// which panic under `AnyArguments`.
fn to_sea_value(v: &Value) -> Result<SeaValue, QueryError> {
    Ok(match v {
        // A scalar null is already lowered to `IsNull`; this placeholder only
        // arises for an explicit null inside a list, binding as SQL NULL.
        Value::Null => SeaValue::String(None),
        Value::Bool(b) => (*b).into(),
        Value::Int(i) => (*i).into(),
        Value::Float(f) => (*f).into(),
        Value::Str(s) => s.clone().into(),
        Value::List(_) => {
            return Err(QueryError::NotRepresentable {
                what: "nested list literal".to_string(),
                at: "filter".to_string(),
            });
        }
    })
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
    if !w.returning.is_empty() && dialect == SqlDialect::Mysql {
        return Err(WriteError::FeatureUnsupportedByTarget {
            feature: "returning".to_string(),
            target: "mysql".to_string(),
        });
    }
    match w.op {
        WriteOp::Insert => render_insert(w, dialect, false),
        WriteOp::Upsert => render_insert(w, dialect, true),
        WriteOp::Update => render_update(w, dialect),
        WriteOp::Delete => render_delete(w, dialect),
    }
}

fn render_insert(
    w: &ResolvedWrite,
    dialect: SqlDialect,
    upsert: bool,
) -> Result<(String, SqlxValues), WriteError> {
    let mut stmt = Query::insert();
    stmt.into_table(Alias::new(w.table.as_str()));
    stmt.columns(w.columns.iter().map(|c| Alias::new(c.as_str())));
    for row in &w.rows {
        let vals: Vec<SimpleExpr> = row
            .iter()
            .map(value_expr)
            .collect::<Result<_, WriteError>>()?;
        stmt.values(vals)
            .map_err(|e| WriteError::InvalidEnvelope(e.to_string()))?;
    }

    if upsert {
        // `conflict` is guaranteed present for upsert (validated in `resolve_write`).
        let c = w
            .conflict
            .as_ref()
            .ok_or_else(|| WriteError::MissingField {
                field: "on_conflict".to_string(),
                op: "upsert".to_string(),
            })?;
        let mut oc = OnConflict::columns(c.targets.iter().map(|t| Alias::new(t.as_str())));
        match c.action {
            ConflictAction::Nothing => {
                oc.do_nothing();
            }
            ConflictAction::Update => {
                if !w.set.is_empty() {
                    for (col, v) in &w.set {
                        oc.value(Alias::new(col.as_str()), value_expr(v)?);
                    }
                } else {
                    // Default: overwrite every inserted column except the conflict keys.
                    let upd: Vec<Alias> = w
                        .columns
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

    if !w.returning.is_empty() {
        stmt.returning(
            Query::returning().columns(w.returning.iter().map(|c| Alias::new(c.as_str()))),
        );
    }
    Ok(build_write_for(dialect, &stmt))
}

fn render_update(
    w: &ResolvedWrite,
    dialect: SqlDialect,
) -> Result<(String, SqlxValues), WriteError> {
    let mut stmt = Query::update();
    stmt.table(Alias::new(w.table.as_str()));
    for (col, v) in &w.set {
        stmt.value(Alias::new(col.as_str()), value_expr(v)?);
    }
    if let Some(cond) = &w.cond
        && !matches!(cond, Cond::True)
    {
        stmt.cond_where(render_expr(cond, &w.table).map_err(WriteError::from)?);
    }
    if !w.returning.is_empty() {
        stmt.returning(
            Query::returning().columns(w.returning.iter().map(|c| Alias::new(c.as_str()))),
        );
    }
    Ok(build_write_for(dialect, &stmt))
}

fn render_delete(
    w: &ResolvedWrite,
    dialect: SqlDialect,
) -> Result<(String, SqlxValues), WriteError> {
    let mut stmt = Query::delete();
    stmt.from_table(Alias::new(w.table.as_str()));
    if let Some(cond) = &w.cond
        && !matches!(cond, Cond::True)
    {
        stmt.cond_where(render_expr(cond, &w.table).map_err(WriteError::from)?);
    }
    if !w.returning.is_empty() {
        stmt.returning(
            Query::returning().columns(w.returning.iter().map(|c| Alias::new(c.as_str()))),
        );
    }
    Ok(build_write_for(dialect, &stmt))
}

/// An IR value as a bound `SimpleExpr` (a NULL binds as SQL NULL).
fn value_expr(v: &Value) -> Result<SimpleExpr, WriteError> {
    let sv = to_sea_value(v).map_err(WriteError::from)?;
    Ok(Expr::val(sv).into())
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
    use crate::query::{EntityRegistry, translate_sql, translate_sql_with_schema};
    use serde_json::{Value as Json, json};

    /// Render `query` for `dialect` with values inlined, for golden assertions.
    fn sql_for(query: Json, dialect: SqlDialect) -> String {
        let stmt = translate_sql(&query, &serde_json::Map::new(), dialect, 100, 1000)
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

    /// users→orders (has_many) and users↔tags (many-to-many via user_tags).
    fn rel_schema() -> EntityRegistry {
        EntityRegistry::from_json(&json!({
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
        let stmt = translate_sql_with_schema(
            &query,
            &serde_json::Map::new(),
            &rel_schema(),
            SqlDialect::Sqlite,
            100,
            1000,
        )
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
        // Chained `<` is strict → explicit > AND <, NOT inclusive BETWEEN (§5.11).
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
            r#"SELECT * FROM "t" ORDER BY "created_at" DESC NULLS FIRST LIMIT 20 OFFSET 40"#
        );
    }

    #[test]
    fn test_postgres_placeholders_via_build() {
        // Bound-parameter form (execution path): Postgres uses $1 placeholders.
        let stmt = translate_sql(
            &json!({ "source": "users", "filter": { "==": [{"field": "id"}, 7] } }),
            &serde_json::Map::new(),
            SqlDialect::Postgres,
            100,
            1000,
        )
        .expect("ok");
        let (sql, _values) = build_for(SqlDialect::Postgres, &stmt);
        // In the bound-parameter path, LIMIT is also a placeholder ($2).
        assert_eq!(sql, r#"SELECT * FROM "users" WHERE "id" = $1 LIMIT $2"#);
    }

    #[test]
    fn test_mysql_null_ordering_emulation() {
        let stmt = translate_sql(
            &json!({ "source": "t", "sort": [ { "name": "asc" } ] }),
            &serde_json::Map::new(),
            SqlDialect::Mysql,
            100,
            1000,
        )
        .expect("ok");
        let sql = stmt.to_string(MysqlQueryBuilder);
        // nulls-last on asc via an `IS NULL` prefix key.
        assert_eq!(
            sql,
            "SELECT * FROM `t` ORDER BY `name` IS NULL ASC, `name` ASC LIMIT 100"
        );
    }

    #[test]
    fn test_limit_default_applied() {
        let stmt = translate_sql(
            &json!({ "source": "t" }),
            &serde_json::Map::new(),
            SqlDialect::Sqlite,
            50,
            1000,
        )
        .expect("ok");
        assert_eq!(
            stmt.to_string(SqliteQueryBuilder),
            r#"SELECT * FROM "t" LIMIT 50"#
        );
    }

    #[test]
    fn test_limit_exceeds_max_rejected() {
        let err = translate_sql(
            &json!({ "source": "t", "limit": 5000 }),
            &serde_json::Map::new(),
            SqlDialect::Sqlite,
            100,
            1000,
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

    // ---- Phase 2: relations ----

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
        // all → EXISTS(rel) AND NOT EXISTS(rel WHERE NOT c OR c IS NULL)  (§5.6)
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

    // ---- Phase 4: include planning ----

    #[test]
    fn test_plan_sql_augments_parent_key_and_plans_include() {
        let plan = crate::query::plan_sql(
            &json!({
                "source": "users",
                "fields": ["name"],
                "include": { "orders": { "fields": ["total"], "limit": 5 } }
            }),
            &serde_json::Map::new(),
            &rel_schema(),
            SqlDialect::Sqlite,
            100,
            1000,
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
        assert_eq!(inc.limit, Some(5));
    }

    #[test]
    fn test_include_child_query_selects_foreign_key() {
        let keys = vec![SeaValue::from("u1"), SeaValue::from("u2")];
        let (sql, _v) = build_include_select(
            "orders",
            "user_id",
            &["total".to_string()],
            &keys,
            SqlDialect::Sqlite,
        );
        // `user_id` is appended so children can be grouped by parent.
        assert!(
            sql.starts_with(r#"SELECT "total", "user_id" FROM "orders" WHERE "user_id" IN ("#),
            "sql = {sql}"
        );
    }

    #[test]
    fn test_m2m_include_rejected() {
        let err = crate::query::plan_sql(
            &json!({ "source": "users", "include": { "tags": {} } }),
            &serde_json::Map::new(),
            &rel_schema(),
            SqlDialect::Sqlite,
            100,
            1000,
        )
        .expect_err("m2m include not supported");
        assert!(matches!(err, QueryError::FeatureUnsupportedByTarget { .. }));
    }

    // ---- Write rendering (INSERT / UPDATE / DELETE / upsert) ----

    /// Resolve `input` (identity mode) and render the write SQL for `dialect`.
    fn write_sql(input: Json, dialect: SqlDialect) -> String {
        let resolved = crate::query::write::resolve_write(
            &input,
            &serde_json::Map::new(),
            &EntityRegistry::default(),
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
        let resolved =
            crate::query::write::resolve_write(&input, &serde_json::Map::new(), &rel_schema())
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
            &EntityRegistry::default(),
        )
        .expect("resolve");
        let err = render_write(&resolved, SqlDialect::Mysql).expect_err("no RETURNING on MySQL");
        assert!(matches!(
            err,
            crate::query::write::WriteError::FeatureUnsupportedByTarget { .. }
        ));
    }
}
