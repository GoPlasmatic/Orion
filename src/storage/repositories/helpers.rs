use crate::errors::OrionError;
use crate::storage::{DbPool, DbTransaction};

/// Converts an `Option<&str>` to a `sea_query::Value::String`, mapping `None`
/// to a SQL NULL string.  Replaces the repetitive
/// `.as_ref().map(|s| s.as_str().into()).unwrap_or(sea_query::Value::String(None))`
/// pattern used throughout repository create/update methods.
pub fn optional_string_value(opt: Option<&str>) -> sea_query::Value {
    opt.map(|s| s.to_string().into())
        .unwrap_or(sea_query::Value::String(None))
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
pub async fn fetch_required<T>(
    pool: &DbPool,
    sql: &str,
    values: sea_query_binder::SqlxValues,
    err: impl FnOnce() -> OrionError,
) -> Result<T, OrionError>
where
    T: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> + Send + Unpin,
    T: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>,
    T: for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow>,
{
    pool.fetch_optional_as::<T>(sql, values)
        .await?
        .ok_or_else(err)
}

/// Count rows in `table` matching `cond`. Replaces the `Query::select() ...
/// .expr(Func::count(...)).from(table).cond_where(cond)` boilerplate used by
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

/// Ensure no row matches the given query; returns the `OrionError` from `err`
/// if a row is found. The inverse of [`fetch_required`] — used by
/// `create_new_version` paths to reject duplicate drafts.
pub async fn ensure_absent<T>(
    pool: &DbPool,
    sql: &str,
    values: sea_query_binder::SqlxValues,
    err: impl FnOnce() -> OrionError,
) -> Result<(), OrionError>
where
    T: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> + Send + Unpin,
    T: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>,
    T: for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow>,
{
    if pool.fetch_optional_as::<T>(sql, values).await?.is_some() {
        return Err(err());
    }
    Ok(())
}

/// Transaction-scoped variant of [`fetch_required`].
pub async fn fetch_required_tx<T>(
    tx: &mut DbTransaction,
    sql: &str,
    values: sea_query_binder::SqlxValues,
    err: impl FnOnce() -> OrionError,
) -> Result<T, OrionError>
where
    T: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> + Send + Unpin,
    T: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>,
    T: for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow>,
{
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
}
