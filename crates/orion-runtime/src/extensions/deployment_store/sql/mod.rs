//! The deployment store's SQL: the rows as stored, and every statement over
//! them, in each backend's dialect through sea-query. Nothing here decides
//! anything; [`super::writes`] and [`super::catalog`] do.
//!
//! The only raw SQL is the database clock expressions (`Expr::cust`), which
//! differ per backend and carry no caller value.

mod artifacts;
mod reads;
#[cfg(test)]
mod tests;
mod writes;

pub(super) use artifacts::{
    artifact_meta, artifact_meta_from_pool, artifacts, artifacts_from_pool, live_artifacts,
    move_artifacts, put_artifact, recent_artifacts,
};
pub(super) use reads::{
    admissions, all_admissions, all_connections, all_packages, compacted_through, connection,
    connection_keys, connections_of, head, manifests, other_package_with_identity, package_digests,
    package_inventories, packages, read_head, receipt, revision, revision_flags,
    revision_from_pool, revisions_after,
};
pub(super) use writes::{
    compact, insert_connection, insert_package, insert_receipt, insert_revision, lock_head, revoke,
    upsert_admission, write_head,
};

use chrono::NaiveDateTime;
use sea_query::{InsertStatement, OnConflict, SelectStatement};

use super::{
    ArtifactState, ConnectionRevisionRow, DeploymentError, DeploymentReceipt,
    DeploymentRevisionRow, ProviderAdmissionRow, ProviderPackageRow,
};
use crate::extensions::{Digest, Revision};
use crate::storage::{DbBackend, DbTransaction, build_sqlx};

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

pub(super) fn state_column(state: ArtifactState) -> &'static str {
    match state {
        ArtifactState::Active => "active",
        ArtifactState::Draining => "draining",
        ArtifactState::Deleted => "deleted",
    }
}

/// A connection revision column read back. Revisions are written from a
/// [`Revision`], so a stored value below 1 is read as the first.
pub(super) fn stored_revision(v: i64) -> Revision {
    Revision::new(to_u64(v)).unwrap_or(Revision::FIRST)
}

pub(super) fn to_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

pub(super) fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

// ============================================================
// Execution
// ============================================================

pub(super) async fn fetch_all<T: crate::storage::DbRow>(
    tx: &mut DbTransaction,
    stmt: &mut SelectStatement,
) -> Result<Vec<T>, DeploymentError> {
    let (sql, values) = build_sqlx(tx.backend(), stmt);
    tx.fetch_all_as(&sql, values)
        .await
        .map_err(DeploymentError::read)
}

pub(super) async fn fetch_optional<T: crate::storage::DbRow>(
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
pub(super) async fn fetch_in_chunks<T, K>(
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

pub(super) async fn execute(
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
pub(super) async fn insert_if_absent<C>(
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
