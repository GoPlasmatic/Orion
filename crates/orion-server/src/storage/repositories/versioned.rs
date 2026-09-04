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
    /// The engine load order, highest first — `None` for an entity without
    /// one (plugins), which lists by id instead.
    pub priority_col: Option<DynIden>,
    /// Named so [`archive_latest_active`]'s single-statement SQLite arm can
    /// stamp it explicitly — there RETURNING cannot see the AFTER UPDATE
    /// trigger that normally maintains it.
    pub updated_at_col: DynIden,
    /// Capitalised, for "not found": `Workflow '{id}' not found`.
    pub label: &'static str,
    /// Lowercase noun: `No draft version found for workflow '{id}'`.
    pub noun: &'static str,
}

/// The predicate that makes a plain `FROM <entity table>` read the *current*
/// version of each entity — the highest `version` for each id, which is the
/// row the `current_workflows` / `current_channels` views serve (§5).
///
/// Expressed as a predicate over the base table rather than as a view because
/// a view of `SELECT *` is a liability on two of the three backends: Postgres
/// and MySQL resolve the column list at `CREATE VIEW` time, so every migration
/// that adds a column to `workflows` or `channels` has to drop and recreate
/// both views — the reason `current_*` is re-created across five Postgres and
/// five MySQL migrations, one of which (`mysql/011`) documents a non-atomic
/// DDL failure mode that ends "finish by hand". A predicate has no schema of
/// its own to keep in step, so a new column reaches every reader for free, the
/// way it already does on SQLite.
///
/// Correlated `= (SELECT MAX(version) …)` rather than a tuple `IN`: it is one
/// column comparison, so it composes into an existing `Condition` without
/// touching the projection or the `FROM`, and every backend plans it against
/// the composite primary key `(id, version)`.
///
/// The subquery aliases the table because it names the same one as the outer
/// query; the outer reference stays unqualified so this drops into conditions
/// written against bare column idens.
pub(crate) fn is_current_version(spec: &VersionedSpec) -> Expr {
    let inner = sea_query::Alias::new("current_v");
    Expr::col(spec.version_col.clone()).eq(Expr::SubQuery(
        None,
        Box::new(
            Query::select()
                .expr(Expr::col(spec.version_col.clone()).max())
                .from_as(spec.table.clone(), inner.clone())
                .and_where(
                    Expr::col((inner, spec.id_col.clone()))
                        .equals((spec.table.clone(), spec.id_col.clone())),
                )
                .take()
                .into(),
        ),
    ))
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

/// The DELETE, and the `NotFound` for an id that had no versions. Shared so
/// the pooled and transactional forms cannot differ on either.
fn delete_all_versions_query(spec: &VersionedSpec, id: &str) -> sea_query::DeleteStatement {
    Query::delete()
        .from_table(spec.table.clone())
        .and_where(Expr::col(spec.id_col.clone()).eq(id))
        .to_owned()
}

fn deleted_or_missing(rows: u64, spec: &VersionedSpec, id: &str) -> Result<(), OrionError> {
    if rows == 0 {
        return Err(OrionError::NotFound(format!(
            "{} '{id}' not found",
            spec.label
        )));
    }
    Ok(())
}

/// Delete every version of an entity; `NotFound` when none existed.
pub(crate) async fn delete_all_versions(
    pool: &DbPool,
    spec: &VersionedSpec,
    id: &str,
) -> Result<(), OrionError> {
    let (sql, values) = build_sqlx(pool.backend(), &mut delete_all_versions_query(spec, id));
    deleted_or_missing(pool.execute_query(&sql, values).await?, spec, id)
}

/// [`delete_all_versions`] inside a caller-owned transaction, so the delete
/// and its audit row commit together (§2.6).
pub(crate) async fn delete_all_versions_tx(
    tx: &mut DbTransaction,
    spec: &VersionedSpec,
    id: &str,
) -> Result<(), OrionError> {
    let (sql, values) = build_sqlx(tx.backend(), &mut delete_all_versions_query(spec, id));
    deleted_or_missing(tx.execute_query(&sql, values).await?, spec, id)
}

/// All active versions across entities, highest priority first (engine load
/// order).
pub(crate) async fn list_active<T: DbRow>(
    pool: &DbPool,
    spec: &VersionedSpec,
) -> Result<Vec<T>, OrionError> {
    let mut select = Query::select();
    select
        .column(Asterisk)
        .from(spec.table.clone())
        .and_where(Expr::col(spec.status_col.clone()).eq(EntityStatus::Active.as_str()));
    match &spec.priority_col {
        Some(priority) => select.order_by(priority.clone(), Order::Desc),
        None => select.order_by(spec.id_col.clone(), Order::Asc),
    };
    let (sql, values) = build_sqlx(pool.backend(), &mut select);
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
    // D23. Postgres and SQLite do the whole thing in one statement: archive
    // every active version and take the archived rows back — the newest is
    // the row this method returns, and no rows means nothing was active. One
    // statement, not a read-then-write transaction: a tx that reads first and
    // writes second deadlocks against a concurrent activate's tx doing the
    // same (and on WAL SQLite is an unretryable SQLITE_BUSY_SNAPSHOT).
    // Read-then-write transactions that cannot collapse into one statement
    // take `DbPool::begin_write_tx` instead (D30), which starts SQLite with
    // BEGIN IMMEDIATE so no read snapshot exists to go stale.
    let backend = pool.backend();
    if backend != crate::storage::DbBackend::Mysql {
        let (sql, values) = build_sqlx(backend, &mut archive_returning_query(spec, id, backend));
        let archived: Vec<T> = pool.fetch_all_as(&sql, values).await?;
        return newest_or_no_active(archived, spec, id);
    }

    let mut tx = pool.begin_tx().await.map_err(OrionError::Storage)?;
    let archived = archive_latest_active_mysql(&mut tx, spec, id).await?;
    tx.commit().await.map_err(OrionError::Storage)?;
    Ok(archived)
}

/// [`archive_latest_active`] inside a caller-owned transaction, so the archive
/// and its audit row commit together (§2.6).
///
/// The single-statement path is kept rather than collapsed into the MySQL one:
/// it is the same statement, and running it inside the caller's transaction is
/// sound because that transaction is a `begin_write_tx` — the D30 shape, which
/// on SQLite holds the write lock from BEGIN and so has no read snapshot to go
/// stale.
pub(crate) async fn archive_latest_active_tx<T: DbRow + HasVersion>(
    tx: &mut DbTransaction,
    spec: &VersionedSpec,
    id: &str,
) -> Result<T, OrionError> {
    let backend = tx.backend();
    if backend != crate::storage::DbBackend::Mysql {
        let (sql, values) = build_sqlx(backend, &mut archive_returning_query(spec, id, backend));
        let archived: Vec<T> = tx.fetch_all_as(&sql, values).await?;
        return newest_or_no_active(archived, spec, id);
    }
    archive_latest_active_mysql(tx, spec, id).await
}

/// Archive every active version and return the archived rows.
///
/// On SQLite the statement also stamps `updated_at` itself: the column is
/// normally maintained by an AFTER UPDATE trigger whose second write
/// RETURNING cannot see (Postgres' BEFORE trigger needs no such help).
fn archive_returning_query(
    spec: &VersionedSpec,
    id: &str,
    backend: crate::storage::DbBackend,
) -> sea_query::UpdateStatement {
    let mut update = archive_actives_query(spec, id, None);
    if backend == crate::storage::DbBackend::Sqlite {
        update.value(
            spec.updated_at_col.clone(),
            Expr::cust(super::helpers::sql_now(backend)),
        );
    }
    update.returning_all();
    update
}

/// MySQL has no RETURNING: find the newest active version, archive every
/// active one, and read the archived row back — all in one transaction, so
/// the read-back cannot pick up a concurrent writer's row.
async fn archive_latest_active_mysql<T: DbRow + HasVersion>(
    tx: &mut DbTransaction,
    spec: &VersionedSpec,
    id: &str,
) -> Result<T, OrionError> {
    let backend = tx.backend();
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
    let active: T = fetch_required_tx(tx, &sql, values, || no_active(spec, id)).await?;

    let (sql, values) = build_sqlx(backend, &mut archive_actives_query(spec, id, None));
    tx.execute_query(&sql, values).await?;

    get_version_tx(tx, spec, id, active.version()).await
}

/// 404, not 400: every sibling lookup in this module maps a missing row to
/// NotFound, and "which status code does a missing entity give?" should not
/// depend on which repository method you happened to call (proposal G7).
fn no_active(spec: &VersionedSpec, id: &str) -> OrionError {
    OrionError::NotFound(format!("No active version found for {} '{id}'", spec.noun))
}

/// The newest of the rows an archive returned, or the `no_active` refusal when
/// it archived nothing.
fn newest_or_no_active<T: HasVersion>(
    archived: Vec<T>,
    spec: &VersionedSpec,
    id: &str,
) -> Result<T, OrionError> {
    archived
        .into_iter()
        .max_by_key(|row| row.version())
        .ok_or_else(|| no_active(spec, id))
}

/// The one field [`archive_latest_active`] needs off the fetched row.
pub(crate) trait HasVersion {
    fn version(&self) -> i64;
}

// D18: `paginate` used to live here despite having nothing to do with
// versioned entities, and five of the seven list paths could not reach it. It
// is now `helpers::paginate`.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::schema::Channels;
    use sea_query::IntoIden;

    fn channels_spec() -> VersionedSpec {
        VersionedSpec {
            table: Channels::Table.into_iden(),
            id_col: Channels::ChannelId.into_iden(),
            version_col: Channels::Version.into_iden(),
            status_col: Channels::Status.into_iden(),
            priority_col: Some(Channels::Priority.into_iden()),
            updated_at_col: Channels::UpdatedAt.into_iden(),
            label: "Channel",
            noun: "channel",
        }
    }

    /// The predicate is a correlated `MAX(version)` against the same table,
    /// which is the view body rewritten as a `WHERE` — asserted on the text so
    /// a rewrite that quietly drops the correlation (and so matches the global
    /// maximum, i.e. one row for the whole table) is a red test.
    #[test]
    fn the_current_version_predicate_correlates_on_the_id_column() {
        let sql = Query::select()
            .column(Asterisk)
            .from(channels_spec().table.clone())
            .and_where(is_current_version(&channels_spec()))
            .to_string(sea_query::SqliteQueryBuilder);
        assert_eq!(
            sql,
            r#"SELECT * FROM "channels" WHERE "version" = (SELECT MAX("version") FROM "channels" AS "current_v" WHERE "current_v"."channel_id" = "channels"."channel_id")"#
        );
    }

    /// Seed one channel row directly, so the version/status shape under test
    /// is stated rather than arrived at through the lifecycle methods.
    async fn seed(pool: &DbPool, id: &str, version: i64, status: &str) {
        let (sql, values) = build_sqlx(
            pool.backend(),
            Query::insert()
                .into_table(Channels::Table)
                .columns([
                    Channels::ChannelId,
                    Channels::Version,
                    Channels::Name,
                    Channels::ChannelType,
                    Channels::Protocol,
                    Channels::Status,
                    Channels::ConfigJson,
                ])
                .values_panic([
                    id.into(),
                    version.into(),
                    format!("{id} v{version}").into(),
                    "sync".into(),
                    "rest".into(),
                    status.into(),
                    "{}".into(),
                ]),
        );
        pool.execute_query(&sql, values).await.expect("seed");
    }

    async fn statuses(pool: &DbPool, id: &str) -> Vec<(i64, String)> {
        let (sql, values) = build_sqlx(
            pool.backend(),
            Query::select()
                .column(Channels::Version)
                .column(Channels::Status)
                .from(Channels::Table)
                .and_where(Expr::col(Channels::ChannelId).eq(id))
                .order_by(Channels::Version, Order::Asc),
        );
        pool.fetch_all_as::<(i64, String)>(&sql, values)
            .await
            .expect("read")
    }

    /// `archive_latest_active` returns the *newest* archived version, and
    /// archives every active one — not just the newest. A partial rollout has
    /// two active versions at once, so archiving one of them and leaving the
    /// other serving is the failure this guards.
    #[tokio::test]
    async fn archiving_takes_every_active_version_and_returns_the_newest() {
        let pool = crate::storage::test_sqlite_pool().await;
        seed(&pool, "chan-arch", 1, "archived").await;
        seed(&pool, "chan-arch", 2, "active").await;
        seed(&pool, "chan-arch", 3, "active").await;

        let archived: crate::storage::models::Channel =
            archive_latest_active(&pool, &channels_spec(), "chan-arch")
                .await
                .expect("archive");
        assert_eq!(archived.version, 3, "the newest active version is returned");

        assert_eq!(
            statuses(&pool, "chan-arch").await,
            vec![
                (1, "archived".to_string()),
                (2, "archived".to_string()),
                (3, "archived".to_string()),
            ],
            "every active version must be archived, not only the newest"
        );
    }

    /// Nothing active is a `NotFound`, not an `Ok` with a stale row — the
    /// distinction the admin route turns into a 404.
    #[tokio::test]
    async fn archiving_with_nothing_active_is_not_found() {
        let pool = crate::storage::test_sqlite_pool().await;
        seed(&pool, "chan-draft-only", 1, "draft").await;

        let err = archive_latest_active::<crate::storage::models::Channel>(
            &pool,
            &channels_spec(),
            "chan-draft-only",
        )
        .await
        .expect_err("no active version must be NotFound");
        assert!(
            matches!(err, OrionError::NotFound(ref m) if m.contains("No active version")),
            "got: {err:?}"
        );
    }

    /// §2.6: the transactional twin must archive exactly what the pooled one
    /// does. They are separate code paths — the pooled form runs the statement
    /// on the pool, the tx form inside the caller's transaction — so the
    /// equivalence is asserted rather than assumed.
    #[tokio::test]
    async fn the_transactional_archive_matches_the_pooled_one() {
        let pool = crate::storage::test_sqlite_pool().await;
        seed(&pool, "chan-tx", 1, "active").await;
        seed(&pool, "chan-tx", 2, "active").await;

        let mut tx = pool.begin_write_tx().await.expect("tx");
        let archived: crate::storage::models::Channel =
            archive_latest_active_tx(&mut tx, &channels_spec(), "chan-tx")
                .await
                .expect("archive");
        tx.commit().await.expect("commit");

        assert_eq!(archived.version, 2);
        assert_eq!(
            statuses(&pool, "chan-tx").await,
            vec![(1, "archived".to_string()), (2, "archived".to_string())]
        );
    }

    /// A delete takes the whole version history, and a delete of an id that
    /// was never there is `NotFound` rather than a silent success.
    #[tokio::test]
    async fn deleting_removes_every_version_and_a_miss_is_not_found() {
        let pool = crate::storage::test_sqlite_pool().await;
        seed(&pool, "chan-del", 1, "archived").await;
        seed(&pool, "chan-del", 2, "active").await;

        delete_all_versions(&pool, &channels_spec(), "chan-del")
            .await
            .expect("delete");
        assert!(statuses(&pool, "chan-del").await.is_empty());

        assert!(
            matches!(
                delete_all_versions(&pool, &channels_spec(), "never-existed").await,
                Err(OrionError::NotFound(_))
            ),
            "deleting an absent id must be NotFound, not a no-op success"
        );
    }

    /// The transactional delete rolls back with its transaction — which is
    /// what makes it safe to pair with an audit row (§2.6).
    #[tokio::test]
    async fn a_rolled_back_transactional_delete_leaves_the_rows() {
        let pool = crate::storage::test_sqlite_pool().await;
        seed(&pool, "chan-keep", 1, "active").await;

        let mut tx = pool.begin_write_tx().await.expect("tx");
        delete_all_versions_tx(&mut tx, &channels_spec(), "chan-keep")
            .await
            .expect("delete");
        drop(tx);

        assert_eq!(
            statuses(&pool, "chan-keep").await,
            vec![(1, "active".to_string())],
            "a dropped transaction must not have deleted anything"
        );
    }

    /// §5: the predicate must select exactly the rows `current_channels`
    /// serves. Both are run against the same database, so this is the
    /// equivalence the migration away from the views rests on — not a claim in
    /// a comment.
    ///
    /// The fixture is deliberately awkward: three ids, differing version
    /// counts, and versions that are *not* dense (a gap at `chan-b` v2), so a
    /// predicate that picked "the highest version in the table" or "version =
    /// count of versions" would disagree here.
    #[tokio::test]
    async fn the_predicate_and_the_view_select_the_same_rows() {
        let pool = crate::storage::test_sqlite_pool().await;
        for (id, version, status) in [
            ("chan-a", 1, "archived"),
            ("chan-a", 2, "active"),
            ("chan-b", 1, "archived"),
            ("chan-b", 3, "draft"),
            ("chan-c", 7, "active"),
        ] {
            let (sql, values) = build_sqlx(
                pool.backend(),
                Query::insert()
                    .into_table(Channels::Table)
                    .columns([
                        Channels::ChannelId,
                        Channels::Version,
                        Channels::Name,
                        Channels::ChannelType,
                        Channels::Protocol,
                        Channels::Status,
                        Channels::ConfigJson,
                    ])
                    .values_panic([
                        id.into(),
                        version.into(),
                        format!("{id} v{version}").into(),
                        "sync".into(),
                        "rest".into(),
                        status.into(),
                        "{}".into(),
                    ]),
            );
            pool.execute_query(&sql, values).await.expect("seed");
        }

        let read = |stmt: sea_query::SelectStatement| {
            let pool = pool.clone();
            async move {
                let (sql, values) = build_sqlx(pool.backend(), &mut stmt.clone());
                let mut rows: Vec<(String, i64)> = pool
                    .fetch_all_as::<(String, i64)>(&sql, values)
                    .await
                    .expect("read");
                rows.sort();
                rows
            }
        };

        let via_predicate = read(
            Query::select()
                .column(Channels::ChannelId)
                .column(Channels::Version)
                .from(Channels::Table)
                .and_where(is_current_version(&channels_spec()))
                .take(),
        )
        .await;
        let via_view = read(
            Query::select()
                .column(Channels::ChannelId)
                .column(Channels::Version)
                .from(crate::storage::schema::CurrentChannels::Table)
                .take(),
        )
        .await;

        assert_eq!(
            via_predicate,
            vec![
                ("chan-a".to_string(), 2),
                ("chan-b".to_string(), 3),
                ("chan-c".to_string(), 7),
            ],
            "the predicate must select the highest version of each id"
        );
        assert_eq!(
            via_predicate, via_view,
            "the predicate must select exactly what current_channels serves"
        );
    }
}
