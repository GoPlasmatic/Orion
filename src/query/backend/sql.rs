//! SQL rendering over sea-query.
//!
//! Walks a [`Cond`] + [`QuerySpec`] into a `sea_query::SelectStatement`, then
//! [`build_for`] produces the dialect-specific `(sql, values)` bound to the
//! external connector's `sqlx::AnyPool`. Identifiers are dynamic
//! (`sea_query::Alias`) and every literal is a bound value, so the output is
//! injection-safe and quoted per dialect.

use sea_query::{
    Alias, Asterisk, Condition, Expr, LikeExpr, MysqlQueryBuilder, NullOrdering, Order,
    PostgresQueryBuilder, Query, SelectStatement, SimpleExpr, SqliteQueryBuilder,
    Value as SeaValue,
};
use sea_query_binder::{SqlxBinder, SqlxValues};

use crate::query::backend::SqlDialect;
use crate::query::error::QueryError;
use crate::query::ir::{CmpOp, Cond, FieldRef, TextOp, Value};
use crate::query::spec::{QuerySpec, SortDir, SortKey};

/// Build a `SelectStatement` from the envelope and lowered condition, enforcing
/// the page-size bounds (`LimitExceeded` when `limit` > `max_limit`).
pub fn render(
    spec: &QuerySpec,
    cond: &Cond,
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
    stmt.from(Alias::new(spec.source.as_str()));
    // Skip the WHERE clause entirely when the filter is unconditionally true, so
    // a filterless query does not render a redundant `WHERE TRUE`.
    if !matches!(cond, Cond::True) {
        stmt.cond_where(render_cond(cond)?);
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

fn render_cond(cond: &Cond) -> Result<Condition, QueryError> {
    Ok(match cond {
        Cond::True => Condition::all(),
        Cond::False => Condition::all().add(Expr::val(1).eq(0)),
        Cond::And(cs) => {
            let mut c = Condition::all();
            for x in cs {
                c = c.add(render_cond(x)?);
            }
            c
        }
        Cond::Or(cs) => {
            let mut c = Condition::any();
            for x in cs {
                c = c.add(render_cond(x)?);
            }
            c
        }
        Cond::Not(inner) => render_cond(inner)?.not(),
        Cond::Compare { field, op, value } => {
            Condition::all().add(compare_expr(field, *op, value)?)
        }
        Cond::In {
            field,
            values,
            negated,
        } => Condition::all().add(in_expr(field, values, *negated)?),
        Cond::IsNull { field, negated } => {
            let col = col_expr(field);
            let e = if *negated {
                col.is_not_null()
            } else {
                col.is_null()
            };
            Condition::all().add(e)
        }
        Cond::Between {
            field,
            low,
            high,
            low_incl,
            high_incl,
            negated,
        } => between_cond(field, low, high, *low_incl, *high_incl, *negated)?,
        Cond::Text {
            field,
            op,
            pattern,
            ci: _,
        } => Condition::all().add(text_expr(field, *op, pattern)),
    })
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

fn between_cond(
    field: &FieldRef,
    low: &Value,
    high: &Value,
    low_incl: bool,
    high_incl: bool,
    negated: bool,
) -> Result<Condition, QueryError> {
    let lo = to_sea_value(low)?;
    let hi = to_sea_value(high)?;
    // Native BETWEEN is inclusive-only; use it only when both bounds are
    // inclusive, else render explicit per-bound comparisons (§5.11).
    let cond = if low_incl && high_incl {
        Condition::all().add(col_expr(field).between(lo, hi))
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
        Condition::all().add(lo_e).add(hi_e)
    };
    Ok(if negated { cond.not() } else { cond })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::translate_sql;
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
}
