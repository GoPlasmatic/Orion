//! Package receipts (K14): the storage half of the applied-version
//! immutability rule.
//!
//! A receipt records that a package artifact — a bundle of connectors,
//! workflows and channels the `orion-server package` CLI stages and activates
//! through the per-kind admin APIs — landed here, at which version, with what
//! canonical content hash. The rule this repository enforces is the entity
//! lifecycle one level up: a `staged` receipt may be re-put with new content
//! (only a draft can be updated), an `applied` receipt is immutable — the same
//! version arriving with a different hash is a 409, and the fix is a package
//! version bump.

use async_trait::async_trait;
use sea_query::{Asterisk, Expr, ExprTrait, Order, Query};
use serde::Deserialize;

use crate::errors::OrionError;
use crate::storage::models::{PackageReceipt, PackageState};
use crate::storage::{DbPool, build_sqlx, schema::Packages};

use super::helpers::{fetch_required_tx, sql_now};

// -- DTOs --

/// Body of `PUT /api/v1/admin/packages/{name}`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct PutPackageReceiptRequest {
    /// Package version this receipt records, e.g. `1.4.0`.
    pub version: String,
    /// Canonical content hash of the artifact, e.g. `sha256:…`. Compared for
    /// equality against later PUTs of the same version; never parsed.
    pub content_hash: String,
    /// `staged` before the artifact's entities are activated, `applied` after.
    pub state: PackageState,
}

// -- Repository trait --

#[async_trait]
pub trait PackageRepository: Send + Sync {
    /// Record (or advance) the receipt for one package version, enforcing
    /// applied-version immutability:
    ///
    /// - no row for `(name, version)` → insert as requested;
    /// - existing `staged` row → update content/state in place;
    /// - existing `applied` row, same hash → touch (idempotent re-apply;
    ///   `staged` is refused — an applied version cannot be demoted);
    /// - existing `applied` row, different hash → `Conflict`.
    ///
    /// Every write carries its state predicate, so two concurrent PUTs cannot
    /// lose an update: the one that finds the row no longer in the state it
    /// read gets a `Conflict` instead of silently overwriting.
    async fn put(
        &self,
        name: &str,
        req: &PutPackageReceiptRequest,
        principal: &str,
    ) -> Result<PackageReceipt, OrionError>;
    /// One page of receipt rows, ordered by name then newest first.
    async fn list(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<super::helpers::PaginatedResult<PackageReceipt>, OrionError>;
    /// All of one package's receipts, newest first. `NotFound` when none.
    async fn get_by_name(&self, name: &str) -> Result<Vec<PackageReceipt>, OrionError>;
}

// -- SQL implementation --

pub struct SqlPackageRepository {
    pool: DbPool,
}

impl SqlPackageRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

/// The `SELECT * WHERE name = ? AND version = ?` single-receipt shape.
fn receipt_select(name: &str, version: &str) -> sea_query::SelectStatement {
    Query::select()
        .column(Asterisk)
        .from(Packages::Table)
        .and_where(Expr::col(Packages::Name).eq(name))
        .and_where(Expr::col(Packages::Version).eq(version))
        .to_owned()
}

/// The immutability refusal, worded for the operator who has to fix it.
fn applied_conflict(name: &str, version: &str) -> OrionError {
    OrionError::Conflict(format!(
        "package '{name}' version '{version}' is already applied here with different \
         content — an applied package version is immutable; bump the package version"
    ))
}

#[async_trait]
impl PackageRepository for SqlPackageRepository {
    async fn put(
        &self,
        name: &str,
        req: &PutPackageReceiptRequest,
        principal: &str,
    ) -> Result<PackageReceipt, OrionError> {
        crate::metrics::timed_db_op("packages.put", async {
            let backend = crate::storage::get_backend();
            let mut tx = self.pool.begin_write_tx().await?;

            let (sql, values) = build_sqlx(&mut receipt_select(name, &req.version));
            let existing: Option<PackageReceipt> = tx.fetch_optional_as(&sql, values).await?;

            match existing {
                None => {
                    let (sql, values) = build_sqlx(
                        Query::insert()
                            .into_table(Packages::Table)
                            .columns([
                                Packages::Name,
                                Packages::Version,
                                Packages::ContentHash,
                                Packages::State,
                                Packages::Principal,
                            ])
                            .values_panic([
                                name.into(),
                                req.version.as_str().into(),
                                req.content_hash.as_str().into(),
                                req.state.as_str().into(),
                                principal.into(),
                            ]),
                    );
                    // A concurrent PUT that inserted first surfaces as the
                    // PK collision here — a retry will read its row.
                    tx.execute_query(&sql, values).await.map_err(|e| {
                        super::helpers::map_duplicate(e, || {
                            format!(
                                "package '{name}' version '{}' was recorded by a \
                                 concurrent request — retry to read it",
                                req.version
                            )
                        })
                    })?;
                }
                Some(row) if row.state == PackageState::Applied.as_str() => {
                    if row.content_hash != req.content_hash {
                        return Err(applied_conflict(name, &req.version));
                    }
                    if req.state == PackageState::Staged {
                        return Err(OrionError::Conflict(format!(
                            "package '{name}' version '{}' is already applied — an \
                             applied package version cannot go back to staged",
                            req.version
                        )));
                    }
                    // Idempotent re-apply: touch updated_at so this version
                    // becomes the newest applied receipt again (the rollback
                    // path re-applies an older version). The state and hash
                    // predicates make the touch a no-op — refused, not
                    // absorbed — if the row changed underneath us.
                    let (sql, values) = build_sqlx(
                        Query::update()
                            .table(Packages::Table)
                            .value(Packages::Principal, principal)
                            .value(Packages::UpdatedAt, Expr::cust(sql_now(backend)))
                            .and_where(Expr::col(Packages::Name).eq(name))
                            .and_where(Expr::col(Packages::Version).eq(req.version.as_str()))
                            .and_where(
                                Expr::col(Packages::State).eq(PackageState::Applied.as_str()),
                            )
                            .and_where(
                                Expr::col(Packages::ContentHash).eq(req.content_hash.as_str()),
                            ),
                    );
                    if tx.execute_query(&sql, values).await? == 0 {
                        return Err(applied_conflict(name, &req.version));
                    }
                }
                Some(_) => {
                    // Staged: content and state may move — only a draft can be
                    // updated. The state predicate refuses the update if a
                    // concurrent PUT applied this version after our read.
                    let (sql, values) = build_sqlx(
                        Query::update()
                            .table(Packages::Table)
                            .value(Packages::ContentHash, req.content_hash.as_str())
                            .value(Packages::State, req.state.as_str())
                            .value(Packages::Principal, principal)
                            .value(Packages::UpdatedAt, Expr::cust(sql_now(backend)))
                            .and_where(Expr::col(Packages::Name).eq(name))
                            .and_where(Expr::col(Packages::Version).eq(req.version.as_str()))
                            .and_where(
                                Expr::col(Packages::State).eq(PackageState::Staged.as_str()),
                            ),
                    );
                    if tx.execute_query(&sql, values).await? == 0 {
                        return Err(OrionError::Conflict(format!(
                            "package '{name}' version '{}' was applied by a concurrent \
                             request — re-check its receipt before writing again",
                            req.version
                        )));
                    }
                }
            }

            // D23: the row this PUT produced, read inside the same transaction.
            let (sql, values) = build_sqlx(&mut receipt_select(name, &req.version));
            let row = fetch_required_tx(&mut tx, &sql, values, || {
                OrionError::internal(format!(
                    "package receipt '{name}' version '{}' vanished mid-write",
                    req.version
                ))
            })
            .await?;
            tx.commit().await?;
            Ok(row)
        })
        .await
    }

    async fn list(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<super::helpers::PaginatedResult<PackageReceipt>, OrionError> {
        crate::metrics::timed_db_op("packages.list", async {
            let total = super::helpers::count_where(
                &self.pool,
                Packages::Table,
                sea_query::Condition::all(),
            )
            .await?;
            let (sql, values) = build_sqlx(
                Query::select()
                    .column(Asterisk)
                    .from(Packages::Table)
                    .order_by(Packages::Name, Order::Asc)
                    .order_by(Packages::UpdatedAt, Order::Desc)
                    // Unique tiebreaker so OFFSET paging cannot skip or repeat
                    // rows when updated_at collides.
                    .order_by(Packages::Version, Order::Asc)
                    .limit(limit as u64)
                    .offset(offset as u64),
            );
            let data: Vec<PackageReceipt> = self.pool.fetch_all_as(&sql, values).await?;
            Ok(super::helpers::PaginatedResult {
                data,
                total,
                limit,
                offset,
            })
        })
        .await
    }

    async fn get_by_name(&self, name: &str) -> Result<Vec<PackageReceipt>, OrionError> {
        crate::metrics::timed_db_op("packages.get_by_name", async {
            let (sql, values) = build_sqlx(
                Query::select()
                    .column(Asterisk)
                    .from(Packages::Table)
                    .and_where(Expr::col(Packages::Name).eq(name))
                    .order_by(Packages::UpdatedAt, Order::Desc)
                    .order_by(Packages::Version, Order::Desc),
            );
            let rows: Vec<PackageReceipt> = self.pool.fetch_all_as(&sql, values).await?;
            if rows.is_empty() {
                return Err(OrionError::NotFound(format!(
                    "Package '{name}' has no receipts"
                )));
            }
            Ok(rows)
        })
        .await
    }
}
