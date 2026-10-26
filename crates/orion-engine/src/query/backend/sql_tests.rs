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

// -----------------------------------------------------------------
// `after` — keyset paging, rendered from the shared `Cond`
// -----------------------------------------------------------------

#[test]
fn test_after_on_an_ascending_key_is_one_comparison() {
    let sql = sqlite(json!({
        "source": "t", "sort": [{ "id": "asc" }], "after": { "id": 7 }, "limit": 20
    }));
    assert_eq!(
        sql,
        r#"SELECT * FROM "t" WHERE "id" > 7 ORDER BY "id" ASC NULLS FIRST LIMIT 20"#
    );
}

/// Nulls sort last on `desc` (W8), so the page after a real value still has
/// to reach them. Without the `IS NULL` arm the final page silently drops
/// every null-valued row.
#[test]
fn test_after_on_a_descending_key_reaches_the_nulls() {
    let sql = sqlite(json!({
        "source": "t", "sort": [{ "score": "desc" }], "after": { "score": 10 }
    }));
    assert_eq!(
        sql,
        r#"SELECT * FROM "t" WHERE "score" < 10 OR "score" IS NULL ORDER BY "score" DESC NULLS LAST LIMIT 100"#
    );
}

/// `score DESC, id ASC` — an ordinary leaderboard — has no row-value form
/// at all: `(a, b) < (x, y)` requires one direction for every key. The
/// OR-expansion is the general shape, and it is also the one MySQL turns
/// into an index range scan (see `storage::repositories::traces`).
#[test]
fn test_after_with_mixed_directions_is_an_or_expansion_not_a_row_comparison() {
    let sql = sqlite(json!({
        "source": "t",
        "sort": [{ "score": "desc" }, { "id": "asc" }],
        "after": { "score": 10, "id": 5 }
    }));
    assert_eq!(
        sql,
        r#"SELECT * FROM "t" WHERE "score" < 10 OR "score" IS NULL OR ("score" = 10 AND "id" > 5) ORDER BY "score" DESC NULLS LAST, "id" ASC NULLS FIRST LIMIT 100"#
    );
    assert!(!sql.contains(") < ("), "no row-value constructor: {sql}");
    // sea-query flattens the nested `Or` (disjunction is associative) and
    // keeps the tie-break `And` parenthesised, which is the grouping that
    // actually carries meaning here.
}

/// A filterless keyset page must render exactly as it did before `after`
/// existed — no `1 = 1 AND` prefix from an `And([True, seek])`.
#[test]
fn test_after_composes_with_a_filter_and_adds_nothing_without_one() {
    let sql = sqlite(json!({
        "source": "t", "sort": [{ "id": "asc" }], "after": { "id": 7 },
        "filter": { "==": [{ "field": "status" }, "active"] }
    }));
    assert_eq!(
        sql,
        r#"SELECT * FROM "t" WHERE "status" = 'active' AND "id" > 7 ORDER BY "id" ASC NULLS FIRST LIMIT 100"#
    );
    assert!(
        !sqlite(json!({
            "source": "t", "sort": [{ "id": "asc" }], "after": { "id": 7 }
        }))
        .contains("1 = 1")
    );
}

/// MySQL emits no `NULLS` clause (its native order already realises W8), so
/// its seek still has to carry the `IS NULL` arm itself.
#[test]
fn test_after_renders_on_every_dialect() {
    let query = json!({
        "source": "t", "sort": [{ "score": "desc" }], "after": { "score": 10 }
    });
    for dialect in [SqlDialect::Sqlite, SqlDialect::Postgres, SqlDialect::Mysql] {
        let sql = sql_for(query.clone(), dialect);
        assert!(sql.contains("< 10"), "{dialect:?}: {sql}");
        assert!(sql.contains("IS NULL"), "{dialect:?}: {sql}");
    }
}

/// The seek column and the `ORDER BY` column are one resolution: `after`
/// carries no names of its own, so a rename cannot move one without the
/// other.
#[test]
fn test_after_over_a_renamed_column_seeks_on_the_physical_name() {
    let reg = EntityRegistry::from_json(&json!({
        "entities": { "users": { "columns": {
            "handle": { "name": "nickname" }, "id": {} } } }
    }))
    .expect("schema parses");
    let stmt = plan_stmt(
        &json!({
            "source": "users", "fields": ["handle", "id"],
            "sort": [{ "handle": "asc" }], "after": { "handle": "cee" }
        }),
        &reg,
        SqlDialect::Sqlite,
        &limits(),
    )
    .expect("translates");
    let sql = stmt.to_string(SqliteQueryBuilder);
    assert!(sql.contains(r#""nickname" > 'cee'"#), "{sql}");
    assert!(sql.contains(r#"ORDER BY "nickname""#), "{sql}");
    assert!(!sql.contains("handle"), "{sql}");
}

/// Cursor values are caller data and go out bound, not inlined.
#[test]
fn test_after_values_are_bound_not_inlined() {
    let stmt = plan_stmt(
        &json!({ "source": "t", "sort": [{ "id": "asc" }], "after": { "id": 7 } }),
        &EntityRegistry::identity(),
        SqlDialect::Postgres,
        &limits(),
    )
    .expect("translates");
    let (sql, values) = build_for(SqlDialect::Postgres, &stmt);
    assert!(sql.contains("$1"), "{sql}");
    assert!(!values.0.0.is_empty(), "the cursor value is bound");
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

/// MySQL has no `DO NOTHING`, and sea-query's bare `do_nothing()` renders
/// `ON DUPLICATE KEY IGNORE` there — not MySQL syntax, so the server
/// refuses the statement and every `action: "nothing"` upsert against a
/// MySQL connector failed at first traffic. The polyfill's no-op assignment
/// is the valid spelling of the same intent.
/// `count` renders one aliased `COUNT(*)` and keeps the filter, on every
/// dialect — the alias is what lets the handler read one column name
/// instead of PostgreSQL's `count` and the other two's `COUNT(*)`.
#[test]
fn test_count_renders_on_every_dialect() {
    let query = json!({
        "source": "users",
        "count": true,
        "filter": { "==": [{ "field": "status" }, "active"] }
    });
    assert_eq!(
        sql_for(query.clone(), SqlDialect::Sqlite),
        r#"SELECT COUNT(*) AS "orion_count" FROM "users" WHERE "status" = 'active'"#
    );
    assert_eq!(
        sql_for(query.clone(), SqlDialect::Postgres),
        r#"SELECT COUNT(*) AS "orion_count" FROM "users" WHERE "status" = 'active'"#
    );
    assert_eq!(
        sql_for(query, SqlDialect::Mysql),
        "SELECT COUNT(*) AS `orion_count` FROM `users` WHERE `status` = 'active'"
    );
}

/// A filterless count still counts the table, with no `WHERE TRUE` and no
/// `LIMIT` — the page bounds shape a row set and there is no row set here.
#[test]
fn test_count_without_a_filter_has_no_where_or_limit() {
    let sql = sql_for(
        json!({ "source": "users", "count": true }),
        SqlDialect::Sqlite,
    );
    assert_eq!(sql, r#"SELECT COUNT(*) AS "orion_count" FROM "users""#);
}

#[test]
fn test_upsert_do_nothing_renders_on_every_dialect() {
    let input = json!({
        "op": "upsert",
        "target": "users",
        "values": { "email": "a@x.io", "name": "Ada" },
        "on_conflict": { "target": ["email"], "action": "nothing" }
    });
    assert_eq!(
        write_sql(input.clone(), SqlDialect::Mysql),
        "INSERT INTO `users` (`email`, `name`) VALUES (?, ?) \
             ON DUPLICATE KEY UPDATE `email` = `email`"
    );
    for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
        let sql = write_sql(input.clone(), dialect);
        assert!(
            sql.contains("DO NOTHING"),
            "{dialect:?} must keep DO NOTHING: {sql}"
        );
    }
}

/// An upsert whose inserted columns are *all* conflict targets has nothing
/// left to assign, so it collapses to the do-nothing clause — by the same
/// route, and so with the same MySQL hazard.
#[test]
fn test_upsert_with_no_updatable_column_renders_on_every_dialect() {
    let input = json!({
        "op": "upsert",
        "target": "tags",
        "values": { "a": 1, "b": 2 },
        "on_conflict": { "target": ["a", "b"], "action": "update" }
    });
    assert_eq!(
        write_sql(input.clone(), SqlDialect::Mysql),
        "INSERT INTO `tags` (`a`, `b`) VALUES (?, ?) \
             ON DUPLICATE KEY UPDATE `a` = `a`, `b` = `b`"
    );
    assert!(write_sql(input, SqlDialect::Postgres).contains("DO NOTHING"));
}

/// Every write op, rendered on all three dialects.
///
/// The write goldens above run through `sqlite_w()` almost exclusively, and
/// that is how a MySQL upsert spent its life emitting
/// `ON DUPLICATE KEY IGNORE` — a statement the server refuses — with a
/// green suite. The read goldens have looped over the dialects since 1.0;
/// this is the write half of the same guard.
///
/// `returning` is absent on purpose: MySQL refuses it, and that divergence
/// has its own test below.
#[test]
fn every_write_op_renders_on_every_dialect() {
    let cases: [(&str, Json, [&str; 3]); 5] = [
        (
            "insert",
            json!({ "op": "insert", "target": "users",
                        "values": { "id": "u1", "name": "Alice" } }),
            [
                r#"INSERT INTO "users" ("id", "name") VALUES (?, ?)"#,
                r#"INSERT INTO "users" ("id", "name") VALUES ($1, $2)"#,
                "INSERT INTO `users` (`id`, `name`) VALUES (?, ?)",
            ],
        ),
        (
            "insert_bulk",
            json!({ "op": "insert", "target": "users",
                        "values": [{ "id": "u1" }, { "id": "u2" }] }),
            [
                r#"INSERT INTO "users" ("id") VALUES (?), (?)"#,
                r#"INSERT INTO "users" ("id") VALUES ($1), ($2)"#,
                "INSERT INTO `users` (`id`) VALUES (?), (?)",
            ],
        ),
        (
            "update",
            json!({ "op": "update", "target": "users", "set": { "name": "Ada" },
                        "filter": { "==": [{ "field": "id" }, "u1"] } }),
            [
                r#"UPDATE "users" SET "name" = ? WHERE "id" = ?"#,
                r#"UPDATE "users" SET "name" = $1 WHERE "id" = $2"#,
                "UPDATE `users` SET `name` = ? WHERE `id` = ?",
            ],
        ),
        (
            "delete",
            json!({ "op": "delete", "target": "users",
                        "filter": { "==": [{ "field": "id" }, "u1"] } }),
            [
                r#"DELETE FROM "users" WHERE "id" = ?"#,
                r#"DELETE FROM "users" WHERE "id" = $1"#,
                "DELETE FROM `users` WHERE `id` = ?",
            ],
        ),
        (
            "upsert_do_update",
            json!({ "op": "upsert", "target": "users",
                        "values": { "email": "a@x.io", "name": "Ada" },
                        "on_conflict": { "target": ["email"], "action": "update" } }),
            [
                r#"INSERT INTO "users" ("email", "name") VALUES (?, ?) ON CONFLICT ("email") DO UPDATE SET "name" = "excluded"."name""#,
                r#"INSERT INTO "users" ("email", "name") VALUES ($1, $2) ON CONFLICT ("email") DO UPDATE SET "name" = "excluded"."name""#,
                // MySQL cannot name a conflict target — the upsert fires on
                // any unique index. Documented in the parity table.
                "INSERT INTO `users` (`email`, `name`) VALUES (?, ?) ON DUPLICATE KEY UPDATE `name` = VALUES(`name`)",
            ],
        ),
    ];

    for (name, envelope, expected) in cases {
        for (dialect, want) in [SqlDialect::Sqlite, SqlDialect::Postgres, SqlDialect::Mysql]
            .into_iter()
            .zip(expected)
        {
            assert_eq!(
                write_sql(envelope.clone(), dialect),
                want,
                "case '{name}' on {dialect:?}"
            );
        }
    }
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
