use sea_query::{Asterisk, Condition, DynIden, Order, Query};
use serde::Serialize;

use crate::errors::OrionError;
use crate::storage::{DbPool, DbRow, DbTransaction};

/// One page of repository results plus the paging bookkeeping every admin
/// list endpoint returns. Shared by all repositories.
#[derive(Debug, Serialize)]
pub struct PaginatedResult<T> {
    pub data: Vec<T>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// Converts an `Option<&str>` to a `sea_query::Value::String`, mapping `None`
/// to a SQL NULL string.  Replaces the repetitive
/// `.as_ref().map(|s| s.as_str().into()).unwrap_or(sea_query::Value::String(None))`
/// pattern used throughout repository create/update methods.
pub fn optional_string_value(opt: Option<&str>) -> sea_query::Value {
    opt.map(|s| s.to_string().into())
        .unwrap_or(sea_query::Value::String(None))
}

/// Map a driver-level "this row already exists" error from a `create` to
/// `OrionError::Conflict` (409), leaving everything else a `Storage` error
/// (500). Two shapes count as a duplicate (D16):
///
/// - the structured `UniqueViolation` kind — Postgres always (its single-draft
///   rule is a partial unique index), and SQLite/MySQL primary-key collisions;
/// - the *generic* error the single-draft triggers raise — SQLite
///   `RAISE(ABORT, …)` and MySQL `SIGNAL SQLSTATE '45000'` carry no
///   constraint kind sqlx can classify, so those are matched on the trigger's
///   message text (`migrations/{sqlite,mysql}/…single_draft…`).
pub fn map_duplicate(e: sqlx::Error, conflict_msg: impl FnOnce() -> String) -> OrionError {
    match &e {
        sqlx::Error::Database(db_err)
            if db_err.kind() == sqlx::error::ErrorKind::UniqueViolation
                || db_err.message().contains("Only one draft version allowed") =>
        {
            OrionError::Conflict(conflict_msg())
        }
        _ => OrionError::Storage(e),
    }
}

/// The database's own current time, per backend. Lease and claim comparisons
/// use this (never node clocks) so every replica agrees with the DB.
/// SQLite `datetime('now')` and MySQL `UTC_TIMESTAMP()` are UTC; Postgres
/// `LOCALTIMESTAMP` matches the session timezone (UTC by convention here),
/// staying in timestamp-without-tz land like the stored columns.
pub fn sql_now(backend: crate::storage::DbBackend) -> &'static str {
    match backend {
        crate::storage::DbBackend::Sqlite => "datetime('now')",
        crate::storage::DbBackend::Postgres => "LOCALTIMESTAMP",
        crate::storage::DbBackend::Mysql => "UTC_TIMESTAMP()",
    }
}

/// `sql_now` plus a positive offset in seconds, per backend.
pub fn sql_now_plus_secs(backend: crate::storage::DbBackend, secs: u64) -> String {
    match backend {
        crate::storage::DbBackend::Sqlite => format!("datetime('now', '+{secs} seconds')"),
        crate::storage::DbBackend::Postgres => {
            format!("LOCALTIMESTAMP + interval '{secs} seconds'")
        }
        crate::storage::DbBackend::Mysql => {
            format!("DATE_ADD(UTC_TIMESTAMP(), INTERVAL {secs} SECOND)")
        }
    }
}

/// Fetch a single required row from the pool. Maps a missing row to the
/// `OrionError` returned by `err`. Replaces the
/// `.fetch_optional_as(...).await?.ok_or_else(...)` pattern repeated across
/// repository read paths.
pub async fn fetch_required<T: DbRow>(
    pool: &DbPool,
    sql: &str,
    values: sea_query_binder::SqlxValues,
    err: impl FnOnce() -> OrionError,
) -> Result<T, OrionError> {
    pool.fetch_optional_as::<T>(sql, values)
        .await?
        .ok_or_else(err)
}

/// Count rows in `table` matching `cond`. Replaces the `Query::select() ...
/// .expr(Func::count(...)).from(table.clone()).cond_where(cond)` boilerplate used by
/// every paginated list path.
pub async fn count_where<I>(
    pool: &DbPool,
    table: I,
    cond: sea_query::Condition,
) -> Result<i64, OrionError>
where
    I: sea_query::IntoTableRef,
{
    use sea_query::{Asterisk, Expr, Func, Query};
    let (sql, values) = crate::storage::build_sqlx(
        Query::select()
            .expr(Func::count(Expr::col(Asterisk)))
            .from(table)
            .cond_where(cond),
    );
    let (total,): (i64,) = pool.fetch_one_as::<(i64,)>(&sql, values).await?;
    Ok(total)
}

/// What a paginated read selects.
///
/// An explicit choice rather than an "empty means everything" convention: a
/// narrow projection is always deliberate, and saying so at the call site is
/// what keeps a withheld column withheld.
pub enum Projection {
    /// Every column — the row type's fields and the table's columns match.
    All,
    /// A named subset, for a row type deliberately narrower than its table.
    /// `traces` and `trace_dlq` both use this to keep request payloads — and,
    /// for traces, a capability-token hash — out of a list page (D27).
    Columns(Vec<DynIden>),
}

/// One page of a list query: which rows, in what order, and how many.
///
/// A struct rather than seven positional parameters, three of which are
/// `DynIden` and would transpose silently.
pub struct Page {
    /// Table or current-version view to read.
    pub from: DynIden,
    pub projection: Projection,
    /// Filter, applied identically to the count and the page.
    pub cond: Condition,
    pub sort: DynIden,
    pub order: Order,
    /// Already clamped by the caller — [`clamp_pagination`] for the filter
    /// DTOs that default an absent page size, or an explicit `clamp` where
    /// the values are not optional.
    pub limit: i64,
    pub offset: i64,
}

/// Count matching rows, then read one page of them.
///
/// The single implementation of count-then-page (D18). Seven call sites —
/// workflows, channels, version history, traces, the trace DLQ, audit logs and
/// connectors — each used to spell it out, so every pagination fix had to land
/// six more times than it should have. It lives here and not in `versioned`
/// because it never had anything to do with versioned entities.
///
/// `total` counts the rows matching `cond` across the whole table, ignoring
/// `limit` and `offset`, which is what the paginated envelope's `total` means.
pub async fn paginate<T: DbRow>(
    pool: &DbPool,
    page: Page,
) -> Result<PaginatedResult<T>, OrionError> {
    let total = count_where(pool, page.from.clone(), page.cond.clone()).await?;

    let (sql, values) = crate::storage::build_sqlx(&mut page_select(&page));
    let data = pool.fetch_all_as::<T>(&sql, values).await?;

    Ok(PaginatedResult {
        data,
        total,
        limit: page.limit,
        offset: page.offset,
    })
}

/// The `SELECT` half of [`paginate`], split out so a projection can be
/// asserted without a database — see `traces` and `trace_dlq`, whose list
/// queries must provably name no payload column.
pub(crate) fn page_select(page: &Page) -> sea_query::SelectStatement {
    let mut select = Query::select();
    match &page.projection {
        Projection::All => select.column(Asterisk),
        Projection::Columns(columns) => select.columns(columns.iter().cloned()),
    };
    select
        .from(page.from.clone())
        .cond_where(page.cond.clone())
        .order_by(page.sort.clone(), page.order.clone())
        .limit(page.limit as u64)
        .offset(page.offset as u64)
        .to_owned()
}

/// Ensure no row matches the given query; returns the `OrionError` from `err`
/// if a row is found. The inverse of [`fetch_required`] — used by
/// `create_new_version` paths to reject duplicate drafts.
pub async fn ensure_absent<T: DbRow>(
    pool: &DbPool,
    sql: &str,
    values: sea_query_binder::SqlxValues,
    err: impl FnOnce() -> OrionError,
) -> Result<(), OrionError> {
    if pool.fetch_optional_as::<T>(sql, values).await?.is_some() {
        return Err(err());
    }
    Ok(())
}

/// Transaction-scoped variant of [`fetch_required`].
pub async fn fetch_required_tx<T: DbRow>(
    tx: &mut DbTransaction,
    sql: &str,
    values: sea_query_binder::SqlxValues,
    err: impl FnOnce() -> OrionError,
) -> Result<T, OrionError> {
    tx.fetch_optional_as::<T>(sql, values)
        .await?
        .ok_or_else(err)
}

/// Normalises the `limit` / `offset` pagination parameters coming from filter
/// DTOs into clamped values safe for SQL queries.
///
/// - `limit`:  defaults to 50, clamped to [1, 1000]
/// - `offset`: defaults to 0, clamped to >= 0
pub fn clamp_pagination(limit: Option<i64>, offset: Option<i64>) -> (i64, i64) {
    let limit = limit.unwrap_or(50).clamp(1, 1000);
    let offset = offset.unwrap_or(0).max(0);
    (limit, offset)
}

/// Parses an optional sort-order string (`"asc"` or `"desc"`) into a
/// `sea_query::Order`.  Defaults to `Desc` when the value is `None` or any
/// unrecognised string.
pub fn parse_sort_order(sort_order: Option<&str>) -> sea_query::Order {
    match sort_order {
        Some("asc") => sea_query::Order::Asc,
        _ => sea_query::Order::Desc,
    }
}

/// Run an UPDATE that must yield one scalar: `RETURNING` on Postgres/SQLite;
/// on MySQL (no `UPDATE ... RETURNING`) execute the update and read the value
/// back inside the same transaction — the row lock makes the read-back ours.
pub async fn update_returning_scalar<C>(
    pool: &DbPool,
    update: &mut sea_query::UpdateStatement,
    returning: C,
    read_back: &mut sea_query::SelectStatement,
    missing: impl FnOnce() -> OrionError,
) -> Result<i64, OrionError>
where
    C: sea_query::IntoColumnRef,
{
    use crate::storage::{DbBackend, build_sqlx, get_backend};

    match get_backend() {
        DbBackend::Sqlite | DbBackend::Postgres => {
            update.returning(sea_query::Query::returning().column(returning));
            let (sql, values) = build_sqlx(update);
            pool.fetch_scalar::<i64>(&sql, values)
                .await
                .map_err(OrionError::Storage)
        }
        DbBackend::Mysql => {
            let mut tx = pool.begin_tx().await.map_err(OrionError::Storage)?;
            let (sql, values) = build_sqlx(update);
            tx.execute_query(&sql, values).await?;
            let (sql, values) = build_sqlx(read_back);
            let (value,): (i64,) = fetch_required_tx(&mut tx, &sql, values, missing).await?;
            tx.commit().await.map_err(OrionError::Storage)?;
            Ok(value)
        }
    }
}

/// INSERT that silently loses to an existing row: `ON CONFLICT DO NOTHING` on
/// Postgres/SQLite, `INSERT IGNORE` on MySQL. Returns rows affected (0 = an
/// existing row won).
pub async fn insert_if_absent<C>(
    pool: &DbPool,
    mut insert: sea_query::InsertStatement,
    conflict_col: C,
) -> Result<u64, OrionError>
where
    C: sea_query::IntoIden,
{
    use crate::storage::{DbBackend, build_sqlx, get_backend};

    match get_backend() {
        DbBackend::Sqlite | DbBackend::Postgres => {
            insert.on_conflict(
                sea_query::OnConflict::column(conflict_col)
                    .do_nothing()
                    .to_owned(),
            );
            let (sql, values) = build_sqlx(&mut insert);
            pool.execute_query(&sql, values)
                .await
                .map_err(OrionError::Storage)
        }
        DbBackend::Mysql => {
            // sea-query has no INSERT IGNORE rendering; patch the verb into
            // the rendered SQL rather than hand-rolling placeholders.
            let (sql, values) = build_sqlx(&mut insert);
            let sql = sql.replacen("INSERT INTO", "INSERT IGNORE INTO", 1);
            pool.execute_query(&sql, values)
                .await
                .map_err(OrionError::Storage)
        }
    }
}

/// Rows deleted per statement by [`delete_chunked`].
///
/// Small enough that one statement is short on any backend, large enough that
/// a big backlog drains in a sane number of round trips.
const DELETE_CHUNK_ROWS: u64 = 1_000;

/// Hard stop on chunks per call, so one tick cannot run unboundedly (D6).
///
/// At the default chunk size this is 5M rows per tick. Anything past it is
/// left for the next tick rather than held open — in cluster mode the job
/// lease expires `interval_secs + 60` after the tick starts, and a delete
/// still running past that lets a second node begin a duplicate.
const DELETE_MAX_CHUNKS: usize = 5_000;

/// Delete matching rows in bounded chunks rather than one unbounded statement.
///
/// Retention deletes used to be a single `DELETE … WHERE created_at < cutoff`
/// per tick. The first run after enabling retention is then one transaction
/// over potentially millions of rows: SQLite holds the write lock for its
/// whole duration, so every other writer hits the 5 s `busy_timeout` and
/// fails; Postgres bloats WAL and blocks autovacuum; MySQL can exceed
/// `innodb_lock_wait_timeout`.
///
/// The statement is
/// `DELETE FROM t WHERE id IN (SELECT id FROM (SELECT id FROM t WHERE … LIMIT n) AS d6_chunk)`.
/// The nested derived table looks redundant and is not: MySQL rejects a
/// subquery that selects from the table being deleted (error 1093) unless it
/// is materialised through one. SQLite and Postgres accept the same form, so
/// all three backends run identical SQL.
///
/// Yields to the runtime between chunks so a long drain cannot starve request
/// handling. Returns the total rows deleted.
pub async fn delete_chunked(
    pool: &DbPool,
    table: impl sea_query::IntoIden,
    id_column: impl sea_query::IntoIden,
    condition: sea_query::Condition,
) -> Result<u64, OrionError> {
    use sea_query::{Alias, Expr, Query};

    // `DynIden` so the statement can be rebuilt per chunk — the `Iden` derive
    // gives neither `Clone` nor `Copy`.
    let table = table.into_iden();
    let id_column = id_column.into_iden();

    let mut total = 0u64;
    for chunk in 0..DELETE_MAX_CHUNKS {
        let inner = Query::select()
            .column(id_column.clone())
            .from(table.clone())
            .cond_where(condition.clone())
            .limit(DELETE_CHUNK_ROWS)
            .to_owned();
        let materialised = Query::select()
            .column(id_column.clone())
            .from_subquery(inner, Alias::new("d6_chunk"))
            .to_owned();
        let (sql, values) = crate::storage::build_sqlx(
            Query::delete()
                .from_table(table.clone())
                .and_where(Expr::col(id_column.clone()).in_subquery(materialised)),
        );

        let deleted = pool.execute_query(&sql, values).await?;
        total += deleted;

        // A short chunk means the tail is gone; nothing left to loop for.
        if deleted < DELETE_CHUNK_ROWS {
            return Ok(total);
        }
        if chunk + 1 == DELETE_MAX_CHUNKS {
            tracing::warn!(
                deleted = total,
                "Retention delete hit its per-tick chunk cap; the remainder \
                 is left for the next tick"
            );
        }
        tokio::task::yield_now().await;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_string_some() {
        let v = optional_string_value(Some("hello"));
        assert_eq!(v, sea_query::Value::String(Some(Box::new("hello".into()))));
    }

    #[test]
    fn optional_string_none() {
        let v = optional_string_value(None);
        assert_eq!(v, sea_query::Value::String(None));
    }

    #[test]
    fn pagination_defaults() {
        assert_eq!(clamp_pagination(None, None), (50, 0));
    }

    #[test]
    fn pagination_clamps() {
        assert_eq!(clamp_pagination(Some(0), Some(-5)), (1, 0));
        assert_eq!(clamp_pagination(Some(9999), Some(10)), (1000, 10));
    }

    #[test]
    fn sort_order_asc() {
        assert!(matches!(
            parse_sort_order(Some("asc")),
            sea_query::Order::Asc
        ));
    }

    #[test]
    fn sort_order_desc() {
        assert!(matches!(
            parse_sort_order(Some("desc")),
            sea_query::Order::Desc
        ));
    }

    #[test]
    fn sort_order_none_defaults_desc() {
        assert!(matches!(parse_sort_order(None), sea_query::Order::Desc));
    }

    // -- D18: the one count-then-page shape --

    fn sample_page(projection: Projection) -> Page {
        use sea_query::IntoIden;
        Page {
            from: crate::storage::schema::Traces::Table.into_iden(),
            projection,
            cond: sea_query::Condition::all(),
            sort: crate::storage::schema::Traces::CreatedAt.into_iden(),
            order: Order::Desc,
            limit: 25,
            offset: 50,
        }
    }

    fn rendered(page: &Page) -> String {
        super::page_select(page).to_string(sea_query::SqliteQueryBuilder)
    }

    #[test]
    fn projection_all_selects_every_column() {
        let sql = rendered(&sample_page(Projection::All));
        assert!(sql.contains("SELECT *"), "{sql}");
    }

    /// The seam D27 turns on: a named projection must render exactly its
    /// columns, so a withheld one cannot travel from the database.
    #[test]
    fn projection_columns_names_only_what_it_was_given() {
        use sea_query::IntoIden;
        let sql = rendered(&sample_page(Projection::Columns(vec![
            crate::storage::schema::Traces::Id.into_iden(),
            crate::storage::schema::Traces::Status.into_iden(),
        ])));
        assert!(sql.contains(r#"SELECT "id", "status""#), "{sql}");
        assert!(!sql.contains('*'), "{sql}");
        assert!(!sql.contains("access_token_hash"), "{sql}");
    }

    /// Page bounds reach the statement, and an empty filter constrains
    /// nothing — the shape `connectors.list_paginated` relies on, having no
    /// filter at all.
    #[test]
    fn page_bounds_and_empty_filter_render_as_expected() {
        let sql = rendered(&sample_page(Projection::All));
        assert!(sql.contains("LIMIT 25"), "{sql}");
        assert!(sql.contains("OFFSET 50"), "{sql}");
        assert!(sql.contains(r#"ORDER BY "created_at" DESC"#), "{sql}");
        // sea-query renders an empty `Condition::all()` as no predicate; what
        // matters is that it names no column.
        assert!(
            !sql.contains(r#"WHERE "#) || sql.contains("WHERE TRUE"),
            "{sql}"
        );
    }
}
