use sea_query::{Asterisk, Condition, DynIden, ExprTrait, Order, Query};
use serde::Serialize;

use crate::{DbPool, DbRow, DbTransaction};
use orion_common::errors::OrionError;

/// One page of repository results plus the paging bookkeeping every admin
/// list endpoint returns. Shared by all repositories.
#[derive(Debug, Serialize)]
pub struct PaginatedResult<T> {
    pub data: Vec<T>,
    /// `Some` only when the caller asked for the count; `None` is "not
    /// counted", never "no rows".
    pub total: Option<i64>,
    pub limit: i64,
    /// Where the page after this one starts; `None` on the last page.
    pub next_cursor: Option<String>,
}

/// The `(created_at, id)` of a row, which is its position in every list.
///
/// `created_at` alone is not unique (rows written in one second share it on
/// backends with second-precision defaults), so the id is the tie-break; without
/// it a page boundary inside a group of equal timestamps skips or repeats rows.
pub trait Keyed {
    fn created_at(&self) -> chrono::NaiveDateTime;
    fn id(&self) -> &str;
}

macro_rules! impl_keyed {
    ($($row:ty),+ $(,)?) => {$(
        impl Keyed for $row {
            fn created_at(&self) -> chrono::NaiveDateTime {
                self.created_at
            }
            fn id(&self) -> &str {
                &self.id
            }
        }
    )+};
}

impl_keyed!(
    crate::models::TraceListRow,
    crate::models::TraceDlqSummary,
    crate::models::AuditLogEntry,
    crate::models::CronOccurrence,
);

/// A position in a list: the `(created_at, id)` of the last row a caller saw.
///
/// Encoded as `<microseconds since the Unix epoch>.<id>`, URL-safe because every
/// id is a UUID or a slug. **Opaque to callers** — the encoding is not part of
/// the API contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeysetCursor {
    pub created_at: chrono::NaiveDateTime,
    pub id: String,
}

impl KeysetCursor {
    pub fn of(row: &impl Keyed) -> Self {
        Self {
            created_at: row.created_at(),
            id: row.id().to_string(),
        }
    }

    pub fn encode(&self) -> String {
        format!(
            "{}.{}",
            self.created_at.and_utc().timestamp_micros(),
            self.id
        )
    }

    pub fn decode(raw: &str) -> Result<Self, OrionError> {
        let invalid = || {
            OrionError::validation(
                "Invalid `cursor`: pass back a `next_cursor` from a previous page unmodified"
                    .to_string(),
            )
        };
        let (micros, id) = raw.split_once('.').ok_or_else(invalid)?;
        let micros: i64 = micros.parse().map_err(|_| invalid())?;
        let created_at = chrono::DateTime::from_timestamp_micros(micros)
            .ok_or_else(invalid)?
            .naive_utc();
        if id.is_empty() {
            return Err(invalid());
        }
        Ok(Self {
            created_at,
            id: id.to_string(),
        })
    }

    /// `(created_at, id)` strictly after this position in newest-first order —
    /// the row-value comparison spelled out, because MySQL does not optimise
    /// `(a, b) < (?, ?)` into an index range scan.
    fn condition(&self, created_at: &DynIden, id: &DynIden) -> Condition {
        use sea_query::Expr;
        Condition::any()
            .add(Expr::col(created_at.clone()).lt(self.created_at))
            .add(
                Condition::all()
                    .add(Expr::col(created_at.clone()).eq(self.created_at))
                    .add(Expr::col(id.clone()).lt(self.id.as_str())),
            )
    }
}

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
///
/// All three are UTC, matching the UTC-naive values Rust writes and the
/// columns' defaults. Postgres spells it out rather than trusting the session
/// zone (the pool pins it to UTC as well, but the SQL is what a replica using
/// another client must agree with).
pub fn sql_now(backend: crate::DbBackend) -> &'static str {
    match backend {
        crate::DbBackend::Sqlite => "datetime('now')",
        crate::DbBackend::Postgres => "(now() AT TIME ZONE 'UTC')",
        crate::DbBackend::Mysql => "UTC_TIMESTAMP()",
    }
}

/// What every UPDATE on a table with an `updated_at` column writes to it.
///
/// The statement sets it itself; there is no trigger behind it, because on
/// SQLite a per-table trigger is a second UPDATE per UPDATE.
/// `CURRENT_TIMESTAMP` renders the same on all three backends (SQLite's is the
/// text `datetime('now')` produces). Any new UPDATE on `traces`, `trace_dlq`
/// or a cron table must set it, or the row keeps its old time.
pub fn updated_now() -> sea_query::SimpleExpr {
    sea_query::Expr::cust("CURRENT_TIMESTAMP")
}

/// `sql_now` plus a positive offset in seconds, per backend.
pub fn sql_now_plus_secs(backend: crate::DbBackend, secs: u64) -> String {
    match backend {
        crate::DbBackend::Sqlite => format!("datetime('now', '+{secs} seconds')"),
        crate::DbBackend::Postgres => {
            format!("(now() AT TIME ZONE 'UTC') + interval '{secs} seconds'")
        }
        crate::DbBackend::Mysql => {
            format!("DATE_ADD(UTC_TIMESTAMP(), INTERVAL {secs} SECOND)")
        }
    }
}

/// `sql_now` minus a positive offset in seconds, per backend.
pub fn sql_now_minus_secs(backend: crate::DbBackend, secs: u64) -> String {
    match backend {
        crate::DbBackend::Sqlite => format!("datetime('now', '-{secs} seconds')"),
        crate::DbBackend::Postgres => {
            format!("(now() AT TIME ZONE 'UTC') - interval '{secs} seconds'")
        }
        crate::DbBackend::Mysql => {
            format!("DATE_SUB(UTC_TIMESTAMP(), INTERVAL {secs} SECOND)")
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
    values: sea_query_sqlx::SqlxValues,
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
    let (sql, values) = crate::build_sqlx(
        pool.backend(),
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
    /// for traces, a capability-token hash — out of a list page.
    Columns(Vec<DynIden>),
}

/// One page of a list query: which rows, newest first, from where.
///
/// A struct rather than positional parameters, several of which are `DynIden`
/// and would transpose silently.
pub struct Page {
    /// Table or current-version view to read.
    pub from: DynIden,
    pub projection: Projection,
    /// Filter, applied identically to the count and the page.
    pub cond: Condition,
    /// The ordering columns: `created_at` descending, then `id` descending.
    pub created_at: DynIden,
    pub id: DynIden,
    /// Already clamped by the caller with [`clamp_limit`].
    pub limit: i64,
    /// Resume after this position; `None` starts at the newest row.
    pub cursor: Option<KeysetCursor>,
    /// Count the filtered set (opt-in; see [`PaginatedResult::total`]).
    pub include_total: bool,
}

/// Read one page of a newest-first list.
///
/// The single implementation of paging for the trace list, trace DLQ, audit logs
/// and cron occurrences, so a pagination fix lands once. It reads `limit + 1`
/// rows: the extra one is how it knows a next page exists, so a list whose length
/// is an exact multiple of the page size does not end with an empty page.
pub async fn paginate<T: DbRow + Keyed>(
    pool: &DbPool,
    page: Page,
) -> Result<PaginatedResult<T>, OrionError> {
    let total = if page.include_total {
        Some(count_where(pool, page.from.clone(), page.cond.clone()).await?)
    } else {
        None
    };

    let (sql, values) = crate::build_sqlx(pool.backend(), &mut page_select(&page));
    let mut data = pool.fetch_all_as::<T>(&sql, values).await?;

    let limit = usize::try_from(page.limit).unwrap_or(usize::MAX);
    let next_cursor = if data.len() > limit {
        data.truncate(limit);
        data.last().map(|last| KeysetCursor::of(last).encode())
    } else {
        None
    };

    Ok(PaginatedResult {
        data,
        total,
        limit: page.limit,
        next_cursor,
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
    let mut cond = page.cond.clone();
    if let Some(cursor) = &page.cursor {
        cond = cond.add(cursor.condition(&page.created_at, &page.id));
    }
    select
        .from(page.from.clone())
        .cond_where(cond)
        .order_by(page.created_at.clone(), Order::Desc)
        .order_by(page.id.clone(), Order::Desc)
        // One more than the page: see [`paginate`].
        .limit(page.limit as u64 + 1)
        .to_owned()
}

/// Ensure no row matches the given query; returns the `OrionError` from `err`
/// if a row is found. The inverse of [`fetch_required`] — used by
/// `create_new_version` paths to reject duplicate drafts.
pub async fn ensure_absent<T: DbRow>(
    pool: &DbPool,
    sql: &str,
    values: sea_query_sqlx::SqlxValues,
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
    values: sea_query_sqlx::SqlxValues,
    err: impl FnOnce() -> OrionError,
) -> Result<T, OrionError> {
    tx.fetch_optional_as::<T>(sql, values)
        .await?
        .ok_or_else(err)
}

/// A page size: defaults to 50, clamped to [1, 1000].
pub fn clamp_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(50).clamp(1, 1000)
}

/// Decode an optional `cursor` query value.
pub fn parse_cursor(raw: Option<&str>) -> Result<Option<KeysetCursor>, OrionError> {
    raw.map(KeysetCursor::decode).transpose()
}

/// The `LIKE` pattern for "this JSON array of strings contains `tag`".
///
/// One implementation for the three tagged entities (workflows, channels,
/// connectors): the stored form is `serde_json::to_string(&Vec<String>)`, so
/// a member is always `"tag"` with the quotes, and matching `%"tag"%` cannot
/// hit a substring of a longer tag. LIKE wildcards in the tag itself are
/// escaped — and the escape character rides along as an explicit `ESCAPE`
/// clause, because SQLite (the default backend) has no default LIKE escape:
/// without it the backslashes are matched literally and a tag containing
/// `_`, `%` or `\` never matches its own stored value.
pub fn tag_like_pattern(tag: &str) -> sea_query::LikeExpr {
    let escaped = tag
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    sea_query::LikeExpr::new(format!("%\"{escaped}\"%")).escape('\\')
}

/// Retention/purge cutoff `now - hours`, saturating on any out-of-range
/// input: an API-supplied huge value means "older than longer ago than
/// representable" and yields `NaiveDateTime::MIN` (matches nothing) — never
/// a wrapped-negative duration, which would put the cutoff in the *future*
/// and match everything, and never the `Duration::hours` panic. A bare
/// `as i64` cast fails both ways (`u64::MAX as i64 == -1`).
pub fn cutoff_hours_ago(now: chrono::NaiveDateTime, hours: u64) -> chrono::NaiveDateTime {
    chrono::Duration::try_hours(i64::try_from(hours).unwrap_or(i64::MAX))
        .and_then(|d| now.checked_sub_signed(d))
        .unwrap_or(chrono::NaiveDateTime::MIN)
}

/// [`cutoff_hours_ago`], in days.
pub fn cutoff_days_ago(now: chrono::NaiveDateTime, days: u64) -> chrono::NaiveDateTime {
    chrono::Duration::try_days(i64::try_from(days).unwrap_or(i64::MAX))
        .and_then(|d| now.checked_sub_signed(d))
        .unwrap_or(chrono::NaiveDateTime::MIN)
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
    use crate::{DbBackend, build_sqlx};

    let backend = pool.backend();
    match backend {
        DbBackend::Sqlite | DbBackend::Postgres => {
            update.returning(sea_query::Query::returning().column(returning));
            let (sql, values) = build_sqlx(backend, update);
            pool.fetch_scalar::<i64>(&sql, values)
                .await
                .map_err(OrionError::Storage)
        }
        DbBackend::Mysql => {
            let mut tx = pool.begin_tx().await.map_err(OrionError::Storage)?;
            let (sql, values) = build_sqlx(backend, update);
            tx.execute_query(&sql, values).await?;
            let (sql, values) = build_sqlx(backend, read_back);
            let (value,): (i64,) = fetch_required_tx(&mut tx, &sql, values, missing).await?;
            tx.commit().await.map_err(OrionError::Storage)?;
            Ok(value)
        }
    }
}

/// The two statement kinds [`write_returning_row`] accepts.
pub enum WriteStatement<'a> {
    Insert(&'a mut sea_query::InsertStatement),
    Update(&'a mut sea_query::UpdateStatement),
}

impl WriteStatement<'_> {
    /// Whether `RETURNING *` on this backend yields the row as stored.
    ///
    /// Postgres and SQLite: yes for both kinds — an UPDATE sets its own
    /// `updated_at` ([`updated_now`]), so the returned row carries the new
    /// time. (SQLite's UPDATEs could not use it while an AFTER UPDATE trigger
    /// wrote `updated_at` in a second statement RETURNING never saw.) MySQL:
    /// no RETURNING at all.
    fn supports_returning(&self, backend: crate::DbBackend) -> bool {
        match backend {
            crate::DbBackend::Postgres | crate::DbBackend::Sqlite => true,
            crate::DbBackend::Mysql => false,
        }
    }

    fn build_returning_all(
        &mut self,
        backend: crate::DbBackend,
    ) -> (String, sea_query_sqlx::SqlxValues) {
        match self {
            Self::Insert(q) => crate::build_sqlx(backend, q.returning_all()),
            Self::Update(q) => crate::build_sqlx(backend, q.returning_all()),
        }
    }

    fn build(&mut self, backend: crate::DbBackend) -> (String, sea_query_sqlx::SqlxValues) {
        match self {
            Self::Insert(q) => crate::build_sqlx(backend, &mut **q),
            Self::Update(q) => crate::build_sqlx(backend, &mut **q),
        }
    }
}

/// Run an INSERT or UPDATE that must yield the row it wrote.
///
/// Where `RETURNING *` reflects the stored row (see
/// `WriteStatement::supports_returning` — a private detail, hence not a doc
/// link) the write and the read are one
/// statement; everywhere else the write and the `read_back` SELECT run inside
/// one transaction, so the row returned is the row this statement produced
/// and not a later writer's — which a separate pool query after the write
/// (let alone after `tx.commit()`) cannot promise.
///
/// Error mapping stays per-site: driver errors from the write itself go
/// through `map_write_err`, and a write that yields no row — an UPDATE
/// whose predicate matched nothing, or a MySQL read-back miss — becomes
/// `missing()`.
pub async fn write_returning_row<T: DbRow>(
    pool: &DbPool,
    mut write: WriteStatement<'_>,
    read_back: &mut sea_query::SelectStatement,
    map_write_err: impl FnOnce(sqlx::Error) -> OrionError,
    missing: impl FnOnce() -> OrionError,
) -> Result<T, OrionError> {
    let backend = pool.backend();
    if write.supports_returning(backend) {
        let (sql, values) = write.build_returning_all(backend);
        pool.fetch_optional_as::<T>(&sql, values)
            .await
            .map_err(map_write_err)?
            .ok_or_else(missing)
    } else {
        let mut tx = pool.begin_tx().await.map_err(OrionError::Storage)?;
        let row = write_returning_row_tx(&mut tx, write, read_back, map_write_err, missing).await?;
        tx.commit().await.map_err(OrionError::Storage)?;
        Ok(row)
    }
}

/// Transaction-scoped variant of [`write_returning_row`].
///
/// Same contract; the difference is who owns the transaction. The pooled twin
/// above opens one only where the backend cannot reflect the stored row from
/// the write itself, because there the read-back must not see a later writer's
/// row. Here the caller already holds a transaction, so the write and its
/// read-back join whatever else is in it — for the audited mutations (§2.6),
/// the audit row that has to commit with the change.
pub async fn write_returning_row_tx<T: DbRow>(
    tx: &mut DbTransaction,
    mut write: WriteStatement<'_>,
    read_back: &mut sea_query::SelectStatement,
    map_write_err: impl FnOnce(sqlx::Error) -> OrionError,
    missing: impl FnOnce() -> OrionError,
) -> Result<T, OrionError> {
    let backend = tx.backend();
    if write.supports_returning(backend) {
        let (sql, values) = write.build_returning_all(backend);
        tx.fetch_optional_as::<T>(&sql, values)
            .await
            .map_err(map_write_err)?
            .ok_or_else(missing)
    } else {
        let (sql, values) = write.build(backend);
        tx.execute_query(&sql, values)
            .await
            .map_err(map_write_err)?;
        let (sql, values) = crate::build_sqlx(backend, read_back);
        fetch_required_tx(tx, &sql, values, missing).await
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
    use crate::{DbBackend, build_sqlx};

    let backend = pool.backend();
    match backend {
        DbBackend::Sqlite | DbBackend::Postgres => {
            insert.on_conflict(
                sea_query::OnConflict::column(conflict_col)
                    .do_nothing()
                    .to_owned(),
            );
            let (sql, values) = build_sqlx(backend, &mut insert);
            pool.execute_query(&sql, values)
                .await
                .map_err(OrionError::Storage)
        }
        DbBackend::Mysql => {
            // sea-query has no INSERT IGNORE rendering; patch the verb into
            // the rendered SQL rather than hand-rolling placeholders.
            let (sql, values) = build_sqlx(backend, &mut insert);
            let sql = sql.replacen("INSERT INTO", "INSERT IGNORE INTO", 1);
            pool.execute_query(&sql, values)
                .await
                .map_err(OrionError::Storage)
        }
    }
}

/// [`insert_if_absent`] over a **composite** conflict target.
///
/// The occurrence ledger needs this: its uniqueness is
/// `(channel_id, scheduled_for)`, not its primary key — the id is minted fresh
/// on every attempt to materialise, so a duplicate is only ever detectable by
/// the identity index.
pub async fn insert_if_absent_on<I>(
    pool: &DbPool,
    mut insert: sea_query::InsertStatement,
    conflict_cols: I,
) -> Result<u64, OrionError>
where
    I: IntoIterator,
    I::Item: sea_query::IntoIden,
{
    use crate::{DbBackend, build_sqlx};

    let backend = pool.backend();
    match backend {
        DbBackend::Sqlite | DbBackend::Postgres => {
            insert.on_conflict(
                sea_query::OnConflict::columns(conflict_cols)
                    .do_nothing()
                    .to_owned(),
            );
            let (sql, values) = build_sqlx(backend, &mut insert);
            pool.execute_query(&sql, values)
                .await
                .map_err(OrionError::Storage)
        }
        DbBackend::Mysql => {
            let (sql, values) = build_sqlx(backend, &mut insert);
            let sql = sql.replacen("INSERT INTO", "INSERT IGNORE INTO", 1);
            pool.execute_query(&sql, values)
                .await
                .map_err(OrionError::Storage)
        }
    }
}

/// [`insert_if_absent_on`] inside a transaction.
///
/// Needed where the insert's *outcome* is the decision and the surrounding
/// writes must commit or roll back with it — singleton acquisition, where "the
/// row already existed" means another occurrence holds the key.
pub async fn insert_if_absent_tx<I>(
    tx: &mut crate::DbTransaction,
    mut insert: sea_query::InsertStatement,
    conflict_cols: I,
) -> Result<u64, OrionError>
where
    I: IntoIterator,
    I::Item: sea_query::IntoIden,
{
    use crate::{DbBackend, build_sqlx};

    let backend = tx.backend();
    match backend {
        DbBackend::Sqlite | DbBackend::Postgres => {
            insert.on_conflict(
                sea_query::OnConflict::columns(conflict_cols)
                    .do_nothing()
                    .to_owned(),
            );
            let (sql, values) = build_sqlx(backend, &mut insert);
            tx.execute_query(&sql, values).await.map_err(Into::into)
        }
        DbBackend::Mysql => {
            let (sql, values) = build_sqlx(backend, &mut insert);
            let sql = sql.replacen("INSERT INTO", "INSERT IGNORE INTO", 1);
            tx.execute_query(&sql, values).await.map_err(Into::into)
        }
    }
}

/// Rows deleted per statement by [`delete_chunked`].
///
/// Small enough that one statement is short on any backend, large enough that
/// a big backlog drains in a sane number of round trips.
const DELETE_CHUNK_ROWS: u64 = 1_000;

/// Hard stop on chunks per call, so one tick cannot run unboundedly.
///
/// At the default chunk size this is 5M rows per tick. Anything past it is
/// left for the next tick rather than held open — in cluster mode the job
/// lease expires `interval_secs + 60` after the tick starts, and a delete
/// still running past that lets a second node begin a duplicate.
const DELETE_MAX_CHUNKS: usize = 5_000;

/// Delete matching rows in bounded chunks rather than one unbounded statement.
///
/// A single `DELETE … WHERE created_at < cutoff` per tick would make the first
/// run after enabling retention one transaction over potentially millions of
/// rows: SQLite holds the write lock for its whole duration, so every other
/// writer hits the 5 s `busy_timeout` and fails; Postgres bloats WAL and blocks
/// autovacuum; MySQL can exceed `innodb_lock_wait_timeout`.
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
        let (sql, values) = crate::build_sqlx(
            pool.backend(),
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
    /// Every clock helper is UTC. A literal `LOCALTIMESTAMP` (or a bare
    /// `CURRENT_TIMESTAMP` on a non-UTC session) shifts cron, DLQ retry and
    /// retention by the server's offset.
    #[test]
    fn every_backends_clock_is_utc() {
        use crate::DbBackend::{Mysql, Postgres, Sqlite};
        assert_eq!(sql_now(Sqlite), "datetime('now')");
        assert!(sql_now(Postgres).contains("UTC"), "{}", sql_now(Postgres));
        assert_eq!(sql_now(Mysql), "UTC_TIMESTAMP()");
        assert!(!sql_now_plus_secs(Postgres, 5).contains("LOCALTIMESTAMP"));
        assert!(sql_now_minus_secs(Postgres, 5).contains("UTC"));
        assert!(sql_now_plus_secs(Mysql, 5).contains("UTC_TIMESTAMP"));
        assert!(sql_now_minus_secs(Sqlite, 5).contains("datetime('now'"));
    }

    use super::*;

    #[test]
    fn optional_string_some() {
        let v = optional_string_value(Some("hello"));
        assert_eq!(v, sea_query::Value::String(Some("hello".into())));
    }

    #[test]
    fn optional_string_none() {
        let v = optional_string_value(None);
        assert_eq!(v, sea_query::Value::String(None));
    }

    #[test]
    fn limit_defaults_and_clamps() {
        assert_eq!(clamp_limit(None), 50);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(9999)), 1000);
    }

    #[test]
    fn a_cursor_round_trips_and_a_forged_one_is_refused() {
        let cursor = KeysetCursor {
            created_at: chrono::DateTime::from_timestamp_micros(1_700_000_000_123_456)
                .expect("test")
                .naive_utc(),
            id: "0190-abc".to_string(),
        };
        assert_eq!(
            KeysetCursor::decode(&cursor.encode()).expect("test"),
            cursor
        );
        for bad in ["", "nodot", "x.id", "123.", "99999999999999999999.id"] {
            assert!(KeysetCursor::decode(bad).is_err(), "{bad}");
        }
    }

    // -- D18: the one count-then-page shape --

    fn sample_page(projection: Projection) -> Page {
        use sea_query::IntoIden;
        Page {
            from: crate::schema::Traces::Table.into_iden(),
            projection,
            cond: sea_query::Condition::all(),
            created_at: crate::schema::Traces::CreatedAt.into_iden(),
            id: crate::schema::Traces::Id.into_iden(),
            limit: 25,
            cursor: None,
            include_total: false,
        }
    }

    fn rendered(page: &Page) -> String {
        super::page_select(page).to_string(sea_query::SqliteQueryBuilder)
    }

    /// Newest first with the id as tie-break, one row past the page to learn
    /// whether another follows, and the cursor as a strict `(created_at, id)`
    /// comparison.
    #[test]
    fn the_page_query_is_keyset_ordered_and_reads_one_extra_row() {
        let mut page = sample_page(Projection::All);
        let sql = rendered(&page);
        assert!(
            sql.contains(r#"ORDER BY "created_at" DESC, "id" DESC LIMIT 26"#),
            "{sql}"
        );
        assert!(!sql.contains("OFFSET"), "{sql}");
        page.cursor = Some(KeysetCursor {
            created_at: chrono::DateTime::from_timestamp_micros(1_700_000_000_000_000)
                .expect("test")
                .naive_utc(),
            id: "t-9".to_string(),
        });
        let sql = rendered(&page);
        assert!(sql.contains(r#""created_at" <"#), "{sql}");
        assert!(sql.contains(r#""id" < 't-9'"#), "{sql}");
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
            crate::schema::Traces::Id.into_iden(),
            crate::schema::Traces::Status.into_iden(),
        ])));
        assert!(sql.contains(r#"SELECT "id", "status""#), "{sql}");
        assert!(!sql.contains('*'), "{sql}");
        assert!(!sql.contains("access_token_hash"), "{sql}");
    }

    // -- D23: one write, one row back --

    /// The backend dispatch `write_returning_row` rides on. An UPDATE sets its
    /// own `updated_at`, so RETURNING reflects the stored row on Postgres and
    /// SQLite for both statement kinds; MySQL has no RETURNING at all.
    #[test]
    fn returning_dispatch_matches_backend_semantics() {
        use crate::DbBackend::{Mysql, Postgres, Sqlite};
        use crate::schema::Traces;

        let mut insert = Query::insert()
            .into_table(Traces::Table)
            .columns([Traces::Id])
            .values_panic(["t1".into()])
            .to_owned();
        let mut update = Query::update()
            .table(Traces::Table)
            .value(Traces::Status, "completed")
            .to_owned();

        assert!(WriteStatement::Insert(&mut insert).supports_returning(Postgres));
        assert!(WriteStatement::Update(&mut update).supports_returning(Postgres));
        assert!(WriteStatement::Insert(&mut insert).supports_returning(Sqlite));
        assert!(WriteStatement::Update(&mut update).supports_returning(Sqlite));
        assert!(!WriteStatement::Insert(&mut insert).supports_returning(Mysql));
        assert!(!WriteStatement::Update(&mut update).supports_returning(Mysql));
    }

    /// The single-statement arm must ask for every column — the callers decode
    /// the same row types their `SELECT *` read-backs decoded.
    #[test]
    fn returning_all_is_appended_to_both_statement_kinds() {
        use crate::schema::Traces;
        use sea_query::PostgresQueryBuilder;

        let mut insert = Query::insert()
            .into_table(Traces::Table)
            .columns([Traces::Id])
            .values_panic(["t1".into()])
            .to_owned();
        let (sql, _) = insert.returning_all().build(PostgresQueryBuilder);
        assert!(sql.ends_with("RETURNING *"), "{sql}");

        let mut update = Query::update()
            .table(Traces::Table)
            .value(Traces::Status, "completed")
            .to_owned();
        let (sql, _) = update.returning_all().build(PostgresQueryBuilder);
        assert!(sql.ends_with("RETURNING *"), "{sql}");
    }

    /// Page bounds reach the statement, and an empty filter constrains
    /// nothing — the shape `connectors.list_paginated` relies on, having no
    /// filter at all.
    #[test]
    fn page_bounds_and_empty_filter_render_as_expected() {
        let sql = rendered(&sample_page(Projection::All));
        // The page size plus the one extra row that reveals a next page.
        assert!(sql.contains("LIMIT 26"), "{sql}");
        assert!(!sql.contains("OFFSET"), "{sql}");
        assert!(sql.contains(r#"ORDER BY "created_at" DESC"#), "{sql}");
        // sea-query renders an empty `Condition::all()` as no predicate; what
        // matters is that it names no column.
        assert!(
            !sql.contains(r#"WHERE "#) || sql.contains("WHERE TRUE"),
            "{sql}"
        );
    }
}
