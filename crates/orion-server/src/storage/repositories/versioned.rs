//! Shared machinery for the two versioned entities (workflows, channels):
//! composite-PK `(id, version)` tables with a draft/active/archived lifecycle.
//! Both repositories delegate the common read and lifecycle shapes here so a
//! fix lands once instead of twice; the genuinely entity-specific parts —
//! column sets, draft merging, and workflow rollout arithmetic — stay in the
//! owning repository.

use sea_query::{Asterisk, Condition, DynIden, Expr, ExprTrait, Order, Query};

use crate::errors::OrionError;
use crate::storage::models::EntityStatus;
use crate::storage::{DbPool, DbRow, DbTransaction, build_sqlx};

use super::helpers::{
    Page, PaginatedResult, Projection, fetch_required, fetch_required_tx, paginate,
};

/// Idens and error wording for one versioned entity.
pub(crate) struct VersionedSpec {
    pub table: DynIden,
    pub id_col: DynIden,
    pub version_col: DynIden,
    pub status_col: DynIden,
    pub priority_col: DynIden,
    /// Named so [`archive_latest_active`]'s single-statement SQLite arm can
    /// stamp it explicitly — there RETURNING cannot see the AFTER UPDATE
    /// trigger that normally maintains it.
    pub updated_at_col: DynIden,
    /// Capitalised, for "not found": `Workflow '{id}' not found`.
    pub label: &'static str,
    /// Lowercase noun: `No draft version found for workflow '{id}'`.
    pub noun: &'static str,
}

/// The `SELECT * WHERE id = ? AND version = ?` shape shared by
/// [`get_version_tx`] and the read-back arm of [`write_returning_version`].
fn version_select(spec: &VersionedSpec, id: &str, version: i64) -> sea_query::SelectStatement {
    Query::select()
        .column(Asterisk)
        .from(spec.table.clone())
        .and_where(Expr::col(spec.id_col.clone()).eq(id))
        .and_where(Expr::col(spec.version_col.clone()).eq(version))
        .to_owned()
}

/// Uniform miss wording for a [`version_select`] that found nothing.
fn version_not_found(spec: &VersionedSpec, id: &str, version: i64) -> OrionError {
    OrionError::NotFound(format!("{} '{id}' version {version} not found", spec.label))
}

/// Fetch one specific `(id, version)` row inside a transaction — the
/// read-back the multi-statement lifecycle paths (activate, update_rollout)
/// run INSIDE their transaction, so the row they return is the row their
/// writes produced. D23: it used to be fetched from the pool after
/// `tx.commit()`, where a concurrent writer's version of the row could come
/// back instead.
pub(crate) async fn get_version_tx<T: DbRow>(
    tx: &mut DbTransaction,
    spec: &VersionedSpec,
    id: &str,
    version: i64,
) -> Result<T, OrionError> {
    let (sql, values) = build_sqlx(tx.backend(), &mut version_select(spec, id, version));
    fetch_required_tx(tx, &sql, values, || version_not_found(spec, id, version)).await
}

/// Run a single-statement create/update that must yield the `(id, version)`
/// row it wrote (D23): `RETURNING *` where the backend's triggers allow it,
/// otherwise the write and the read-back inside one transaction — see
/// [`super::helpers::write_returning_row`]. `map_write_err` keeps error
/// mapping per-site: the create paths map duplicate-key errors to 409.
pub(crate) async fn write_returning_version<T: DbRow>(
    pool: &DbPool,
    spec: &VersionedSpec,
    write: super::helpers::WriteStatement<'_>,
    id: &str,
    version: i64,
    map_write_err: impl FnOnce(sqlx::Error) -> OrionError,
) -> Result<T, OrionError> {
    let mut read_back = version_select(spec, id, version);
    super::helpers::write_returning_row(pool, write, &mut read_back, map_write_err, || {
        version_not_found(spec, id, version)
    })
    .await
}

/// Fetch the latest version of an entity (any status).
pub(crate) async fn get_latest<T: DbRow>(
    pool: &DbPool,
    spec: &VersionedSpec,
    id: &str,
) -> Result<T, OrionError> {
    let (sql, values) = build_sqlx(
        pool.backend(),
        Query::select()
            .column(Asterisk)
            .from(spec.table.clone())
            .and_where(Expr::col(spec.id_col.clone()).eq(id))
            .order_by(spec.version_col.clone(), Order::Desc)
            .limit(1),
    );
    fetch_required(pool, &sql, values, || {
        OrionError::NotFound(format!("{} '{id}' not found", spec.label))
    })
    .await
}

/// Delete every version of an entity; `NotFound` when none existed.
pub(crate) async fn delete_all_versions(
    pool: &DbPool,
    spec: &VersionedSpec,
    id: &str,
) -> Result<(), OrionError> {
    let (sql, values) = build_sqlx(
        pool.backend(),
        Query::delete()
            .from_table(spec.table.clone())
            .and_where(Expr::col(spec.id_col.clone()).eq(id)),
    );
    if pool.execute_query(&sql, values).await? == 0 {
        return Err(OrionError::NotFound(format!(
            "{} '{id}' not found",
            spec.label
        )));
    }
    Ok(())
}

/// All active versions across entities, highest priority first (engine load
/// order).
pub(crate) async fn list_active<T: DbRow>(
    pool: &DbPool,
    spec: &VersionedSpec,
) -> Result<Vec<T>, OrionError> {
    let (sql, values) = build_sqlx(
        pool.backend(),
        Query::select()
            .column(Asterisk)
            .from(spec.table.clone())
            .and_where(Expr::col(spec.status_col.clone()).eq(EntityStatus::Active.as_str()))
            .order_by(spec.priority_col.clone(), Order::Desc),
    );
    Ok(pool.fetch_all_as::<T>(&sql, values).await?)
}

/// One page of an entity's version history, newest first.
pub(crate) async fn list_versions<T: DbRow>(
    pool: &DbPool,
    spec: &VersionedSpec,
    id: &str,
    filter: &super::helpers::VersionFilter,
) -> Result<PaginatedResult<T>, OrionError> {
    let (limit, offset) = super::helpers::clamp_pagination(filter.limit, filter.offset);
    paginate(
        pool,
        Page {
            from: spec.table.clone(),
            projection: Projection::All,
            cond: Condition::all().add(Expr::col(spec.id_col.clone()).eq(id)),
            sort: spec.version_col.clone(),
            order: Order::Desc,
            limit,
            offset,
        },
    )
    .await
}

/// The `SELECT * WHERE id = ? AND status = 'draft'` both draft-consuming
/// paths (update, activate) start from.
fn draft_query(
    backend: crate::storage::DbBackend,
    spec: &VersionedSpec,
    id: &str,
) -> (String, sea_query_sqlx::SqlxValues) {
    build_sqlx(
        backend,
        Query::select()
            .column(Asterisk)
            .from(spec.table.clone())
            .and_where(Expr::col(spec.id_col.clone()).eq(id))
            .and_where(Expr::col(spec.status_col.clone()).eq(EntityStatus::Draft.as_str())),
    )
}

/// Uniform "no draft" wording for [`draft_query`] misses.
///
/// D22: this was a 400 while every sibling lookup in the module maps a
/// missing row to `NotFound` — the same inconsistency the comment on
/// [`archive_latest_active`] already argues against (G7). "The draft you
/// addressed does not exist" is a 404 like "the entity you addressed does
/// not exist"; which one a caller gets should not depend on which lifecycle
/// method they reached first.
fn no_draft_err(spec: &VersionedSpec, id: &str) -> OrionError {
    OrionError::NotFound(format!("No draft version found for {} '{id}'", spec.noun))
}

/// Fetch the draft version, `NotFound` when there is none.
///
/// The query and its miss wording are paired here rather than left to the four
/// draft-consuming call sites (update, activate, per entity) to combine
/// correctly one at a time.
pub(crate) async fn require_draft<T: DbRow>(
    pool: &DbPool,
    spec: &VersionedSpec,
    id: &str,
) -> Result<T, OrionError> {
    let (sql, values) = draft_query(pool.backend(), spec, id);
    fetch_required(pool, &sql, values, || no_draft_err(spec, id)).await
}

/// Transaction-scoped variant of [`require_draft`], for the activate paths.
pub(crate) async fn require_draft_tx<T: DbRow>(
    tx: &mut DbTransaction,
    spec: &VersionedSpec,
    id: &str,
) -> Result<T, OrionError> {
    let (sql, values) = draft_query(tx.backend(), spec, id);
    fetch_required_tx(tx, &sql, values, || no_draft_err(spec, id)).await
}

/// Reject `create_new_version` when a draft already exists (the single-draft
/// invariant the DB triggers also enforce — this gives the friendly error).
pub(crate) async fn ensure_no_draft<T: DbRow>(
    pool: &DbPool,
    spec: &VersionedSpec,
    id: &str,
) -> Result<(), OrionError> {
    let (sql, values) = draft_query(pool.backend(), spec, id);
    super::helpers::ensure_absent::<T>(pool, &sql, values, || {
        OrionError::Conflict(format!("{} '{id}' already has a draft version", spec.label))
    })
    .await
}

/// `UPDATE ... SET status = 'archived' WHERE id = ? AND status = 'active'`,
/// optionally sparing one version (partial-rollout activation keeps the
/// primary active row).
pub(crate) fn archive_actives_query(
    spec: &VersionedSpec,
    id: &str,
    exclude_version: Option<i64>,
) -> sea_query::UpdateStatement {
    let mut q = Query::update();
    q.table(spec.table.clone())
        .value(spec.status_col.clone(), EntityStatus::Archived.as_str())
        .and_where(Expr::col(spec.id_col.clone()).eq(id))
        .and_where(Expr::col(spec.status_col.clone()).eq(EntityStatus::Active.as_str()));
    if let Some(v) = exclude_version {
        q.and_where(Expr::col(spec.version_col.clone()).ne(v));
    }
    q
}

/// Archive an entity: find the latest active version (NotFound when none),
/// archive every active version, and return the newly archived row.
pub(crate) async fn archive_latest_active<T: DbRow + HasVersion>(
    pool: &DbPool,
    spec: &VersionedSpec,
    id: &str,
) -> Result<T, OrionError> {
    // 404, not 400: every sibling lookup in this module maps a missing row to
    // NotFound, and "which status code does a missing entity give?" should not
    // depend on which repository method you happened to call (proposal G7).
    let no_active =
        || OrionError::NotFound(format!("No active version found for {} '{id}'", spec.noun));

    // D23. Postgres and SQLite do the whole thing in one statement: archive
    // every active version and take the archived rows back — the newest is
    // the row this method returns, and no rows means nothing was active. One
    // statement, not a read-then-write transaction: a tx that reads first and
    // writes second deadlocks against a concurrent activate's tx doing the
    // same (and on WAL SQLite is an unretryable SQLITE_BUSY_SNAPSHOT).
    // Read-then-write transactions that cannot collapse into one statement
    // take `DbPool::begin_write_tx` instead (D30), which starts SQLite with
    // BEGIN IMMEDIATE so no read snapshot exists to go stale.
    //
    // On SQLite the statement also stamps `updated_at` itself: the column is
    // normally maintained by an AFTER UPDATE trigger whose second write
    // RETURNING cannot see (Postgres' BEFORE trigger needs no such help).
    let backend = pool.backend();
    if backend != crate::storage::DbBackend::Mysql {
        let mut update = archive_actives_query(spec, id, None);
        if backend == crate::storage::DbBackend::Sqlite {
            update.value(
                spec.updated_at_col.clone(),
                Expr::cust(super::helpers::sql_now(backend)),
            );
        }
        update.returning_all();
        let (sql, values) = build_sqlx(backend, &mut update);
        let archived: Vec<T> = pool.fetch_all_as(&sql, values).await?;
        return archived
            .into_iter()
            .max_by_key(|row| row.version())
            .ok_or_else(no_active);
    }

    // MySQL (no RETURNING): find the newest active version, archive every
    // active one, and read the archived row back — all inside one
    // transaction. The read-back used to run on the pool after the archive,
    // where a concurrent writer's row could come back instead.
    let mut tx = pool.begin_tx().await.map_err(OrionError::Storage)?;
    let (sql, values) = build_sqlx(
        backend,
        Query::select()
            .column(Asterisk)
            .from(spec.table.clone())
            .and_where(Expr::col(spec.id_col.clone()).eq(id))
            .and_where(Expr::col(spec.status_col.clone()).eq(EntityStatus::Active.as_str()))
            .order_by(spec.version_col.clone(), Order::Desc)
            .limit(1),
    );
    let active: T = fetch_required_tx(&mut tx, &sql, values, no_active).await?;

    let (sql, values) = build_sqlx(backend, &mut archive_actives_query(spec, id, None));
    tx.execute_query(&sql, values).await?;

    let archived = get_version_tx(&mut tx, spec, id, active.version()).await?;
    tx.commit().await.map_err(OrionError::Storage)?;
    Ok(archived)
}

/// The one field [`archive_latest_active`] needs off the fetched row.
pub(crate) trait HasVersion {
    fn version(&self) -> i64;
}

// D18: `paginate` used to live here despite having nothing to do with
// versioned entities, and five of the seven list paths could not reach it. It
// is now `helpers::paginate`.
