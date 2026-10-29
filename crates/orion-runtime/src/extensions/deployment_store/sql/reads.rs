use sea_query::{Expr, ExprTrait, Order, Query, SelectStatement};

use super::super::DeploymentError;
use super::{
    AdmissionRecord, ConnectionRecord, HEAD_ROW, HeadRecord, PackageRecord, ReceiptRecord,
    RevisionRecord, fetch_all, fetch_in_chunks, fetch_optional, stored_revision, to_i64, to_u64,
};
use crate::storage::schema::{
    ConnectionRevisions, DeploymentHead, DeploymentOperations, DeploymentRevisions,
    ProviderAdmissions, ProviderPackages,
};
use crate::storage::{DbPool, DbTransaction, build_sqlx};

pub(super) fn select_head() -> SelectStatement {
    Query::select()
        .columns([DeploymentHead::HeadRevision, DeploymentHead::Epoch])
        .from(DeploymentHead::Table)
        .and_where(Expr::col(DeploymentHead::SingletonId).eq(HEAD_ROW))
        .to_owned()
}

/// The committed head and epoch, without a lock: what a node polls.
pub(in super::super) async fn read_head(pool: &DbPool) -> Result<Option<(u64, u64)>, sqlx::Error> {
    let (sql, values) = build_sqlx(pool.backend(), &mut select_head());
    Ok(pool
        .fetch_optional_as::<HeadRecord>(&sql, values)
        .await?
        .map(|h| h.pair()))
}

/// The head as this transaction sees it, without a lock.
pub(in super::super) async fn head(
    tx: &mut DbTransaction,
) -> Result<Option<(u64, u64)>, sqlx::Error> {
    let (sql, values) = build_sqlx(tx.backend(), &mut select_head());
    Ok(tx
        .fetch_optional_as::<HeadRecord>(&sql, values)
        .await?
        .map(|h| h.pair()))
}

fn select_revisions() -> SelectStatement {
    Query::select()
        .columns([
            DeploymentRevisions::Revision,
            DeploymentRevisions::ParentRevision,
            DeploymentRevisions::ManifestDigest,
            DeploymentRevisions::ManifestJson,
            DeploymentRevisions::Principal,
            DeploymentRevisions::Reason,
            DeploymentRevisions::MembersCompacted,
            DeploymentRevisions::CreatedAt,
        ])
        .from(DeploymentRevisions::Table)
        .to_owned()
}

pub(in super::super) async fn revision(
    tx: &mut DbTransaction,
    revision: u64,
) -> Result<Option<RevisionRecord>, DeploymentError> {
    fetch_optional(
        tx,
        select_revisions().and_where(Expr::col(DeploymentRevisions::Revision).eq(to_i64(revision))),
    )
    .await
}

pub(in super::super) async fn revision_from_pool(
    pool: &DbPool,
    revision: u64,
) -> Result<Option<RevisionRecord>, DeploymentError> {
    let (sql, values) = build_sqlx(
        pool.backend(),
        select_revisions().and_where(Expr::col(DeploymentRevisions::Revision).eq(to_i64(revision))),
    );
    pool.fetch_optional_as(&sql, values)
        .await
        .map_err(DeploymentError::read)
}

/// Every revision above `after`, oldest first.
pub(in super::super) async fn revisions_after(
    tx: &mut DbTransaction,
    after: u64,
) -> Result<Vec<RevisionRecord>, DeploymentError> {
    fetch_all(
        tx,
        select_revisions()
            .and_where(Expr::col(DeploymentRevisions::Revision).gt(to_i64(after)))
            .order_by(DeploymentRevisions::Revision, Order::Asc),
    )
    .await
}

/// Every revision's number and whether its members are compacted, oldest
/// first.
pub(in super::super) async fn revision_flags(
    tx: &mut DbTransaction,
) -> Result<Vec<(u64, bool)>, DeploymentError> {
    let rows: Vec<(i64, bool)> = fetch_all(
        tx,
        Query::select()
            .columns([
                DeploymentRevisions::Revision,
                DeploymentRevisions::MembersCompacted,
            ])
            .from(DeploymentRevisions::Table)
            .order_by(DeploymentRevisions::Revision, Order::Asc),
    )
    .await?;
    Ok(rows.into_iter().map(|(r, c)| (to_u64(r), c)).collect())
}

/// The compacted revisions at or below `through`.
pub(in super::super) async fn compacted_through(
    tx: &mut DbTransaction,
    through: u64,
) -> Result<Vec<u64>, DeploymentError> {
    let rows: Vec<(i64,)> = fetch_all(
        tx,
        Query::select()
            .column(DeploymentRevisions::Revision)
            .from(DeploymentRevisions::Table)
            .and_where(Expr::col(DeploymentRevisions::MembersCompacted).eq(true))
            .and_where(Expr::col(DeploymentRevisions::Revision).lte(to_i64(through))),
    )
    .await?;
    Ok(rows.into_iter().map(|(r,)| to_u64(r)).collect())
}

/// The manifests of `revisions`.
pub(in super::super) async fn manifests(
    tx: &mut DbTransaction,
    revisions: &[u64],
) -> Result<Vec<(u64, super::super::CanonicalManifest)>, DeploymentError> {
    let keys: Vec<i64> = revisions.iter().copied().map(to_i64).collect();
    let rows: Vec<(i64, String)> = fetch_in_chunks(tx, &keys, |chunk| {
        Query::select()
            .columns([
                DeploymentRevisions::Revision,
                DeploymentRevisions::ManifestJson,
            ])
            .from(DeploymentRevisions::Table)
            .and_where(Expr::col(DeploymentRevisions::Revision).is_in(chunk.iter().copied()))
            .to_owned()
    })
    .await?;
    rows.into_iter()
        .map(|(r, json)| {
            let revision = to_u64(r);
            serde_json::from_str(&json)
                .map(|m| (revision, m))
                .map_err(|e| {
                    DeploymentError::corrupt(format!(
                        "corrupt manifest_json for revision {revision}: {e}"
                    ))
                })
        })
        .collect()
}

pub(in super::super) async fn receipt(
    tx: &mut DbTransaction,
    principal: &str,
    idempotency_key: &str,
) -> Result<Option<ReceiptRecord>, DeploymentError> {
    fetch_optional(
        tx,
        Query::select()
            .columns([
                DeploymentOperations::Principal,
                DeploymentOperations::IdempotencyKey,
                DeploymentOperations::RequestHash,
                DeploymentOperations::CommittedRevision,
                DeploymentOperations::ManifestDigest,
                DeploymentOperations::OperationId,
                DeploymentOperations::Epoch,
                DeploymentOperations::ReceivingNodeId,
                DeploymentOperations::CreatedAt,
            ])
            .from(DeploymentOperations::Table)
            .and_where(Expr::col(DeploymentOperations::Principal).eq(principal))
            .and_where(Expr::col(DeploymentOperations::IdempotencyKey).eq(idempotency_key)),
    )
    .await
}

fn select_packages() -> SelectStatement {
    Query::select()
        .columns([
            ProviderPackages::Digest,
            ProviderPackages::Publisher,
            ProviderPackages::ProviderId,
            ProviderPackages::Version,
            ProviderPackages::Implementation,
            ProviderPackages::Tier,
            ProviderPackages::ManifestJson,
            ProviderPackages::InventoryJson,
        ])
        .from(ProviderPackages::Table)
        .to_owned()
}

pub(in super::super) async fn packages(
    tx: &mut DbTransaction,
    digests: &[String],
) -> Result<Vec<PackageRecord>, DeploymentError> {
    fetch_in_chunks(tx, digests, |chunk| {
        select_packages()
            .and_where(Expr::col(ProviderPackages::Digest).is_in(chunk.iter().cloned()))
            .to_owned()
    })
    .await
}

pub(in super::super) async fn all_packages(
    tx: &mut DbTransaction,
) -> Result<Vec<PackageRecord>, DeploymentError> {
    fetch_all(tx, &mut select_packages()).await
}

pub(in super::super) async fn package_digests(
    tx: &mut DbTransaction,
) -> Result<Vec<String>, DeploymentError> {
    let rows: Vec<(String,)> = fetch_all(
        tx,
        Query::select()
            .column(ProviderPackages::Digest)
            .from(ProviderPackages::Table),
    )
    .await?;
    Ok(rows.into_iter().map(|(d,)| d).collect())
}

/// Every package's digest and inventory.
pub(in super::super) async fn package_inventories(
    tx: &mut DbTransaction,
) -> Result<Vec<(String, String)>, DeploymentError> {
    fetch_all(
        tx,
        Query::select()
            .columns([ProviderPackages::Digest, ProviderPackages::InventoryJson])
            .from(ProviderPackages::Table),
    )
    .await
}

/// A package stored under `publisher/provider_id/version` with a digest
/// other than `digest`.
pub(in super::super) async fn other_package_with_identity(
    tx: &mut DbTransaction,
    (publisher, provider_id, version): (&str, &str, &str),
    digest: &str,
) -> Result<Option<String>, DeploymentError> {
    let row: Option<(String,)> = fetch_optional(
        tx,
        Query::select()
            .column(ProviderPackages::Digest)
            .from(ProviderPackages::Table)
            .and_where(Expr::col(ProviderPackages::Publisher).eq(publisher))
            .and_where(Expr::col(ProviderPackages::ProviderId).eq(provider_id))
            .and_where(Expr::col(ProviderPackages::Version).eq(version))
            .and_where(Expr::col(ProviderPackages::Digest).ne(digest))
            .limit(1),
    )
    .await?;
    Ok(row.map(|(d,)| d))
}

fn select_admissions() -> SelectStatement {
    Query::select()
        .columns([
            ProviderAdmissions::PackageDigest,
            ProviderAdmissions::PolicyId,
            ProviderAdmissions::AdmissionFingerprint,
            ProviderAdmissions::Revoked,
        ])
        .from(ProviderAdmissions::Table)
        .to_owned()
}

pub(in super::super) async fn admissions(
    tx: &mut DbTransaction,
    digests: &[String],
) -> Result<Vec<AdmissionRecord>, DeploymentError> {
    fetch_in_chunks(tx, digests, |chunk| {
        select_admissions()
            .and_where(Expr::col(ProviderAdmissions::PackageDigest).is_in(chunk.iter().cloned()))
            .to_owned()
    })
    .await
}

pub(in super::super) async fn all_admissions(
    tx: &mut DbTransaction,
) -> Result<Vec<AdmissionRecord>, DeploymentError> {
    fetch_all(tx, &mut select_admissions()).await
}

fn select_connections() -> SelectStatement {
    Query::select()
        .columns([
            ConnectionRevisions::ConnectionId,
            ConnectionRevisions::Revision,
            ConnectionRevisions::ProviderDigest,
            ConnectionRevisions::PublicConfigJson,
            ConnectionRevisions::SecretLocatorsJson,
            ConnectionRevisions::BindingsJson,
        ])
        .from(ConnectionRevisions::Table)
        .to_owned()
}

pub(in super::super) async fn connection(
    tx: &mut DbTransaction,
    connection_id: &str,
    revision: u64,
) -> Result<Option<ConnectionRecord>, DeploymentError> {
    fetch_optional(
        tx,
        select_connections()
            .and_where(Expr::col(ConnectionRevisions::ConnectionId).eq(connection_id))
            .and_where(Expr::col(ConnectionRevisions::Revision).eq(to_i64(revision))),
    )
    .await
}

/// Every revision of the connections `ids`.
pub(in super::super) async fn connections_of(
    tx: &mut DbTransaction,
    ids: &[String],
) -> Result<Vec<ConnectionRecord>, DeploymentError> {
    fetch_in_chunks(tx, ids, |chunk| {
        select_connections()
            .and_where(Expr::col(ConnectionRevisions::ConnectionId).is_in(chunk.iter().cloned()))
            .to_owned()
    })
    .await
}

pub(in super::super) async fn all_connections(
    tx: &mut DbTransaction,
) -> Result<Vec<ConnectionRecord>, DeploymentError> {
    fetch_all(tx, &mut select_connections()).await
}

pub(in super::super) async fn connection_keys(
    tx: &mut DbTransaction,
) -> Result<Vec<(String, u64)>, DeploymentError> {
    let rows: Vec<(String, i64)> = fetch_all(
        tx,
        Query::select()
            .columns([
                ConnectionRevisions::ConnectionId,
                ConnectionRevisions::Revision,
            ])
            .from(ConnectionRevisions::Table),
    )
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, r)| (id, stored_revision(r).get()))
        .collect())
}
