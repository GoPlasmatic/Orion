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
            let backend = self.pool.backend();
            let mut tx = self.pool.begin_write_tx().await?;

            let (sql, values) =
                build_sqlx(self.pool.backend(), &mut receipt_select(name, &req.version));
            let existing: Option<PackageReceipt> = tx.fetch_optional_as(&sql, values).await?;

            match existing {
                None => {
                    let (sql, values) = build_sqlx(
                        self.pool.backend(),
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
                        self.pool.backend(),
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
                        self.pool.backend(),
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
            let (sql, values) =
                build_sqlx(self.pool.backend(), &mut receipt_select(name, &req.version));
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
                self.pool.backend(),
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
                self.pool.backend(),
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn repo() -> SqlPackageRepository {
        SqlPackageRepository::new(crate::storage::test_sqlite_pool().await)
    }

    fn put(version: &str, hash: &str, state: PackageState) -> PutPackageReceiptRequest {
        PutPackageReceiptRequest {
            version: version.to_string(),
            content_hash: hash.to_string(),
            state,
        }
    }

    /// The receipt table's whole purpose is the applied-version immutability
    /// rule, and every branch of it lives in one `match` on the existing row.
    /// Each arm gets an assertion here, in the order the trait doc states
    /// them, because the arms differ only in which predicate the write
    /// carries — a difference no type checks.
    #[tokio::test]
    async fn a_new_version_is_inserted_as_requested() {
        let repo = repo().await;
        let receipt = repo
            .put(
                "orders",
                &put("1.0.0", "sha256:aaa", PackageState::Staged),
                "ci",
            )
            .await
            .expect("insert");
        assert_eq!(receipt.version, "1.0.0");
        assert_eq!(receipt.content_hash, "sha256:aaa");
        assert_eq!(receipt.state, PackageState::Staged.as_str());
        assert_eq!(receipt.principal, "ci");
    }

    /// A staged version is a draft: content may still change under it, and it
    /// may be promoted. Neither is a conflict.
    #[tokio::test]
    async fn a_staged_version_is_updated_in_place() {
        let repo = repo().await;
        repo.put(
            "orders",
            &put("1.0.0", "sha256:aaa", PackageState::Staged),
            "ci",
        )
        .await
        .expect("stage");

        let restaged = repo
            .put(
                "orders",
                &put("1.0.0", "sha256:bbb", PackageState::Staged),
                "ci",
            )
            .await
            .expect("re-stage with new content");
        assert_eq!(
            restaged.content_hash, "sha256:bbb",
            "a staged version's content is still mutable"
        );

        let applied = repo
            .put(
                "orders",
                &put("1.0.0", "sha256:bbb", PackageState::Applied),
                "ci",
            )
            .await
            .expect("promote");
        assert_eq!(applied.state, PackageState::Applied.as_str());
    }

    /// Re-applying the identical artifact is the retry a promotion pipeline
    /// performs, so it must be a no-op and not a 409.
    #[tokio::test]
    async fn re_applying_the_same_content_is_idempotent() {
        let repo = repo().await;
        repo.put(
            "orders",
            &put("1.0.0", "sha256:aaa", PackageState::Applied),
            "ci",
        )
        .await
        .expect("apply");
        let again = repo
            .put(
                "orders",
                &put("1.0.0", "sha256:aaa", PackageState::Applied),
                "ci",
            )
            .await
            .expect("re-applying identical content must be accepted");
        assert_eq!(again.state, PackageState::Applied.as_str());
        assert_eq!(again.content_hash, "sha256:aaa");
    }

    /// The rule the table exists for: an applied version's content is fixed.
    /// Different bytes under the same version is the mistake that makes a
    /// promotion receipt worthless, so it is refused rather than recorded.
    #[tokio::test]
    async fn applied_content_cannot_change_under_the_same_version() {
        let repo = repo().await;
        repo.put(
            "orders",
            &put("1.0.0", "sha256:aaa", PackageState::Applied),
            "ci",
        )
        .await
        .expect("apply");

        let err = repo
            .put(
                "orders",
                &put("1.0.0", "sha256:zzz", PackageState::Applied),
                "ci",
            )
            .await
            .expect_err("different content under an applied version must conflict");
        assert!(
            matches!(err, OrionError::Conflict(ref m) if m.contains("immutable")),
            "expected the immutability conflict, got: {err:?}"
        );

        // And the stored row is the one that was applied, not the refused one.
        let stored = repo.get_by_name("orders").await.expect("read back");
        assert_eq!(stored[0].content_hash, "sha256:aaa");
    }

    /// An applied version cannot be demoted back to staged: the receipt is the
    /// record that this content ran here, and a later `staged` PUT would erase
    /// that without changing what is deployed.
    #[tokio::test]
    async fn an_applied_version_cannot_be_demoted_to_staged() {
        let repo = repo().await;
        repo.put(
            "orders",
            &put("1.0.0", "sha256:aaa", PackageState::Applied),
            "ci",
        )
        .await
        .expect("apply");
        assert!(
            repo.put(
                "orders",
                &put("1.0.0", "sha256:aaa", PackageState::Staged),
                "ci"
            )
            .await
            .is_err(),
            "an applied version must not be demotable to staged"
        );
    }

    /// `get_by_name` is newest-first, and a package with no receipts is a
    /// `NotFound` rather than an empty list — the distinction the admin route
    /// turns into 404 versus 200.
    #[tokio::test]
    async fn receipts_read_back_newest_first_and_a_miss_is_not_found() {
        let repo = repo().await;
        for version in ["1.0.0", "1.1.0", "1.2.0"] {
            repo.put(
                "orders",
                &put(version, "sha256:aaa", PackageState::Applied),
                "ci",
            )
            .await
            .expect("apply");
        }

        let receipts = repo.get_by_name("orders").await.expect("read back");
        let versions: Vec<&str> = receipts.iter().map(|r| r.version.as_str()).collect();
        assert_eq!(versions, ["1.2.0", "1.1.0", "1.0.0"]);

        assert!(
            matches!(
                repo.get_by_name("no-such-package").await,
                Err(OrionError::NotFound(_))
            ),
            "a package with no receipts must be NotFound, not an empty list"
        );
    }

    /// Receipts for different packages do not collide on version: the key is
    /// `(name, version)`, and a shared version string is the normal case in an
    /// estate that versions its packages together.
    #[tokio::test]
    async fn the_receipt_key_is_name_and_version_together() {
        let repo = repo().await;
        repo.put(
            "orders",
            &put("1.0.0", "sha256:aaa", PackageState::Applied),
            "ci",
        )
        .await
        .expect("apply orders");
        repo.put(
            "billing",
            &put("1.0.0", "sha256:bbb", PackageState::Applied),
            "ci",
        )
        .await
        .expect("a different package at the same version must not conflict");

        assert_eq!(
            repo.get_by_name("billing").await.expect("read back")[0].content_hash,
            "sha256:bbb"
        );
        let page = repo.list(50, 0).await.expect("list");
        assert_eq!(page.total, 2);
    }
}
