use chrono::NaiveDateTime;
use sea_query::{Expr, ExprTrait, LockType, OnConflict, Query};

use super::super::{
    ConnectionRevisionRow, DeploymentError, DeploymentErrorKind, DeploymentReceipt,
    DeploymentRevisionRow, ProviderAdmissionRow, ProviderPackageRow,
};
use super::reads::select_head;
use super::{HEAD_ROW, HeadRecord, IN_CHUNK, execute, fetch_optional, insert_if_absent, to_i64};
use crate::storage::schema::{
    ConnectionRevisions, DeploymentHead, DeploymentMembers, DeploymentOperations,
    DeploymentRevisions, ProviderAdmissions, ProviderPackages,
};
use crate::storage::{DbTransaction, build_sqlx};

/// The head, locked for the rest of the transaction: `FOR UPDATE` on
/// PostgreSQL and MySQL (SQLite's `BEGIN IMMEDIATE` already holds the write
/// lock, and sea-query renders no lock clause for it). Every mutation that
/// moves the head or the epoch takes this first, so concurrent ones queue
/// here and each then reads what the one before it committed.
pub(in super::super) async fn lock_head(
    tx: &mut DbTransaction,
) -> Result<Option<(u64, u64)>, DeploymentError> {
    let mut stmt = select_head();
    stmt.lock(LockType::Update);
    Ok(fetch_optional::<HeadRecord>(tx, &mut stmt)
        .await?
        .map(|h| h.pair()))
}

/// Move the head from `observed` (what [`lock_head`] read) to `next`: the
/// compare-and-swap `UPDATE … WHERE head_revision = observed`, which must
/// touch exactly one row, or the first row's INSERT.
///
/// Two writers that both read no head both insert one, and the loser's
/// unique violation is the same lost race the CAS reports for every later
/// write: a conflict on the base revision, not a storage failure.
pub(in super::super) async fn write_head(
    tx: &mut DbTransaction,
    observed: Option<(u64, u64)>,
    next: (u64, u64),
) -> Result<(), DeploymentError> {
    let backend = tx.backend();
    let conflict = || {
        DeploymentError::new(
            DeploymentErrorKind::DeploymentHeadConflict,
            "concurrent modification of deployment_head detected",
        )
    };
    match observed {
        None => {
            let (sql, values) = build_sqlx(
                backend,
                Query::insert()
                    .into_table(DeploymentHead::Table)
                    .columns([
                        DeploymentHead::SingletonId,
                        DeploymentHead::HeadRevision,
                        DeploymentHead::Epoch,
                    ])
                    .values_panic([
                        HEAD_ROW.into(),
                        to_i64(next.0).into(),
                        to_i64(next.1).into(),
                    ]),
            );
            match tx.execute_query(&sql, values).await {
                Ok(_) => Ok(()),
                Err(e) if lost_insert_race(&e) => Err(conflict()),
                Err(e) => Err(DeploymentError::write(e)),
            }
        }
        Some((head, _)) => {
            let (sql, values) = build_sqlx(
                backend,
                Query::update()
                    .table(DeploymentHead::Table)
                    .value(DeploymentHead::HeadRevision, to_i64(next.0))
                    .value(DeploymentHead::Epoch, to_i64(next.1))
                    .value(DeploymentHead::UpdatedAt, Expr::current_timestamp())
                    .and_where(Expr::col(DeploymentHead::SingletonId).eq(HEAD_ROW))
                    .and_where(Expr::col(DeploymentHead::HeadRevision).eq(to_i64(head))),
            );
            if execute(tx, &sql, values).await? == 1 {
                Ok(())
            } else {
                Err(conflict())
            }
        }
    }
}

/// Whether the first head row's INSERT lost to a concurrent one: the
/// unique violation PostgreSQL and SQLite raise, or the deadlock MySQL
/// raises instead when both writers' `FOR UPDATE` found no row and took the
/// same gap lock.
fn lost_insert_race(e: &sqlx::Error) -> bool {
    let Some(db) = e.as_database_error() else {
        return false;
    };
    db.is_unique_violation()
        || db
            .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
            .is_some_and(|my| my.number() == 1213)
}

/// Write a committed revision and its member rows, and read back the time
/// the database stamped it with.
pub(in super::super) async fn insert_revision(
    tx: &mut DbTransaction,
    row: &DeploymentRevisionRow,
) -> Result<NaiveDateTime, DeploymentError> {
    let backend = tx.backend();
    let manifest_json = serde_json::to_string(&row.manifest)
        .map_err(|e| DeploymentError::new(DeploymentErrorKind::SerializeError, e.to_string()))?;
    let (sql, values) = build_sqlx(
        backend,
        Query::insert()
            .into_table(DeploymentRevisions::Table)
            .columns([
                DeploymentRevisions::Revision,
                DeploymentRevisions::ParentRevision,
                DeploymentRevisions::ManifestDigest,
                DeploymentRevisions::ManifestJson,
                DeploymentRevisions::Principal,
                DeploymentRevisions::Reason,
                DeploymentRevisions::MembersCompacted,
            ])
            .values_panic([
                to_i64(row.revision).into(),
                row.parent_revision.map(to_i64).into(),
                row.manifest_digest.clone().into(),
                manifest_json.into(),
                row.created_by.clone().into(),
                row.reason.clone().into(),
                row.members_compacted.into(),
            ]),
    );
    match tx.execute_query(&sql, values).await {
        Ok(_) => {}
        // The head CAS ran first in this transaction, so a revision row
        // already there is a writer that got in between: the same lost race.
        Err(e)
            if e.as_database_error()
                .is_some_and(|db| db.is_unique_violation()) =>
        {
            return Err(DeploymentError::new(
                DeploymentErrorKind::DeploymentHeadConflict,
                format!(
                    "conflicting deployment revision {} already exists in database",
                    row.revision
                ),
            ));
        }
        Err(e) => return Err(DeploymentError::write(e)),
    }
    for chunk in row.manifest.members.chunks(IN_CHUNK) {
        let mut insert = Query::insert()
            .into_table(DeploymentMembers::Table)
            .columns([
                DeploymentMembers::DeploymentRevision,
                DeploymentMembers::EntityKind,
                DeploymentMembers::EntityId,
                DeploymentMembers::EntityRevision,
                DeploymentMembers::OwningPackage,
                DeploymentMembers::DefinitionDigest,
            ])
            .to_owned();
        for m in chunk {
            insert.values_panic([
                to_i64(row.revision).into(),
                m.entity.kind.as_str().into(),
                m.entity.id.clone().into(),
                to_i64(m.entity.revision.get()).into(),
                m.owning_package.clone().unwrap_or_default().into(),
                m.definition_digest.clone().into(),
            ]);
        }
        insert_if_absent(
            tx,
            insert,
            [
                DeploymentMembers::DeploymentRevision,
                DeploymentMembers::EntityKind,
                DeploymentMembers::EntityId,
                DeploymentMembers::EntityRevision,
            ],
        )
        .await?;
    }
    let (at,): (NaiveDateTime,) = fetch_optional(
        tx,
        Query::select()
            .column(DeploymentRevisions::CreatedAt)
            .from(DeploymentRevisions::Table)
            .and_where(Expr::col(DeploymentRevisions::Revision).eq(to_i64(row.revision))),
    )
    .await?
    .ok_or_else(|| DeploymentError::corrupt(format!("revision {} vanished", row.revision)))?;
    Ok(at)
}

/// Mark `revisions` compacted and drop their member rows.
pub(in super::super) async fn compact(
    tx: &mut DbTransaction,
    revisions: &[u64],
) -> Result<(), DeploymentError> {
    let backend = tx.backend();
    for chunk in revisions.chunks(IN_CHUNK) {
        let keys: Vec<i64> = chunk.iter().copied().map(to_i64).collect();
        let (sql, values) = build_sqlx(
            backend,
            Query::update()
                .table(DeploymentRevisions::Table)
                .value(DeploymentRevisions::MembersCompacted, true)
                .and_where(Expr::col(DeploymentRevisions::Revision).is_in(keys.clone())),
        );
        execute(tx, &sql, values).await?;
        let (sql, values) = build_sqlx(
            backend,
            Query::delete()
                .from_table(DeploymentMembers::Table)
                .and_where(Expr::col(DeploymentMembers::DeploymentRevision).is_in(keys)),
        );
        execute(tx, &sql, values).await?;
    }
    Ok(())
}

/// Write a receipt, and read back the time the database stamped it with.
pub(in super::super) async fn insert_receipt(
    tx: &mut DbTransaction,
    receipt: &DeploymentReceipt,
) -> Result<NaiveDateTime, DeploymentError> {
    let (sql, values) = build_sqlx(
        tx.backend(),
        Query::insert()
            .into_table(DeploymentOperations::Table)
            .columns([
                DeploymentOperations::Principal,
                DeploymentOperations::IdempotencyKey,
                DeploymentOperations::RequestHash,
                DeploymentOperations::CommittedRevision,
                DeploymentOperations::ManifestDigest,
                DeploymentOperations::OperationId,
                DeploymentOperations::Epoch,
                DeploymentOperations::ReceivingNodeId,
            ])
            .values_panic([
                receipt.principal.clone().into(),
                receipt.idempotency_key.clone().into(),
                receipt.request_hash.clone().into(),
                to_i64(receipt.committed_revision).into(),
                receipt.manifest_digest.clone().into(),
                receipt.operation_id.clone().into(),
                to_i64(receipt.epoch).into(),
                receipt.receiving_node_id.clone().into(),
            ]),
    );
    execute(tx, &sql, values).await?;
    let (at,): (NaiveDateTime,) = fetch_optional(
        tx,
        Query::select()
            .column(DeploymentOperations::CreatedAt)
            .from(DeploymentOperations::Table)
            .and_where(Expr::col(DeploymentOperations::Principal).eq(receipt.principal.as_str()))
            .and_where(
                Expr::col(DeploymentOperations::IdempotencyKey)
                    .eq(receipt.idempotency_key.as_str()),
            ),
    )
    .await?
    .ok_or_else(|| DeploymentError::corrupt("a receipt vanished inside its transaction"))?;
    Ok(at)
}

pub(in super::super) async fn insert_package(
    tx: &mut DbTransaction,
    pkg: &ProviderPackageRow,
) -> Result<(), DeploymentError> {
    let tier = pkg
        .distribution_json
        .get("tier")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("community");
    let ser = |v: &serde_json::Value| {
        serde_json::to_string(v)
            .map_err(|e| DeploymentError::new(DeploymentErrorKind::SerializeError, e.to_string()))
    };
    let insert = Query::insert()
        .into_table(ProviderPackages::Table)
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
        .values_panic([
            pkg.package_digest.as_str().into(),
            pkg.publisher.clone().into(),
            pkg.provider_id.as_str().into(),
            pkg.version.clone().into(),
            pkg.implementation.as_str().into(),
            tier.into(),
            ser(&pkg.manifest_json)?.into(),
            ser(&pkg.inventory_json)?.into(),
        ])
        .to_owned();
    insert_if_absent(tx, insert, [ProviderPackages::Digest]).await?;
    Ok(())
}

/// Record `adm`, replacing an earlier admission of the same package.
pub(in super::super) async fn upsert_admission(
    tx: &mut DbTransaction,
    adm: &ProviderAdmissionRow,
) -> Result<(), DeploymentError> {
    let (sql, values) = build_sqlx(
        tx.backend(),
        Query::insert()
            .into_table(ProviderAdmissions::Table)
            .columns([
                ProviderAdmissions::PackageDigest,
                ProviderAdmissions::PolicyId,
                ProviderAdmissions::AdmissionFingerprint,
                ProviderAdmissions::Revoked,
            ])
            .values_panic([
                adm.package_digest.as_str().into(),
                adm.policy_id.clone().into(),
                adm.admission_fingerprint.clone().into(),
                adm.revoked.into(),
            ])
            .on_conflict(
                OnConflict::column(ProviderAdmissions::PackageDigest)
                    .update_columns([
                        ProviderAdmissions::PolicyId,
                        ProviderAdmissions::AdmissionFingerprint,
                        ProviderAdmissions::Revoked,
                    ])
                    .value(ProviderAdmissions::UpdatedAt, Expr::current_timestamp())
                    .to_owned(),
            ),
    );
    execute(tx, &sql, values).await?;
    Ok(())
}

pub(in super::super) async fn revoke(
    tx: &mut DbTransaction,
    digest: &str,
) -> Result<(), DeploymentError> {
    let (sql, values) = build_sqlx(
        tx.backend(),
        Query::update()
            .table(ProviderAdmissions::Table)
            .value(ProviderAdmissions::Revoked, true)
            .value(ProviderAdmissions::UpdatedAt, Expr::current_timestamp())
            .and_where(Expr::col(ProviderAdmissions::PackageDigest).eq(digest)),
    );
    execute(tx, &sql, values).await?;
    Ok(())
}

/// Write `row` unless its `(connection_id, revision)` is already stored.
/// `false` when it was.
pub(in super::super) async fn insert_connection(
    tx: &mut DbTransaction,
    row: &ConnectionRevisionRow,
) -> Result<bool, DeploymentError> {
    let public_config = serde_json::to_string(&row.public_config)
        .map_err(|e| DeploymentError::new(DeploymentErrorKind::SerializeError, e.to_string()))?;
    let insert = Query::insert()
        .into_table(ConnectionRevisions::Table)
        .columns([
            ConnectionRevisions::ConnectionId,
            ConnectionRevisions::Revision,
            ConnectionRevisions::ProviderDigest,
            ConnectionRevisions::PublicConfigJson,
            ConnectionRevisions::SecretLocatorsJson,
            ConnectionRevisions::BindingsJson,
        ])
        .values_panic([
            row.connection_id.as_str().into(),
            to_i64(row.revision.get()).into(),
            row.provider_digest.as_str().into(),
            public_config.into(),
            super::super::locators_column(&row.secret_locators).into(),
            super::super::bindings_column(&row.bindings).into(),
        ])
        .to_owned();
    Ok(insert_if_absent(
        tx,
        insert,
        [
            ConnectionRevisions::ConnectionId,
            ConnectionRevisions::Revision,
        ],
    )
    .await?
        > 0)
}
