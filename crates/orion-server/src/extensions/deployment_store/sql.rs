//! The deployment store's SQL: the rows as stored, and every statement over
//! them, in each backend's dialect through sea-query. Nothing here decides
//! anything; [`super::writes`] and [`super::catalog`] do.
//!
//! The only raw SQL is the database clock expressions (`Expr::cust`), which
//! differ per backend and carry no caller value.

use chrono::NaiveDateTime;
use sea_query::{
    Expr, ExprTrait, InsertStatement, LockType, OnConflict, Order, Query, SelectStatement,
};

use super::{
    ArtifactState, ConnectionRevisionRow, DeploymentError, DeploymentErrorKind, DeploymentReceipt,
    DeploymentRevisionRow, ProviderAdmissionRow, ProviderPackageRow,
};
use crate::extensions::identity::{Digest, Revision};
use crate::storage::schema::{
    ArtifactObjects, ConnectionRevisions, DeploymentHead, DeploymentMembers, DeploymentOperations,
    DeploymentRevisions, ProviderAdmissions, ProviderPackages,
};
use crate::storage::{DbBackend, DbPool, DbTransaction, build_sqlx};

/// Values per `IN (…)` list. Every backend's bound-parameter ceiling is far
/// above it; a longer list is sent as several statements.
const IN_CHUNK: usize = 500;

/// The one row of `deployment_head`.
const HEAD_ROW: i32 = 1;

// ============================================================
// Rows as stored
// ============================================================

#[derive(Debug, sqlx::FromRow)]
pub(super) struct HeadRecord {
    pub head_revision: i64,
    pub epoch: i64,
}

impl HeadRecord {
    pub fn pair(&self) -> (u64, u64) {
        (to_u64(self.head_revision), to_u64(self.epoch))
    }
}

#[derive(Debug, sqlx::FromRow)]
pub(super) struct RevisionRecord {
    pub revision: i64,
    pub parent_revision: Option<i64>,
    pub manifest_digest: String,
    pub manifest_json: String,
    pub principal: String,
    pub reason: String,
    pub members_compacted: bool,
    pub created_at: NaiveDateTime,
}

impl RevisionRecord {
    /// The revision as stored, its manifest re-canonicalised and checked
    /// against the digest it was committed under.
    pub fn into_row(self) -> Result<DeploymentRevisionRow, DeploymentError> {
        let revision = to_u64(self.revision);
        let manifest: super::CanonicalManifest = serde_json::from_str(&self.manifest_json)
            .map_err(|e| {
                DeploymentError::corrupt(format!(
                    "corrupt manifest_json for revision {revision}: {e}"
                ))
            })?;
        let (manifest_bytes, computed) = manifest.canonicalize_and_verify().map_err(|e| {
            DeploymentError::corrupt(format!(
                "manifest for revision {revision} does not verify: {e}"
            ))
        })?;
        if computed != self.manifest_digest {
            return Err(DeploymentError::corrupt(format!(
                "revision {revision} manifest_digest mismatch: stored '{}', computed '{computed}'",
                self.manifest_digest
            )));
        }
        Ok(DeploymentRevisionRow {
            revision,
            parent_revision: self.parent_revision.map(to_u64),
            manifest_digest: self.manifest_digest,
            manifest_bytes,
            manifest,
            reason: self.reason,
            created_by: self.principal,
            members_compacted: self.members_compacted,
            created_at: Some(self.created_at),
        })
    }
}

#[derive(Debug, sqlx::FromRow)]
pub(super) struct ReceiptRecord {
    pub principal: String,
    pub idempotency_key: String,
    pub request_hash: String,
    pub committed_revision: i64,
    pub manifest_digest: String,
    pub operation_id: Option<String>,
    pub epoch: Option<i64>,
    pub receiving_node_id: Option<String>,
    pub created_at: NaiveDateTime,
}

impl ReceiptRecord {
    /// The receipt as committed. A row written before the store recorded
    /// `operation_id`, `epoch` and `receiving_node_id` reads back with the
    /// values it was always answered with: the revision for the epoch, and
    /// the reading node.
    pub fn into_receipt(self, legacy_node: &str) -> DeploymentReceipt {
        let revision = to_u64(self.committed_revision);
        let epoch = self.epoch.map_or(revision, to_u64);
        DeploymentReceipt {
            operation_id: self
                .operation_id
                .unwrap_or_else(|| format!("op-{revision}-{epoch}")),
            principal: self.principal,
            idempotency_key: self.idempotency_key,
            request_hash: self.request_hash,
            committed_revision: revision,
            manifest_digest: self.manifest_digest,
            epoch,
            receiving_node_id: self
                .receiving_node_id
                .unwrap_or_else(|| legacy_node.to_string()),
            created_at: Some(self.created_at),
        }
    }
}

#[derive(Debug, sqlx::FromRow)]
pub(super) struct PackageRecord {
    pub digest: String,
    pub publisher: String,
    pub provider_id: String,
    pub version: String,
    pub implementation: String,
    pub tier: String,
    pub manifest_json: String,
    pub inventory_json: String,
}

impl PackageRecord {
    /// The package as stored. `distribution` is its `distribution.json`
    /// when that file is still held; without it the row answers the two
    /// fields the table keeps.
    pub fn into_row(
        self,
        distribution: Option<serde_json::Value>,
    ) -> Result<ProviderPackageRow, DeploymentError> {
        let digest = &self.digest;
        let implementation =
            super::ImplementationKind::parse(&self.implementation).ok_or_else(|| {
                DeploymentError::corrupt(format!(
                    "unknown package implementation '{}' for '{digest}'",
                    self.implementation
                ))
            })?;
        let manifest_json = serde_json::from_str(&self.manifest_json).map_err(|e| {
            DeploymentError::corrupt(format!("corrupt manifest_json for package '{digest}': {e}"))
        })?;
        let inventory_json = serde_json::from_str(&self.inventory_json).map_err(|e| {
            DeploymentError::corrupt(format!(
                "corrupt inventory_json for package '{digest}': {e}"
            ))
        })?;
        let distribution_json = distribution
            .unwrap_or_else(|| serde_json::json!({"publisher": self.publisher, "tier": self.tier}));
        Ok(ProviderPackageRow {
            package_digest: stored_digest(self.digest, "package")?,
            publisher: self.publisher,
            provider_id: self.provider_id.into(),
            version: self.version,
            implementation,
            manifest_json,
            distribution_json,
            inventory_json,
        })
    }
}

#[derive(Debug, sqlx::FromRow)]
pub(super) struct AdmissionRecord {
    pub package_digest: String,
    pub policy_id: String,
    pub admission_fingerprint: String,
    pub revoked: bool,
}

impl TryFrom<AdmissionRecord> for ProviderAdmissionRow {
    type Error = DeploymentError;

    fn try_from(r: AdmissionRecord) -> Result<Self, DeploymentError> {
        Ok(Self {
            package_digest: stored_digest(r.package_digest, "admission")?,
            admission_fingerprint: r.admission_fingerprint,
            policy_id: r.policy_id,
            revoked: r.revoked,
        })
    }
}

/// A digest column read back: written from a checked [`Digest`], so one
/// that no longer parses is corruption.
fn stored_digest(text: String, what: &str) -> Result<Digest, DeploymentError> {
    Digest::try_from(text)
        .map_err(|e| DeploymentError::corrupt(format!("corrupt {what} digest: {e}")))
}

#[derive(Debug, sqlx::FromRow)]
pub(super) struct ConnectionRecord {
    pub connection_id: String,
    pub revision: i64,
    pub provider_digest: String,
    pub public_config_json: String,
    pub secret_locators_json: Option<String>,
    pub bindings_json: Option<String>,
}

impl ConnectionRecord {
    /// The revision as stored, its configuration digest recomputed from what
    /// the row holds.
    pub fn into_row(self) -> Result<ConnectionRevisionRow, DeploymentError> {
        let id = &self.connection_id;
        let revision = stored_revision(self.revision);
        let public_config = serde_json::from_str(&self.public_config_json).map_err(|e| {
            DeploymentError::corrupt(format!(
                "corrupt public_config_json for connection '{id}' revision {revision}: {e}"
            ))
        })?;
        let secret_locators =
            super::locators_from_column(self.secret_locators_json).map_err(|e| {
                DeploymentError::corrupt(format!("connection '{id}' revision {revision}: {e}"))
            })?;
        let bindings = super::bindings_from_column(self.bindings_json).map_err(|e| {
            DeploymentError::corrupt(format!("connection '{id}' revision {revision}: {e}"))
        })?;
        let configuration_digest = super::sha256_digest(&super::connection_config_bytes(
            &public_config,
            &secret_locators,
            &bindings,
        ));
        Ok(ConnectionRevisionRow {
            provider_digest: stored_digest(
                self.provider_digest,
                &format!("connection '{id}' revision {revision} provider"),
            )?,
            connection_id: self.connection_id.into(),
            revision,
            public_config,
            secret_locators,
            bindings,
            configuration_digest,
        })
    }
}

/// An artifact's row without its bytes.
#[derive(Debug, sqlx::FromRow)]
pub(super) struct ArtifactMetaRecord {
    pub digest: String,
    pub size_bytes: i64,
    pub state: String,
    pub drain_epoch: Option<i64>,
}

impl ArtifactMetaRecord {
    pub fn state(&self) -> Result<ArtifactState, DeploymentError> {
        parse_state(&self.state, &self.digest)
    }
}

/// An artifact's row with its bytes.
#[derive(Debug, sqlx::FromRow)]
pub(super) struct ArtifactRecord {
    pub digest: String,
    pub size_bytes: i64,
    pub state: String,
    pub drain_epoch: Option<i64>,
    pub payload_bytes: Option<Vec<u8>>,
}

impl ArtifactRecord {
    /// The artifact as stored. Bytes that do not hash to the digest they are
    /// stored under are refused rather than served.
    pub fn into_row(self) -> Result<super::ArtifactObjectRow, DeploymentError> {
        let state = parse_state(&self.state, &self.digest)?;
        if state != ArtifactState::Deleted
            && let Some(bytes) = &self.payload_bytes
        {
            let computed = super::sha256_digest(bytes);
            if computed != self.digest {
                return Err(DeploymentError::corrupt(format!(
                    "artifact '{}' payload digest mismatch (computed '{computed}')",
                    self.digest
                )));
            }
        }
        Ok(super::ArtifactObjectRow {
            digest: self.digest,
            size_bytes: usize::try_from(self.size_bytes).unwrap_or(0),
            state,
            drain_epoch: self.drain_epoch.map(to_u64),
            bytes: self.payload_bytes,
        })
    }
}

fn parse_state(state: &str, digest: &str) -> Result<ArtifactState, DeploymentError> {
    match state {
        "active" | "ACTIVE" => Ok(ArtifactState::Active),
        "draining" | "DRAINING" => Ok(ArtifactState::Draining),
        "deleted" | "DELETED" => Ok(ArtifactState::Deleted),
        other => Err(DeploymentError::corrupt(format!(
            "unknown artifact state '{other}' for '{digest}'"
        ))),
    }
}

fn state_column(state: ArtifactState) -> &'static str {
    match state {
        ArtifactState::Active => "active",
        ArtifactState::Draining => "draining",
        ArtifactState::Deleted => "deleted",
    }
}

/// A connection revision column read back. Revisions are written from a
/// [`Revision`], so a stored value below 1 is read as the first.
fn stored_revision(v: i64) -> Revision {
    Revision::new(to_u64(v)).unwrap_or(Revision::FIRST)
}

pub(super) fn to_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

// ============================================================
// Execution
// ============================================================

async fn fetch_all<T: crate::storage::DbRow>(
    tx: &mut DbTransaction,
    stmt: &mut SelectStatement,
) -> Result<Vec<T>, DeploymentError> {
    let (sql, values) = build_sqlx(tx.backend(), stmt);
    tx.fetch_all_as(&sql, values)
        .await
        .map_err(DeploymentError::read)
}

async fn fetch_optional<T: crate::storage::DbRow>(
    tx: &mut DbTransaction,
    stmt: &mut SelectStatement,
) -> Result<Option<T>, DeploymentError> {
    let (sql, values) = build_sqlx(tx.backend(), stmt);
    tx.fetch_optional_as(&sql, values)
        .await
        .map_err(DeploymentError::read)
}

/// Run `stmt` once per chunk of `keys`, each time restricted to that chunk
/// by `restrict`, and gather the rows.
async fn fetch_in_chunks<T, K>(
    tx: &mut DbTransaction,
    keys: &[K],
    mut stmt: impl FnMut(&[K]) -> SelectStatement,
) -> Result<Vec<T>, DeploymentError>
where
    T: crate::storage::DbRow,
{
    let mut out = Vec::new();
    for chunk in keys.chunks(IN_CHUNK) {
        out.extend(fetch_all::<T>(tx, &mut stmt(chunk)).await?);
    }
    Ok(out)
}

async fn execute(
    tx: &mut DbTransaction,
    sql: &str,
    values: sea_query_sqlx::SqlxValues,
) -> Result<u64, DeploymentError> {
    tx.execute_query(sql, values)
        .await
        .map_err(DeploymentError::write)
}

/// INSERT that loses silently to an existing row with the same key:
/// `ON CONFLICT DO NOTHING`, or `INSERT IGNORE` on MySQL. Rows affected: 0
/// when the row was already there.
async fn insert_if_absent<C>(
    tx: &mut DbTransaction,
    mut insert: InsertStatement,
    conflict: impl IntoIterator<Item = C>,
) -> Result<u64, DeploymentError>
where
    C: sea_query::IntoIden,
{
    let backend = tx.backend();
    let (sql, values) = match backend {
        DbBackend::Sqlite | DbBackend::Postgres => {
            insert.on_conflict(OnConflict::columns(conflict).do_nothing().to_owned());
            build_sqlx(backend, &mut insert)
        }
        DbBackend::Mysql => {
            let (sql, values) = build_sqlx(backend, &mut insert);
            (sql.replacen("INSERT INTO", "INSERT IGNORE INTO", 1), values)
        }
    };
    execute(tx, &sql, values).await
}

// ============================================================
// deployment_head
// ============================================================

fn select_head() -> SelectStatement {
    Query::select()
        .columns([DeploymentHead::HeadRevision, DeploymentHead::Epoch])
        .from(DeploymentHead::Table)
        .and_where(Expr::col(DeploymentHead::SingletonId).eq(HEAD_ROW))
        .to_owned()
}

/// The committed head and epoch, without a lock: what a node polls.
pub(super) async fn read_head(pool: &DbPool) -> Result<Option<(u64, u64)>, sqlx::Error> {
    let (sql, values) = build_sqlx(pool.backend(), &mut select_head());
    Ok(pool
        .fetch_optional_as::<HeadRecord>(&sql, values)
        .await?
        .map(|h| h.pair()))
}

/// The head as this transaction sees it, without a lock.
pub(super) async fn head(tx: &mut DbTransaction) -> Result<Option<(u64, u64)>, sqlx::Error> {
    let (sql, values) = build_sqlx(tx.backend(), &mut select_head());
    Ok(tx
        .fetch_optional_as::<HeadRecord>(&sql, values)
        .await?
        .map(|h| h.pair()))
}

/// The head, locked for the rest of the transaction: `FOR UPDATE` on
/// PostgreSQL and MySQL (SQLite's `BEGIN IMMEDIATE` already holds the write
/// lock, and sea-query renders no lock clause for it). Every mutation that
/// moves the head or the epoch takes this first, so concurrent ones queue
/// here and each then reads what the one before it committed.
pub(super) async fn lock_head(
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
pub(super) async fn write_head(
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

// ============================================================
// deployment_revisions, deployment_members
// ============================================================

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

pub(super) async fn revision(
    tx: &mut DbTransaction,
    revision: u64,
) -> Result<Option<RevisionRecord>, DeploymentError> {
    fetch_optional(
        tx,
        select_revisions().and_where(Expr::col(DeploymentRevisions::Revision).eq(to_i64(revision))),
    )
    .await
}

pub(super) async fn revision_from_pool(
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
pub(super) async fn revisions_after(
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
pub(super) async fn revision_flags(
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
pub(super) async fn compacted_through(
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
pub(super) async fn manifests(
    tx: &mut DbTransaction,
    revisions: &[u64],
) -> Result<Vec<(u64, super::CanonicalManifest)>, DeploymentError> {
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

/// Write a committed revision and its member rows, and read back the time
/// the database stamped it with.
pub(super) async fn insert_revision(
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
pub(super) async fn compact(
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

// ============================================================
// deployment_operations
// ============================================================

pub(super) async fn receipt(
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

/// Write a receipt, and read back the time the database stamped it with.
pub(super) async fn insert_receipt(
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

// ============================================================
// provider_packages
// ============================================================

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

pub(super) async fn packages(
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

pub(super) async fn all_packages(
    tx: &mut DbTransaction,
) -> Result<Vec<PackageRecord>, DeploymentError> {
    fetch_all(tx, &mut select_packages()).await
}

pub(super) async fn package_digests(
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
pub(super) async fn package_inventories(
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
pub(super) async fn other_package_with_identity(
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

pub(super) async fn insert_package(
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

// ============================================================
// provider_admissions
// ============================================================

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

pub(super) async fn admissions(
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

pub(super) async fn all_admissions(
    tx: &mut DbTransaction,
) -> Result<Vec<AdmissionRecord>, DeploymentError> {
    fetch_all(tx, &mut select_admissions()).await
}

/// Record `adm`, replacing an earlier admission of the same package.
pub(super) async fn upsert_admission(
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

pub(super) async fn revoke(tx: &mut DbTransaction, digest: &str) -> Result<(), DeploymentError> {
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

// ============================================================
// connection_revisions
// ============================================================

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

pub(super) async fn connection(
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
pub(super) async fn connections_of(
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

pub(super) async fn all_connections(
    tx: &mut DbTransaction,
) -> Result<Vec<ConnectionRecord>, DeploymentError> {
    fetch_all(tx, &mut select_connections()).await
}

pub(super) async fn connection_keys(
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

/// Write `row` unless its `(connection_id, revision)` is already stored.
/// `false` when it was.
pub(super) async fn insert_connection(
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
            super::locators_column(&row.secret_locators).into(),
            super::bindings_column(&row.bindings).into(),
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

// ============================================================
// artifact_objects
// ============================================================

fn artifact_meta_columns() -> [ArtifactObjects; 4] {
    [
        ArtifactObjects::Digest,
        ArtifactObjects::SizeBytes,
        ArtifactObjects::State,
        ArtifactObjects::DrainEpoch,
    ]
}

fn select_artifacts_with_bytes(chunk: &[String]) -> SelectStatement {
    Query::select()
        .columns(artifact_meta_columns())
        .column(ArtifactObjects::PayloadBytes)
        .from(ArtifactObjects::Table)
        .and_where(Expr::col(ArtifactObjects::Digest).is_in(chunk.iter().cloned()))
        .to_owned()
}

/// The rows of `digests`, without their bytes.
pub(super) async fn artifact_meta(
    tx: &mut DbTransaction,
    digests: &[String],
) -> Result<Vec<ArtifactMetaRecord>, DeploymentError> {
    fetch_in_chunks(tx, digests, |chunk| {
        Query::select()
            .columns(artifact_meta_columns())
            .from(ArtifactObjects::Table)
            .and_where(Expr::col(ArtifactObjects::Digest).is_in(chunk.iter().cloned()))
            .to_owned()
    })
    .await
}

/// The rows of `digests`, without their bytes, read through the pool.
pub(super) async fn artifact_meta_from_pool(
    pool: &DbPool,
    digests: &[String],
) -> Result<Vec<ArtifactMetaRecord>, DeploymentError> {
    let mut out = Vec::new();
    for chunk in digests.chunks(IN_CHUNK) {
        let (sql, values) = build_sqlx(
            pool.backend(),
            Query::select()
                .columns(artifact_meta_columns())
                .from(ArtifactObjects::Table)
                .and_where(Expr::col(ArtifactObjects::Digest).is_in(chunk.iter().cloned())),
        );
        out.extend(
            pool.fetch_all_as::<ArtifactMetaRecord>(&sql, values)
                .await
                .map_err(DeploymentError::read)?,
        );
    }
    Ok(out)
}

/// Every artifact not yet deleted, without its bytes: what a collection
/// pass judges.
pub(super) async fn live_artifacts(
    tx: &mut DbTransaction,
) -> Result<Vec<ArtifactMetaRecord>, DeploymentError> {
    fetch_all(
        tx,
        Query::select()
            .columns(artifact_meta_columns())
            .from(ArtifactObjects::Table)
            .and_where(Expr::col(ArtifactObjects::State).ne(state_column(ArtifactState::Deleted)))
            .order_by(ArtifactObjects::Digest, Order::Asc),
    )
    .await
}

/// The rows of `digests`, with their bytes, inside a transaction.
pub(super) async fn artifacts(
    tx: &mut DbTransaction,
    digests: &[String],
) -> Result<Vec<ArtifactRecord>, DeploymentError> {
    fetch_in_chunks(tx, digests, select_artifacts_with_bytes).await
}

/// The rows of `digests`, with their bytes, read through the pool.
pub(super) async fn artifacts_from_pool(
    pool: &DbPool,
    digests: &[String],
) -> Result<Vec<ArtifactRecord>, DeploymentError> {
    let mut out = Vec::new();
    for chunk in digests.chunks(IN_CHUNK) {
        let (sql, values) = build_sqlx(pool.backend(), &mut select_artifacts_with_bytes(chunk));
        out.extend(
            pool.fetch_all_as::<ArtifactRecord>(&sql, values)
                .await
                .map_err(DeploymentError::read)?,
        );
    }
    Ok(out)
}

/// The digests written or revived within the last `grace_secs` seconds,
/// judged on the database's clock.
pub(super) async fn recent_artifacts(
    tx: &mut DbTransaction,
    grace_secs: u64,
) -> Result<std::collections::HashSet<String>, DeploymentError> {
    let secs = i64::try_from(grace_secs).unwrap_or(i64::MAX);
    let since = match tx.backend() {
        DbBackend::Sqlite => format!("datetime('now', '-{secs} seconds')"),
        DbBackend::Postgres => format!("LOCALTIMESTAMP - ({secs} * interval '1 second')"),
        DbBackend::Mysql => format!("CURRENT_TIMESTAMP - INTERVAL {secs} SECOND"),
    };
    let rows: Vec<(String,)> = fetch_all(
        tx,
        Query::select()
            .column(ArtifactObjects::Digest)
            .from(ArtifactObjects::Table)
            .and_where(Expr::col(ArtifactObjects::CreatedAt).gt(Expr::cust(since))),
    )
    .await?;
    Ok(rows.into_iter().map(|(d,)| d).collect())
}

/// Store `bytes` as the `ACTIVE` artifact `digest`. A row already active
/// with its bytes is left as it is (`false`); a row the collector drained or
/// deleted is revived with these bytes, its grace window restarted (`true`,
/// as for a new row).
pub(super) async fn put_artifact(
    tx: &mut DbTransaction,
    digest: &str,
    bytes: &[u8],
) -> Result<bool, DeploymentError> {
    let size = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
    let insert = Query::insert()
        .into_table(ArtifactObjects::Table)
        .columns([
            ArtifactObjects::Digest,
            ArtifactObjects::SizeBytes,
            ArtifactObjects::State,
            ArtifactObjects::PayloadBytes,
        ])
        .values_panic([
            digest.into(),
            size.into(),
            state_column(ArtifactState::Active).into(),
            bytes.to_vec().into(),
        ])
        .to_owned();
    if insert_if_absent(tx, insert, [ArtifactObjects::Digest]).await? > 0 {
        return Ok(true);
    }
    // `created_at` is assigned first: MySQL evaluates a SET list left to
    // right, so a later assignment would read the new `state`.
    let (sql, values) = build_sqlx(
        tx.backend(),
        Query::update()
            .table(ArtifactObjects::Table)
            .value(
                ArtifactObjects::CreatedAt,
                Expr::case(
                    Expr::col(ArtifactObjects::State).ne(state_column(ArtifactState::Active)),
                    Expr::current_timestamp(),
                )
                .finally(Expr::col(ArtifactObjects::CreatedAt)),
            )
            .value(ArtifactObjects::State, state_column(ArtifactState::Active))
            .value(ArtifactObjects::DrainEpoch, Option::<i64>::None)
            .value(ArtifactObjects::PayloadBytes, bytes.to_vec())
            .value(ArtifactObjects::SizeBytes, size)
            .and_where(Expr::col(ArtifactObjects::Digest).eq(digest))
            .and_where(
                Expr::col(ArtifactObjects::State)
                    .ne(state_column(ArtifactState::Active))
                    .or(Expr::col(ArtifactObjects::PayloadBytes).is_null()),
            ),
    );
    Ok(execute(tx, &sql, values).await? > 0)
}

/// One collection pass's moves: `draining` marked at `epoch`, `deleted`
/// dropped to tombstones, `revived` put back to `ACTIVE` with a fresh grace
/// window.
pub(super) async fn move_artifacts(
    tx: &mut DbTransaction,
    epoch: u64,
    draining: &[String],
    deleted: &[String],
    revived: &[String],
) -> Result<(), DeploymentError> {
    let backend = tx.backend();
    for chunk in draining.chunks(IN_CHUNK) {
        let (sql, values) = build_sqlx(
            backend,
            Query::update()
                .table(ArtifactObjects::Table)
                .value(
                    ArtifactObjects::State,
                    state_column(ArtifactState::Draining),
                )
                .value(ArtifactObjects::DrainEpoch, to_i64(epoch))
                .and_where(Expr::col(ArtifactObjects::Digest).is_in(chunk.iter().cloned())),
        );
        execute(tx, &sql, values).await?;
    }
    for chunk in deleted.chunks(IN_CHUNK) {
        let (sql, values) = build_sqlx(
            backend,
            Query::update()
                .table(ArtifactObjects::Table)
                .value(ArtifactObjects::State, state_column(ArtifactState::Deleted))
                .value(ArtifactObjects::PayloadBytes, Option::<Vec<u8>>::None)
                .and_where(Expr::col(ArtifactObjects::Digest).is_in(chunk.iter().cloned())),
        );
        execute(tx, &sql, values).await?;
    }
    for chunk in revived.chunks(IN_CHUNK) {
        let (sql, values) = build_sqlx(
            backend,
            Query::update()
                .table(ArtifactObjects::Table)
                .value(ArtifactObjects::CreatedAt, Expr::current_timestamp())
                .value(ArtifactObjects::State, state_column(ArtifactState::Active))
                .value(ArtifactObjects::DrainEpoch, Option::<i64>::None)
                .and_where(Expr::col(ArtifactObjects::Digest).is_in(chunk.iter().cloned())),
        );
        execute(tx, &sql, values).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_query::{MysqlQueryBuilder, PostgresQueryBuilder, SqliteQueryBuilder};

    /// The head lock renders on the backends that have one and nowhere else;
    /// the CAS keeps its `WHERE head_revision = …` guard everywhere.
    #[test]
    fn the_head_lock_and_cas_render_per_backend() {
        let mut stmt = select_head();
        stmt.lock(LockType::Update);
        assert!(stmt.to_string(PostgresQueryBuilder).ends_with("FOR UPDATE"));
        assert!(stmt.to_string(MysqlQueryBuilder).ends_with("FOR UPDATE"));
        assert!(!stmt.to_string(SqliteQueryBuilder).contains("FOR UPDATE"));

        let cas = Query::update()
            .table(DeploymentHead::Table)
            .value(DeploymentHead::HeadRevision, 2i64)
            .and_where(Expr::col(DeploymentHead::SingletonId).eq(HEAD_ROW))
            .and_where(Expr::col(DeploymentHead::HeadRevision).eq(1i64))
            .to_owned();
        let sql = cas.to_string(PostgresQueryBuilder);
        assert!(sql.contains(r#""head_revision" = 1"#), "{sql}");
    }

    /// A revival restarts the grace window only for a row that was not
    /// active, and MySQL reads that before `state` is reassigned.
    #[test]
    fn a_revival_assigns_created_at_before_state() {
        let stmt = Query::update()
            .table(ArtifactObjects::Table)
            .value(
                ArtifactObjects::CreatedAt,
                Expr::case(
                    Expr::col(ArtifactObjects::State).ne("active"),
                    Expr::current_timestamp(),
                )
                .finally(Expr::col(ArtifactObjects::CreatedAt)),
            )
            .value(ArtifactObjects::State, "active")
            .to_owned();
        let sql = stmt.to_string(MysqlQueryBuilder);
        let created = sql.find("`created_at` =").expect("created_at set");
        let state = sql.find("`state` = 'active'").expect("state set");
        assert!(created < state, "{sql}");
    }
}
