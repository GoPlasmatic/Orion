//! Shared machinery for the two versioned entities (workflows, channels):
//! composite-PK `(id, version)` tables with a draft/active/archived lifecycle.
//! Both repositories delegate the common read and lifecycle shapes here so a
//! fix lands once instead of twice; the genuinely entity-specific parts —
//! column sets, draft merging, and workflow rollout arithmetic — stay in the
//! owning repository.

use sea_query::{Asterisk, Condition, DynIden, Expr, Order, Query};

use crate::errors::OrionError;
use crate::storage::models::EntityStatus;
use crate::storage::{DbPool, build_sqlx};

use super::helpers::{PaginatedResult, count_where, fetch_required};

/// Idens and error wording for one versioned entity.
pub(crate) struct VersionedSpec {
    pub table: DynIden,
    pub id_col: DynIden,
    pub version_col: DynIden,
    pub status_col: DynIden,
    pub priority_col: DynIden,
    /// Capitalised, for "not found": `Workflow '{id}' not found`.
    pub label: &'static str,
    /// Lowercase noun: `No draft version found for workflow '{id}'`.
    pub noun: &'static str,
}

/// The row bounds `DbPool`'s typed fetches need on all three backends.
pub(crate) trait VersionedRow:
    for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow>
    + for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>
    + for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow>
    + Send
    + Unpin
{
}
impl<T> VersionedRow for T where
    T: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow>
        + for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>
        + for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow>
        + Send
        + Unpin
{
}

/// Fetch one specific `(id, version)` row.
pub(crate) async fn get_version<T: VersionedRow>(
    pool: &DbPool,
    spec: &VersionedSpec,
    id: &str,
    version: i64,
) -> Result<T, OrionError> {
    let (sql, values) = build_sqlx(
        Query::select()
            .column(Asterisk)
            .from(spec.table.clone())
            .and_where(Expr::col(spec.id_col.clone()).eq(id))
            .and_where(Expr::col(spec.version_col.clone()).eq(version)),
    );
    pool.fetch_optional_as::<T>(&sql, values)
        .await?
        .ok_or_else(|| {
            OrionError::NotFound(format!("{} '{id}' version {version} not found", spec.label))
        })
}

/// Fetch the latest version of an entity (any status).
pub(crate) async fn get_latest<T: VersionedRow>(
    pool: &DbPool,
    spec: &VersionedSpec,
    id: &str,
) -> Result<T, OrionError> {
    let (sql, values) = build_sqlx(
        Query::select()
            .column(Asterisk)
            .from(spec.table.clone())
            .and_where(Expr::col(spec.id_col.clone()).eq(id))
            .order_by(spec.version_col.clone(), Order::Desc)
            .limit(1),
    );
    pool.fetch_optional_as::<T>(&sql, values)
        .await?
        .ok_or_else(|| OrionError::NotFound(format!("{} '{id}' not found", spec.label)))
}

/// Delete every version of an entity; `NotFound` when none existed.
pub(crate) async fn delete_all_versions(
    pool: &DbPool,
    spec: &VersionedSpec,
    id: &str,
) -> Result<(), OrionError> {
    let (sql, values) = build_sqlx(
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
pub(crate) async fn list_active<T: VersionedRow>(
    pool: &DbPool,
    spec: &VersionedSpec,
) -> Result<Vec<T>, OrionError> {
    let (sql, values) = build_sqlx(
        Query::select()
            .column(Asterisk)
            .from(spec.table.clone())
            .and_where(Expr::col(spec.status_col.clone()).eq(EntityStatus::Active.as_str()))
            .order_by(spec.priority_col.clone(), Order::Desc),
    );
    Ok(pool.fetch_all_as::<T>(&sql, values).await?)
}

/// One page of an entity's version history, newest first.
pub(crate) async fn list_versions<T: VersionedRow>(
    pool: &DbPool,
    spec: &VersionedSpec,
    id: &str,
    limit: i64,
    offset: i64,
) -> Result<PaginatedResult<T>, OrionError> {
    let limit = limit.clamp(1, 1000);
    let offset = offset.max(0);

    let total = count_where(
        pool,
        spec.table.clone(),
        Condition::all().add(Expr::col(spec.id_col.clone()).eq(id)),
    )
    .await?;

    let (sql, values) = build_sqlx(
        Query::select()
            .column(Asterisk)
            .from(spec.table.clone())
            .and_where(Expr::col(spec.id_col.clone()).eq(id))
            .order_by(spec.version_col.clone(), Order::Desc)
            .limit(limit as u64)
            .offset(offset as u64),
    );
    let data = pool.fetch_all_as::<T>(&sql, values).await?;

    Ok(PaginatedResult {
        data,
        total,
        limit,
        offset,
    })
}

/// The `SELECT * WHERE id = ? AND status = 'draft'` both draft-consuming
/// paths (update, activate) start from.
pub(crate) fn draft_query(
    spec: &VersionedSpec,
    id: &str,
) -> (String, sea_query_binder::SqlxValues) {
    build_sqlx(
        Query::select()
            .column(Asterisk)
            .from(spec.table.clone())
            .and_where(Expr::col(spec.id_col.clone()).eq(id))
            .and_where(Expr::col(spec.status_col.clone()).eq(EntityStatus::Draft.as_str())),
    )
}

/// Uniform "no draft" wording for [`draft_query`] misses.
pub(crate) fn no_draft_err(spec: &VersionedSpec, id: &str) -> OrionError {
    OrionError::BadRequest(format!("No draft version found for {} '{id}'", spec.noun))
}

/// Reject `create_new_version` when a draft already exists (the single-draft
/// invariant the DB triggers also enforce — this gives the friendly error).
pub(crate) async fn ensure_no_draft<T: VersionedRow>(
    pool: &DbPool,
    spec: &VersionedSpec,
    id: &str,
) -> Result<(), OrionError> {
    let (sql, values) = draft_query(spec, id);
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
) -> (String, sea_query_binder::SqlxValues) {
    let mut q = Query::update();
    q.table(spec.table.clone())
        .value(spec.status_col.clone(), EntityStatus::Archived.as_str())
        .and_where(Expr::col(spec.id_col.clone()).eq(id))
        .and_where(Expr::col(spec.status_col.clone()).eq(EntityStatus::Active.as_str()));
    if let Some(v) = exclude_version {
        q.and_where(Expr::col(spec.version_col.clone()).ne(v));
    }
    build_sqlx(&mut q)
}

/// Archive an entity: find the latest active version (BadRequest when none),
/// archive every active version, and return the newly archived row.
pub(crate) async fn archive_latest_active<T: VersionedRow + HasVersion>(
    pool: &DbPool,
    spec: &VersionedSpec,
    id: &str,
) -> Result<T, OrionError> {
    let (sql, values) = build_sqlx(
        Query::select()
            .column(Asterisk)
            .from(spec.table.clone())
            .and_where(Expr::col(spec.id_col.clone()).eq(id))
            .and_where(Expr::col(spec.status_col.clone()).eq(EntityStatus::Active.as_str()))
            .order_by(spec.version_col.clone(), Order::Desc)
            .limit(1),
    );
    let active: T = fetch_required(pool, &sql, values, || {
        OrionError::BadRequest(format!("No active version found for {} '{id}'", spec.noun))
    })
    .await?;

    let (sql, values) = archive_actives_query(spec, id, None);
    pool.execute_query(&sql, values).await?;

    get_version(pool, spec, id, active.version()).await
}

/// The one field [`archive_latest_active`] needs off the fetched row.
pub(crate) trait HasVersion {
    fn version(&self) -> i64;
}

/// Count + page over `from` (a current-version view or the base table) with
/// the caller's filter condition and sort — the skeleton both
/// `list_paginated` impls shared.
pub(crate) async fn paginate<T: VersionedRow>(
    pool: &DbPool,
    from: DynIden,
    cond: Condition,
    sort: DynIden,
    order: Order,
    limit: i64,
    offset: i64,
) -> Result<PaginatedResult<T>, OrionError> {
    let total = count_where(pool, from.clone(), cond.clone()).await?;

    let (sql, values) = build_sqlx(
        Query::select()
            .column(Asterisk)
            .from(from)
            .cond_where(cond)
            .order_by(sort, order)
            .limit(limit as u64)
            .offset(offset as u64),
    );
    let data = pool.fetch_all_as::<T>(&sql, values).await?;

    Ok(PaginatedResult {
        data,
        total,
        limit,
        offset,
    })
}
