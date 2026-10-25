//! SQLite, PostgreSQL, and MySQL persistence and startup hydration for [`ImmutableDeploymentStore`] (`V2-03.2`..`V2-03.4`, `ARCH-02`, `MOD-02`).

use serde_json::{Value, json};

use super::{
    ArchiveEntryInput, ArtifactObjectRow, ArtifactState, CandidatePlan, CanonicalManifest,
    ConnectionRevisionRow, DeploymentDirtySet, DeploymentError, DeploymentReceipt,
    DeploymentRevisionRow, DeploymentState, ImmutableDeploymentStore, ProviderAdmissionRow,
    ProviderPackageRow, Rollback,
};
use crate::extensions::AdmissionPolicy;
use crate::extensions::contract::ImplementationKind;
use crate::extensions::package_identity::sha256_digest;

type PersistedArtifactTuple = (String, i64, String, Option<i64>, Option<Vec<u8>>);
type PersistedRevisionTuple = (i64, Option<i64>, String, String, String, String, bool);
type PersistedOperationTuple = (
    String,
    String,
    String,
    i64,
    String,
    Option<String>,
    Option<i64>,
    Option<String>,
);

const SELECT_OPERATIONS: &str = "SELECT principal, idempotency_key, request_hash, committed_revision, \
     manifest_digest, operation_id, epoch, receiving_node_id FROM deployment_operations";

/// A stored row the store cannot read back. A read that skipped it would
/// serve a store missing what was committed.
fn corrupt(message: String) -> DeploymentError {
    DeploymentError::new("CORRUPT_PERSISTED_STATE", message)
}

/// A receipt as it was stored. A row written before the store recorded
/// `operation_id`, `epoch` and `receiving_node_id` gets the values it was
/// read back with until then: the revision for the epoch, and this node.
fn receipt_from_row(row: PersistedOperationTuple, legacy_node: &str) -> DeploymentReceipt {
    let (
        principal,
        idempotency_key,
        request_hash,
        committed_revision,
        manifest_digest,
        operation_id,
        epoch,
        receiving_node_id,
    ) = row;
    let rev = committed_revision.max(0) as u64;
    let epoch = epoch.map_or(rev, |e| e.max(0) as u64);
    DeploymentReceipt {
        operation_id: operation_id.unwrap_or_else(|| format!("op-{rev}-{epoch}")),
        principal,
        idempotency_key,
        request_hash,
        committed_revision: rev,
        manifest_digest,
        epoch,
        receiving_node_id: receiving_node_id.unwrap_or_else(|| legacy_node.to_string()),
    }
}
type PersistedPackageTuple = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
);

struct PersistenceBatch {
    write_head: bool,
    expected_head_revision: Option<u64>,
    head_revision: u64,
    epoch: u64,
    artifacts: Vec<ArtifactObjectRow>,
    packages: Vec<ProviderPackageRow>,
    admissions: Vec<ProviderAdmissionRow>,
    connections: Vec<ConnectionRevisionRow>,
    revisions: Vec<DeploymentRevisionRow>,
    receipts: Vec<(String, String, DeploymentReceipt)>,
    compacted_member_revisions: Vec<u64>,
}

impl PersistenceBatch {
    fn is_empty(&self) -> bool {
        !self.write_head
            && self.artifacts.is_empty()
            && self.packages.is_empty()
            && self.admissions.is_empty()
            && self.connections.is_empty()
            && self.revisions.is_empty()
            && self.receipts.is_empty()
            && self.compacted_member_revisions.is_empty()
    }
}

impl DeploymentState {
    fn extract_dirty_batch(&self) -> PersistenceBatch {
        PersistenceBatch {
            write_head: self.dirty.head,
            expected_head_revision: self.dirty.expected_head_revision,
            head_revision: self.head_revision,
            epoch: self.epoch,
            artifacts: self
                .dirty
                .artifacts
                .iter()
                .filter_map(|d| self.artifacts.get(d).cloned())
                .collect(),
            packages: self
                .dirty
                .packages
                .iter()
                .filter_map(|d| self.packages.get(d).cloned())
                .collect(),
            admissions: self
                .dirty
                .admissions
                .iter()
                .filter_map(|d| self.admissions.get(d).cloned())
                .collect(),
            connections: self
                .dirty
                .connections
                .iter()
                .filter_map(|k| self.connections.get(k).cloned())
                .collect(),
            revisions: self
                .dirty
                .revisions
                .iter()
                .filter_map(|r| self.revisions.get(r).cloned())
                .collect(),
            receipts: self
                .dirty
                .receipts
                .iter()
                .filter_map(|k| {
                    self.receipts
                        .get(k)
                        .map(|r| (k.0.clone(), k.1.clone(), r.clone()))
                })
                .collect(),
            compacted_member_revisions: self
                .dirty
                .compacted_member_revisions
                .iter()
                .copied()
                .collect(),
        }
    }
}

/// The first write of `deployment_head` failed. Two writers that both read
/// no head both insert one, and the loser's unique violation is the same lost
/// race the CAS `UPDATE` reports for every later write: a conflict on the
/// base revision, not a storage failure.
fn head_insert_error(e: sqlx::Error) -> DeploymentError {
    let lost_race = e
        .as_database_error()
        .is_some_and(|db| db.is_unique_violation());
    if lost_race {
        DeploymentError::new(
            "DEPLOYMENT_HEAD_CONFLICT",
            "concurrent modification of deployment_head detected",
        )
    } else {
        DeploymentError::new("DB_WRITE_ERROR", e.to_string())
    }
}

impl ImmutableDeploymentStore {
    fn extract_dirty_batch(&self) -> PersistenceBatch {
        let g = self.inner.lock().expect("store lock");
        g.extract_dirty_batch()
    }

    fn acknowledge_persisted_batch(&self, batch: &PersistenceBatch) {
        let mut g = self.inner.lock().expect("store lock");
        if batch.write_head && g.head_revision == batch.head_revision && g.epoch == batch.epoch {
            g.dirty.head = false;
            g.dirty.expected_head_revision = None;
        }
        for art in &batch.artifacts {
            if g.artifacts.get(&art.digest) == Some(art) {
                g.dirty.artifacts.remove(&art.digest);
            }
        }
        for pkg in &batch.packages {
            if g.packages.get(&pkg.package_digest) == Some(pkg) {
                g.dirty.packages.remove(&pkg.package_digest);
            }
        }
        for adm in &batch.admissions {
            if g.admissions.get(&adm.package_digest) == Some(adm) {
                g.dirty.admissions.remove(&adm.package_digest);
            }
        }
        for conn in &batch.connections {
            let key = (conn.connection_id.clone(), conn.revision);
            if g.connections.get(&key) == Some(conn) {
                g.dirty.connections.remove(&key);
            }
        }
        for rev in &batch.revisions {
            if g.revisions.get(&rev.revision) == Some(rev) {
                g.dirty.revisions.remove(&rev.revision);
            }
        }
        for (p, k, receipt) in &batch.receipts {
            let key = (p.clone(), k.clone());
            if g.receipts.get(&key) == Some(receipt) {
                g.dirty.receipts.remove(&key);
            }
        }
        for rev_id in &batch.compacted_member_revisions {
            g.dirty.compacted_member_revisions.remove(rev_id);
        }
    }

    fn publish_committed_batch(&self, staged: &DeploymentState, batch: &PersistenceBatch) {
        let mut g = self.inner.lock().expect("store lock");
        if batch.write_head && staged.head_revision >= g.head_revision {
            g.head_revision = staged.head_revision;
            g.epoch = staged.epoch;
            g.dirty.head = false;
            g.dirty.expected_head_revision = None;
        }
        for (k, v) in &staged.publisher_bindings {
            g.publisher_bindings.insert(k.clone(), v.clone());
        }
        for art in &batch.artifacts {
            g.artifacts.insert(art.digest.clone(), art.clone());
            g.dirty.artifacts.remove(&art.digest);
        }
        for pkg in &batch.packages {
            g.packages.insert(pkg.package_digest.clone(), pkg.clone());
            g.dirty.packages.remove(&pkg.package_digest);
        }
        for adm in &batch.admissions {
            g.admissions.insert(adm.package_digest.clone(), adm.clone());
            g.dirty.admissions.remove(&adm.package_digest);
        }
        for conn in &batch.connections {
            let key = (conn.connection_id.clone(), conn.revision);
            g.connections.insert(key.clone(), conn.clone());
            g.dirty.connections.remove(&key);
        }
        for rev in &batch.revisions {
            g.revisions.insert(rev.revision, rev.clone());
            g.dirty.revisions.remove(&rev.revision);
        }
        for (p, k, receipt) in &batch.receipts {
            let key = (p.clone(), k.clone());
            g.receipts.insert(key.clone(), receipt.clone());
            g.dirty.receipts.remove(&key);
        }
        for rev_id in &batch.compacted_member_revisions {
            g.dirty.compacted_member_revisions.remove(rev_id);
        }
        // Also merge any newly hydrated DB state from `staged` (such as receipts, revisions,
        // admissions, packages, connections from peer replicas) without clobbering local entries.
        if staged.head_revision > g.head_revision {
            g.head_revision = staged.head_revision;
            g.epoch = staged.epoch;
        }
        for (rev_id, rev_row) in &staged.revisions {
            g.revisions
                .entry(*rev_id)
                .or_insert_with(|| rev_row.clone());
        }
        for (op_key, receipt) in &staged.receipts {
            g.receipts
                .entry(op_key.clone())
                .or_insert_with(|| receipt.clone());
        }
        for (pkg_digest, adm) in &staged.admissions {
            g.admissions.insert(pkg_digest.clone(), adm.clone());
        }
        for (pkg_digest, pkg) in &staged.packages {
            g.packages
                .entry(pkg_digest.clone())
                .or_insert_with(|| pkg.clone());
        }
        for (conn_key, conn) in &staged.connections {
            g.connections
                .entry(conn_key.clone())
                .or_insert_with(|| conn.clone());
        }
        for (art_digest, art) in &staged.artifacts {
            g.artifacts
                .entry(art_digest.clone())
                .or_insert_with(|| art.clone());
        }
    }

    fn merge_catalog_rows(
        staged: &mut DeploymentState,
        art_rows: Vec<PersistedArtifactTuple>,
        pkg_rows: Vec<PersistedPackageTuple>,
        conn_rows: Vec<(String, i64, String, String, Option<String>)>,
    ) -> Result<(), DeploymentError> {
        for (digest, size_bytes, state_str, drain_epoch, payload_bytes) in art_rows {
            let state = match state_str.as_str() {
                "active" | "ACTIVE" => ArtifactState::Active,
                "draining" | "DRAINING" => ArtifactState::Draining,
                "deleted" | "DELETED" => ArtifactState::Deleted,
                other => {
                    return Err(corrupt(format!(
                        "unknown artifact state '{other}' for '{digest}'"
                    )));
                }
            };
            staged
                .artifacts
                .entry(digest.clone())
                .or_insert_with(|| ArtifactObjectRow {
                    digest,
                    size_bytes: size_bytes.max(0) as usize,
                    state,
                    drain_epoch: drain_epoch.map(|e| e.max(0) as u64),
                    bytes: payload_bytes,
                });
        }
        for (
            digest,
            publisher,
            provider_id,
            version,
            impl_str,
            tier,
            manifest_str,
            inventory_str,
        ) in pkg_rows
        {
            let implementation = ImplementationKind::parse(&impl_str).ok_or_else(|| {
                corrupt(format!(
                    "unknown package implementation '{impl_str}' for '{digest}'"
                ))
            })?;
            let manifest_json: Value = serde_json::from_str(&manifest_str).map_err(|e| {
                corrupt(format!("corrupt manifest_json for package '{digest}': {e}"))
            })?;
            let inventory_json: Value = serde_json::from_str(&inventory_str).map_err(|e| {
                corrupt(format!(
                    "corrupt inventory_json for package '{digest}': {e}"
                ))
            })?;
            staged
                .artifacts
                .entry(digest.clone())
                .or_insert_with(|| ArtifactObjectRow {
                    digest: digest.clone(),
                    size_bytes: inventory_str.len(),
                    state: ArtifactState::Active,
                    drain_epoch: None,
                    bytes: None,
                });
            staged
                .publisher_bindings
                .entry((publisher.clone(), provider_id.clone(), version.clone()))
                .or_insert_with(|| digest.clone());
            staged
                .packages
                .entry(digest.clone())
                .or_insert_with(|| ProviderPackageRow {
                    package_digest: digest,
                    publisher: publisher.clone(),
                    provider_id,
                    version,
                    implementation,
                    manifest_json,
                    distribution_json: json!({
                        "publisher": publisher,
                        "tier": tier,
                    }),
                    inventory_json,
                });
        }
        for (connection_id, rev, provider_digest, cfg_str, secret_locator) in conn_rows {
            let revision = rev.max(1) as u64;
            let public_config: Value = serde_json::from_str(&cfg_str).map_err(|e| {
                corrupt(format!(
                    "corrupt public_config_json for connection '{connection_id}' revision {revision}: {e}"
                ))
            })?;
            let secret_locators = super::locators_from_column(secret_locator).map_err(|e| {
                corrupt(format!(
                    "connection '{connection_id}' revision {revision}: {e}"
                ))
            })?;
            let cfg_bytes = super::connection_config_bytes(&public_config, &secret_locators);
            let configuration_digest = sha256_digest(&cfg_bytes);
            staged
                .artifacts
                .entry(configuration_digest.clone())
                .or_insert_with(|| ArtifactObjectRow {
                    digest: configuration_digest.clone(),
                    size_bytes: cfg_bytes.len(),
                    state: ArtifactState::Active,
                    drain_epoch: None,
                    bytes: Some(cfg_bytes),
                });
            staged
                .connections
                .entry((connection_id.clone(), revision))
                .or_insert_with(|| ConnectionRevisionRow {
                    connection_id,
                    revision,
                    provider_digest,
                    public_config,
                    secret_locators,
                    configuration_digest,
                });
        }
        Ok(())
    }

    async fn sync_staged_from_postgres_tx(
        staged: &mut DeploymentState,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), DeploymentError> {
        let head_row: Option<(i64, i64)> = sqlx::query_as(
            "SELECT head_revision, epoch FROM deployment_head WHERE singleton_id = 1 FOR UPDATE",
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
        if let Some((h, ep)) = head_row {
            let db_head = h.max(0) as u64;
            let db_epoch = ep.max(0) as u64;
            if db_head > 0 || staged.head_revision == 0 {
                staged.head_revision = db_head;
                staged.epoch = db_epoch;
            }
        }
        let adm_rows: Vec<(String, String, String, bool)> = sqlx::query_as(
            "SELECT package_digest, policy_id, admission_fingerprint, revoked FROM provider_admissions",
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
        for (package_digest, policy_id, admission_fingerprint, revoked) in adm_rows {
            staged.admissions.insert(
                package_digest.clone(),
                ProviderAdmissionRow {
                    package_digest,
                    policy_id,
                    admission_fingerprint,
                    revoked,
                },
            );
        }
        let op_rows: Vec<PersistedOperationTuple> = sqlx::query_as(SELECT_OPERATIONS)
            .fetch_all(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
        for row in op_rows {
            let receipt = receipt_from_row(row, &staged.node_id);
            staged.receipts.insert(
                (receipt.principal.clone(), receipt.idempotency_key.clone()),
                receipt,
            );
        }
        let rev_rows: Vec<PersistedRevisionTuple> = sqlx::query_as(
            "SELECT revision, parent_revision, manifest_digest, manifest_json, principal, reason, members_compacted FROM deployment_revisions",
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
        for (
            rev,
            parent_rev,
            manifest_digest,
            manifest_str,
            principal,
            reason,
            members_compacted,
        ) in rev_rows
        {
            let rev_u64 = rev.max(0) as u64;
            if let std::collections::btree_map::Entry::Vacant(e) = staged.revisions.entry(rev_u64) {
                let manifest =
                    serde_json::from_str::<CanonicalManifest>(&manifest_str).map_err(|err| {
                        corrupt(format!("corrupt manifest for revision {rev_u64}: {err}"))
                    })?;
                let (manifest_bytes, _) = manifest.canonicalize_and_verify().map_err(|err| {
                    corrupt(format!(
                        "manifest for revision {rev_u64} does not verify: {err}"
                    ))
                })?;
                e.insert(DeploymentRevisionRow {
                    revision: rev_u64,
                    parent_revision: parent_rev.map(|r| r.max(0) as u64),
                    manifest_digest,
                    manifest_bytes,
                    manifest,
                    reason,
                    created_by: principal,
                    members_compacted,
                });
            }
        }
        let art_rows: Vec<PersistedArtifactTuple> = sqlx::query_as(
            "SELECT digest, size_bytes, state, drain_epoch, payload_bytes FROM artifact_objects",
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
        let pkg_rows: Vec<PersistedPackageTuple> = sqlx::query_as(
            "SELECT digest, publisher, provider_id, version, implementation, tier, manifest_json, inventory_json FROM provider_packages",
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
        let conn_rows: Vec<(String, i64, String, String, Option<String>)> = sqlx::query_as(
            "SELECT connection_id, revision, provider_digest, public_config_json, secret_locator FROM connection_revisions",
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
        Self::merge_catalog_rows(staged, art_rows, pkg_rows, conn_rows)?;
        Ok(())
    }

    async fn sync_staged_from_mysql_tx(
        staged: &mut DeploymentState,
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    ) -> Result<(), DeploymentError> {
        let head_row: Option<(i64, i64)> = sqlx::query_as(
            "SELECT head_revision, epoch FROM deployment_head WHERE singleton_id = 1 FOR UPDATE",
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
        if let Some((h, ep)) = head_row {
            let db_head = h.max(0) as u64;
            let db_epoch = ep.max(0) as u64;
            if db_head > 0 || staged.head_revision == 0 {
                staged.head_revision = db_head;
                staged.epoch = db_epoch;
            }
        }
        let adm_rows: Vec<(String, String, String, bool)> = sqlx::query_as(
            "SELECT package_digest, policy_id, admission_fingerprint, revoked FROM provider_admissions",
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
        for (package_digest, policy_id, admission_fingerprint, revoked) in adm_rows {
            staged.admissions.insert(
                package_digest.clone(),
                ProviderAdmissionRow {
                    package_digest,
                    policy_id,
                    admission_fingerprint,
                    revoked,
                },
            );
        }
        let op_rows: Vec<PersistedOperationTuple> = sqlx::query_as(SELECT_OPERATIONS)
            .fetch_all(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
        for row in op_rows {
            let receipt = receipt_from_row(row, &staged.node_id);
            staged.receipts.insert(
                (receipt.principal.clone(), receipt.idempotency_key.clone()),
                receipt,
            );
        }
        let rev_rows: Vec<PersistedRevisionTuple> = sqlx::query_as(
            "SELECT revision, parent_revision, manifest_digest, manifest_json, principal, reason, members_compacted FROM deployment_revisions",
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
        for (
            rev,
            parent_rev,
            manifest_digest,
            manifest_str,
            principal,
            reason,
            members_compacted,
        ) in rev_rows
        {
            let rev_u64 = rev.max(0) as u64;
            if let std::collections::btree_map::Entry::Vacant(e) = staged.revisions.entry(rev_u64) {
                let manifest =
                    serde_json::from_str::<CanonicalManifest>(&manifest_str).map_err(|err| {
                        corrupt(format!("corrupt manifest for revision {rev_u64}: {err}"))
                    })?;
                let (manifest_bytes, _) = manifest.canonicalize_and_verify().map_err(|err| {
                    corrupt(format!(
                        "manifest for revision {rev_u64} does not verify: {err}"
                    ))
                })?;
                e.insert(DeploymentRevisionRow {
                    revision: rev_u64,
                    parent_revision: parent_rev.map(|r| r.max(0) as u64),
                    manifest_digest,
                    manifest_bytes,
                    manifest,
                    reason,
                    created_by: principal,
                    members_compacted,
                });
            }
        }
        let art_rows: Vec<PersistedArtifactTuple> = sqlx::query_as(
            "SELECT digest, size_bytes, state, drain_epoch, payload_bytes FROM artifact_objects",
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
        let pkg_rows: Vec<PersistedPackageTuple> = sqlx::query_as(
            "SELECT digest, publisher, provider_id, version, implementation, tier, manifest_json, inventory_json FROM provider_packages",
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
        let conn_rows: Vec<(String, i64, String, String, Option<String>)> = sqlx::query_as(
            "SELECT connection_id, revision, provider_digest, public_config_json, secret_locator FROM connection_revisions",
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
        Self::merge_catalog_rows(staged, art_rows, pkg_rows, conn_rows)?;
        Ok(())
    }

    async fn sync_staged_from_sqlite_tx(
        staged: &mut DeploymentState,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    ) -> Result<(), DeploymentError> {
        let head_row: Option<(i64, i64)> = sqlx::query_as(
            "SELECT head_revision, epoch FROM deployment_head WHERE singleton_id = 1",
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
        if let Some((h, ep)) = head_row {
            let db_head = h.max(0) as u64;
            let db_epoch = ep.max(0) as u64;
            if db_head > 0 || staged.head_revision == 0 {
                staged.head_revision = db_head;
                staged.epoch = db_epoch;
            }
        }
        let adm_rows: Vec<(String, String, String, bool)> = sqlx::query_as(
            "SELECT package_digest, policy_id, admission_fingerprint, revoked FROM provider_admissions",
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
        for (package_digest, policy_id, admission_fingerprint, revoked) in adm_rows {
            staged.admissions.insert(
                package_digest.clone(),
                ProviderAdmissionRow {
                    package_digest,
                    policy_id,
                    admission_fingerprint,
                    revoked,
                },
            );
        }
        let op_rows: Vec<PersistedOperationTuple> = sqlx::query_as(SELECT_OPERATIONS)
            .fetch_all(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
        for row in op_rows {
            let receipt = receipt_from_row(row, &staged.node_id);
            staged.receipts.insert(
                (receipt.principal.clone(), receipt.idempotency_key.clone()),
                receipt,
            );
        }
        let rev_rows: Vec<PersistedRevisionTuple> = sqlx::query_as(
            "SELECT revision, parent_revision, manifest_digest, manifest_json, principal, reason, members_compacted FROM deployment_revisions",
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
        for (
            rev,
            parent_rev,
            manifest_digest,
            manifest_str,
            principal,
            reason,
            members_compacted,
        ) in rev_rows
        {
            let rev_u64 = rev.max(0) as u64;
            if let std::collections::btree_map::Entry::Vacant(e) = staged.revisions.entry(rev_u64) {
                let manifest =
                    serde_json::from_str::<CanonicalManifest>(&manifest_str).map_err(|err| {
                        corrupt(format!("corrupt manifest for revision {rev_u64}: {err}"))
                    })?;
                let (manifest_bytes, _) = manifest.canonicalize_and_verify().map_err(|err| {
                    corrupt(format!(
                        "manifest for revision {rev_u64} does not verify: {err}"
                    ))
                })?;
                e.insert(DeploymentRevisionRow {
                    revision: rev_u64,
                    parent_revision: parent_rev.map(|r| r.max(0) as u64),
                    manifest_digest,
                    manifest_bytes,
                    manifest,
                    reason,
                    created_by: principal,
                    members_compacted,
                });
            }
        }
        let art_rows: Vec<PersistedArtifactTuple> = sqlx::query_as(
            "SELECT digest, size_bytes, state, drain_epoch, payload_bytes FROM artifact_objects",
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
        let pkg_rows: Vec<PersistedPackageTuple> = sqlx::query_as(
            "SELECT digest, publisher, provider_id, version, implementation, tier, manifest_json, inventory_json FROM provider_packages",
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
        let conn_rows: Vec<(String, i64, String, String, Option<String>)> = sqlx::query_as(
            "SELECT connection_id, revision, provider_digest, public_config_json, secret_locator FROM connection_revisions",
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
        Self::merge_catalog_rows(staged, art_rows, pkg_rows, conn_rows)?;
        Ok(())
    }

    async fn write_batch_to_postgres_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        batch: &PersistenceBatch,
        in_memory_admissions: Option<&std::collections::BTreeMap<String, ProviderAdmissionRow>>,
    ) -> Result<(), DeploymentError> {
        for art in &batch.artifacts {
            let state_str = match art.state {
                ArtifactState::Active => "active",
                ArtifactState::Draining => "draining",
                ArtifactState::Deleted => "deleted",
            };
            sqlx::query(
                "INSERT INTO artifact_objects (digest, size_bytes, state, drain_epoch, payload_bytes) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT(digest) DO UPDATE SET state = EXCLUDED.state, drain_epoch = EXCLUDED.drain_epoch, payload_bytes = EXCLUDED.payload_bytes",
            )
            .bind(&art.digest)
            .bind(art.size_bytes as i64)
            .bind(state_str)
            .bind(art.drain_epoch.map(|e| e as i64))
            .bind(art.bytes.as_deref())
            .execute(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for pkg in &batch.packages {
            let tier = pkg
                .distribution_json
                .get("tier")
                .and_then(Value::as_str)
                .unwrap_or("community");
            let manifest_str = serde_json::to_string(&pkg.manifest_json)
                .map_err(|e| DeploymentError::new("SERIALIZE_ERROR", e.to_string()))?;
            let inventory_str = serde_json::to_string(&pkg.inventory_json)
                .map_err(|e| DeploymentError::new("SERIALIZE_ERROR", e.to_string()))?;
            sqlx::query(
                "INSERT INTO provider_packages (digest, publisher, provider_id, version, implementation, tier, manifest_json, inventory_json) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT(digest) DO NOTHING",
            )
            .bind(&pkg.package_digest)
            .bind(&pkg.publisher)
            .bind(&pkg.provider_id)
            .bind(&pkg.version)
            .bind(pkg.implementation.as_str())
            .bind(tier)
            .bind(manifest_str)
            .bind(inventory_str)
            .execute(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for adm in &batch.admissions {
            sqlx::query(
                "INSERT INTO provider_admissions (package_digest, policy_id, admission_fingerprint, revoked) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT(package_digest) DO UPDATE SET revoked = EXCLUDED.revoked, admission_fingerprint = EXCLUDED.admission_fingerprint",
            )
            .bind(&adm.package_digest)
            .bind(&adm.policy_id)
            .bind(&adm.admission_fingerprint)
            .bind(adm.revoked)
            .execute(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for conn in &batch.connections {
            let cfg_str = serde_json::to_string(&conn.public_config)
                .map_err(|e| DeploymentError::new("SERIALIZE_ERROR", e.to_string()))?;
            sqlx::query(
                "INSERT INTO connection_revisions (connection_id, revision, provider_digest, public_config_json, secret_locator) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT(connection_id, revision) DO NOTHING",
            )
            .bind(&conn.connection_id)
            .bind(conn.revision as i64)
            .bind(&conn.provider_digest)
            .bind(cfg_str)
            .bind(super::locators_column(&conn.secret_locators))
            .execute(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for (principal, idem_key, receipt) in &batch.receipts {
            let existing: Option<(String, i64, String)> = sqlx::query_as(
                "SELECT request_hash, committed_revision, manifest_digest FROM deployment_operations WHERE principal = $1 AND idempotency_key = $2",
            )
            .bind(principal)
            .bind(idem_key)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
            if let Some((db_hash, db_rev, db_digest)) = existing {
                if db_hash != receipt.request_hash {
                    return Err(DeploymentError::new(
                        "IDEMPOTENCY_KEY_CONFLICT",
                        "idempotency_key already used by principal with different request parameters",
                    ));
                }
                if db_rev != receipt.committed_revision as i64
                    || db_digest != receipt.manifest_digest
                {
                    return Err(DeploymentError::new(
                        "DEPLOYMENT_HEAD_CONFLICT",
                        format!("idempotency receipt conflicts with committed revision {db_rev}"),
                    ));
                }
            } else {
                sqlx::query(
                    "INSERT INTO deployment_operations (principal, idempotency_key, request_hash, committed_revision, manifest_digest, \
                     operation_id, epoch, receiving_node_id) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                )
                .bind(principal)
                .bind(idem_key)
                .bind(&receipt.request_hash)
                .bind(receipt.committed_revision as i64)
                .bind(&receipt.manifest_digest)
                .bind(&receipt.operation_id)
                .bind(receipt.epoch as i64)
                .bind(&receipt.receiving_node_id)
                .execute(&mut **tx)
                .await
                .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
            }
        }

        for rev in &batch.revisions {
            for p in &rev.manifest.providers {
                let db_adm: Option<(bool,)> = sqlx::query_as(
                    "SELECT revoked FROM provider_admissions WHERE package_digest = $1",
                )
                .bind(&p.digest)
                .fetch_optional(&mut **tx)
                .await
                .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
                match db_adm {
                    Some((true,)) => {
                        return Err(DeploymentError::new(
                            "ADMISSION_INVALIDATED",
                            format!("admission for '{}' is revoked in database", p.digest),
                        ));
                    }
                    Some((false,)) => {}
                    None => {
                        if let Some(adm_map) = in_memory_admissions
                            && let Some(adm) = adm_map.get(&p.digest)
                            && !adm.revoked
                        {
                            sqlx::query(
                                "INSERT INTO provider_admissions (package_digest, policy_id, admission_fingerprint, revoked) \
                                 VALUES ($1, $2, $3, $4)",
                            )
                            .bind(&adm.package_digest)
                            .bind(&adm.policy_id)
                            .bind(&adm.admission_fingerprint)
                            .bind(adm.revoked)
                            .execute(&mut **tx)
                            .await
                            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
                        } else {
                            return Err(DeploymentError::new("PROVIDER_NOT_ADMITTED", &p.digest));
                        }
                    }
                }
            }
        }

        if batch.write_head {
            let head_row: Option<(i64, i64)> = sqlx::query_as(
                "SELECT head_revision, epoch FROM deployment_head WHERE singleton_id = 1 FOR UPDATE",
            )
            .fetch_optional(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
            let db_head = head_row.map(|(h, _)| h.max(0) as u64).unwrap_or(0);
            if let Some(expected_base) = batch.expected_head_revision
                && db_head != expected_base
            {
                return Err(DeploymentError::new(
                    "DEPLOYMENT_HEAD_CONFLICT",
                    format!(
                        "expected base_revision {expected_base}, but current database head_revision is {db_head}"
                    ),
                ));
            }
            if head_row.is_none() {
                sqlx::query(
                    "INSERT INTO deployment_head (singleton_id, head_revision, epoch) VALUES (1, $1, $2)",
                )
                .bind(batch.head_revision as i64)
                .bind(batch.epoch as i64)
                .execute(&mut **tx)
                .await
                .map_err(head_insert_error)?;
            } else {
                let res = sqlx::query(
                    "UPDATE deployment_head SET head_revision = $1, epoch = $2 WHERE singleton_id = 1 AND head_revision = $3",
                )
                .bind(batch.head_revision as i64)
                .bind(batch.epoch as i64)
                .bind(db_head as i64)
                .execute(&mut **tx)
                .await
                .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
                if res.rows_affected() != 1 {
                    return Err(DeploymentError::new(
                        "DEPLOYMENT_HEAD_CONFLICT",
                        "concurrent modification of deployment_head detected",
                    ));
                }
            }
        }

        for rev in &batch.revisions {
            let existing_rev: Option<(String,)> = sqlx::query_as(
                "SELECT manifest_digest FROM deployment_revisions WHERE revision = $1",
            )
            .bind(rev.revision as i64)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;

            if let Some((existing_digest,)) = existing_rev {
                if existing_digest != rev.manifest_digest {
                    return Err(DeploymentError::new(
                        "DEPLOYMENT_HEAD_CONFLICT",
                        format!(
                            "conflicting deployment revision {} already exists in database",
                            rev.revision
                        ),
                    ));
                }
                sqlx::query(
                    "UPDATE deployment_revisions SET members_compacted = $1 WHERE revision = $2 AND manifest_digest = $3",
                )
                .bind(rev.members_compacted)
                .bind(rev.revision as i64)
                .bind(&rev.manifest_digest)
                .execute(&mut **tx)
                .await
                .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
            } else {
                let manifest_str = serde_json::to_string(&rev.manifest)
                    .map_err(|e| DeploymentError::new("SERIALIZE_ERROR", e.to_string()))?;
                sqlx::query(
                    "INSERT INTO deployment_revisions (revision, parent_revision, manifest_digest, manifest_json, principal, reason, members_compacted) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                )
                .bind(rev.revision as i64)
                .bind(rev.parent_revision.map(|r| r as i64))
                .bind(&rev.manifest_digest)
                .bind(manifest_str)
                .bind(&rev.created_by)
                .bind(&rev.reason)
                .bind(rev.members_compacted)
                .execute(&mut **tx)
                .await
                .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
            }

            if !rev.members_compacted {
                for m in &rev.manifest.members {
                    sqlx::query(
                        "INSERT INTO deployment_members (deployment_revision, entity_kind, entity_id, entity_revision, owning_package, definition_digest) \
                         VALUES ($1, $2, $3, $4, $5, $6) \
                         ON CONFLICT(deployment_revision, entity_kind, entity_id, entity_revision) DO NOTHING",
                    )
                    .bind(rev.revision as i64)
                    .bind(m.entity.kind.as_str())
                    .bind(&m.entity.id)
                    .bind(m.entity.revision as i64)
                    .bind(m.owning_package.as_deref().unwrap_or(""))
                    .bind(&m.definition_digest)
                    .execute(&mut **tx)
                    .await
                    .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
                }
            }
        }

        for rev_id in &batch.compacted_member_revisions {
            sqlx::query("DELETE FROM deployment_members WHERE deployment_revision = $1")
                .bind(*rev_id as i64)
                .execute(&mut **tx)
                .await
                .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        Ok(())
    }

    async fn write_batch_to_mysql_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        batch: &PersistenceBatch,
        in_memory_admissions: Option<&std::collections::BTreeMap<String, ProviderAdmissionRow>>,
    ) -> Result<(), DeploymentError> {
        for art in &batch.artifacts {
            let state_str = match art.state {
                ArtifactState::Active => "active",
                ArtifactState::Draining => "draining",
                ArtifactState::Deleted => "deleted",
            };
            sqlx::query(
                "INSERT INTO artifact_objects (digest, size_bytes, state, drain_epoch, payload_bytes) \
                 VALUES (?, ?, ?, ?, ?) \
                 ON DUPLICATE KEY UPDATE state = VALUES(state), drain_epoch = VALUES(drain_epoch), payload_bytes = VALUES(payload_bytes)",
            )
            .bind(&art.digest)
            .bind(art.size_bytes as i64)
            .bind(state_str)
            .bind(art.drain_epoch.map(|e| e as i64))
            .bind(art.bytes.as_deref())
            .execute(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for pkg in &batch.packages {
            let tier = pkg
                .distribution_json
                .get("tier")
                .and_then(Value::as_str)
                .unwrap_or("community");
            let manifest_str = serde_json::to_string(&pkg.manifest_json)
                .map_err(|e| DeploymentError::new("SERIALIZE_ERROR", e.to_string()))?;
            let inventory_str = serde_json::to_string(&pkg.inventory_json)
                .map_err(|e| DeploymentError::new("SERIALIZE_ERROR", e.to_string()))?;
            sqlx::query(
                "INSERT IGNORE INTO provider_packages (digest, publisher, provider_id, version, implementation, tier, manifest_json, inventory_json) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&pkg.package_digest)
            .bind(&pkg.publisher)
            .bind(&pkg.provider_id)
            .bind(&pkg.version)
            .bind(pkg.implementation.as_str())
            .bind(tier)
            .bind(manifest_str)
            .bind(inventory_str)
            .execute(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for adm in &batch.admissions {
            sqlx::query(
                "INSERT INTO provider_admissions (package_digest, policy_id, admission_fingerprint, revoked) \
                 VALUES (?, ?, ?, ?) \
                 ON DUPLICATE KEY UPDATE revoked = VALUES(revoked), admission_fingerprint = VALUES(admission_fingerprint)",
            )
            .bind(&adm.package_digest)
            .bind(&adm.policy_id)
            .bind(&adm.admission_fingerprint)
            .bind(adm.revoked)
            .execute(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for conn in &batch.connections {
            let cfg_str = serde_json::to_string(&conn.public_config)
                .map_err(|e| DeploymentError::new("SERIALIZE_ERROR", e.to_string()))?;
            sqlx::query(
                "INSERT IGNORE INTO connection_revisions (connection_id, revision, provider_digest, public_config_json, secret_locator) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(&conn.connection_id)
            .bind(conn.revision as i64)
            .bind(&conn.provider_digest)
            .bind(cfg_str)
            .bind(super::locators_column(&conn.secret_locators))
            .execute(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for (principal, idem_key, receipt) in &batch.receipts {
            let existing: Option<(String, i64, String)> = sqlx::query_as(
                "SELECT request_hash, committed_revision, manifest_digest FROM deployment_operations WHERE principal = ? AND idempotency_key = ?",
            )
            .bind(principal)
            .bind(idem_key)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
            if let Some((db_hash, db_rev, db_digest)) = existing {
                if db_hash != receipt.request_hash {
                    return Err(DeploymentError::new(
                        "IDEMPOTENCY_KEY_CONFLICT",
                        "idempotency_key already used by principal with different request parameters",
                    ));
                }
                if db_rev != receipt.committed_revision as i64
                    || db_digest != receipt.manifest_digest
                {
                    return Err(DeploymentError::new(
                        "DEPLOYMENT_HEAD_CONFLICT",
                        format!("idempotency receipt conflicts with committed revision {db_rev}"),
                    ));
                }
            } else {
                sqlx::query(
                    "INSERT INTO deployment_operations (principal, idempotency_key, request_hash, committed_revision, manifest_digest, \
                     operation_id, epoch, receiving_node_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(principal)
                .bind(idem_key)
                .bind(&receipt.request_hash)
                .bind(receipt.committed_revision as i64)
                .bind(&receipt.manifest_digest)
                .bind(&receipt.operation_id)
                .bind(receipt.epoch as i64)
                .bind(&receipt.receiving_node_id)
                .execute(&mut **tx)
                .await
                .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
            }
        }

        for rev in &batch.revisions {
            for p in &rev.manifest.providers {
                let db_adm: Option<(bool,)> = sqlx::query_as(
                    "SELECT revoked FROM provider_admissions WHERE package_digest = ?",
                )
                .bind(&p.digest)
                .fetch_optional(&mut **tx)
                .await
                .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
                match db_adm {
                    Some((true,)) => {
                        return Err(DeploymentError::new(
                            "ADMISSION_INVALIDATED",
                            format!("admission for '{}' is revoked in database", p.digest),
                        ));
                    }
                    Some((false,)) => {}
                    None => {
                        if let Some(adm_map) = in_memory_admissions
                            && let Some(adm) = adm_map.get(&p.digest)
                            && !adm.revoked
                        {
                            sqlx::query(
                                "INSERT INTO provider_admissions (package_digest, policy_id, admission_fingerprint, revoked) \
                                 VALUES (?, ?, ?, ?)",
                            )
                            .bind(&adm.package_digest)
                            .bind(&adm.policy_id)
                            .bind(&adm.admission_fingerprint)
                            .bind(adm.revoked)
                            .execute(&mut **tx)
                            .await
                            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
                        } else {
                            return Err(DeploymentError::new("PROVIDER_NOT_ADMITTED", &p.digest));
                        }
                    }
                }
            }
        }

        if batch.write_head {
            let head_row: Option<(i64, i64)> = sqlx::query_as(
                "SELECT head_revision, epoch FROM deployment_head WHERE singleton_id = 1 FOR UPDATE",
            )
            .fetch_optional(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
            let db_head = head_row.map(|(h, _)| h.max(0) as u64).unwrap_or(0);
            if let Some(expected_base) = batch.expected_head_revision
                && db_head != expected_base
            {
                return Err(DeploymentError::new(
                    "DEPLOYMENT_HEAD_CONFLICT",
                    format!(
                        "expected base_revision {expected_base}, but current database head_revision is {db_head}"
                    ),
                ));
            }
            if head_row.is_none() {
                sqlx::query(
                    "INSERT INTO deployment_head (singleton_id, head_revision, epoch) VALUES (1, ?, ?)",
                )
                .bind(batch.head_revision as i64)
                .bind(batch.epoch as i64)
                .execute(&mut **tx)
                .await
                .map_err(head_insert_error)?;
            } else {
                let res = sqlx::query(
                    "UPDATE deployment_head SET head_revision = ?, epoch = ? WHERE singleton_id = 1 AND head_revision = ?",
                )
                .bind(batch.head_revision as i64)
                .bind(batch.epoch as i64)
                .bind(db_head as i64)
                .execute(&mut **tx)
                .await
                .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
                if res.rows_affected() != 1 {
                    return Err(DeploymentError::new(
                        "DEPLOYMENT_HEAD_CONFLICT",
                        "concurrent modification of deployment_head detected",
                    ));
                }
            }
        }

        for rev in &batch.revisions {
            let existing_rev: Option<(String,)> = sqlx::query_as(
                "SELECT manifest_digest FROM deployment_revisions WHERE revision = ?",
            )
            .bind(rev.revision as i64)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;

            if let Some((existing_digest,)) = existing_rev {
                if existing_digest != rev.manifest_digest {
                    return Err(DeploymentError::new(
                        "DEPLOYMENT_HEAD_CONFLICT",
                        format!(
                            "conflicting deployment revision {} already exists in database",
                            rev.revision
                        ),
                    ));
                }
                sqlx::query(
                    "UPDATE deployment_revisions SET members_compacted = ? WHERE revision = ? AND manifest_digest = ?",
                )
                .bind(rev.members_compacted)
                .bind(rev.revision as i64)
                .bind(&rev.manifest_digest)
                .execute(&mut **tx)
                .await
                .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
            } else {
                let manifest_str = serde_json::to_string(&rev.manifest)
                    .map_err(|e| DeploymentError::new("SERIALIZE_ERROR", e.to_string()))?;
                sqlx::query(
                    "INSERT INTO deployment_revisions (revision, parent_revision, manifest_digest, manifest_json, principal, reason, members_compacted) \
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(rev.revision as i64)
                .bind(rev.parent_revision.map(|r| r as i64))
                .bind(&rev.manifest_digest)
                .bind(manifest_str)
                .bind(&rev.created_by)
                .bind(&rev.reason)
                .bind(rev.members_compacted)
                .execute(&mut **tx)
                .await
                .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
            }

            if !rev.members_compacted {
                for m in &rev.manifest.members {
                    sqlx::query(
                        "INSERT IGNORE INTO deployment_members (deployment_revision, entity_kind, entity_id, entity_revision, owning_package, definition_digest) \
                         VALUES (?, ?, ?, ?, ?, ?)",
                    )
                    .bind(rev.revision as i64)
                    .bind(m.entity.kind.as_str())
                    .bind(&m.entity.id)
                    .bind(m.entity.revision as i64)
                    .bind(m.owning_package.as_deref().unwrap_or(""))
                    .bind(&m.definition_digest)
                    .execute(&mut **tx)
                    .await
                    .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
                }
            }
        }

        for rev_id in &batch.compacted_member_revisions {
            sqlx::query("DELETE FROM deployment_members WHERE deployment_revision = ?")
                .bind(*rev_id as i64)
                .execute(&mut **tx)
                .await
                .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        Ok(())
    }

    async fn write_batch_to_sqlite_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        batch: &PersistenceBatch,
        in_memory_admissions: Option<&std::collections::BTreeMap<String, ProviderAdmissionRow>>,
    ) -> Result<(), DeploymentError> {
        for art in &batch.artifacts {
            let state_str = match art.state {
                ArtifactState::Active => "active",
                ArtifactState::Draining => "draining",
                ArtifactState::Deleted => "deleted",
            };
            sqlx::query(
                "INSERT INTO artifact_objects (digest, size_bytes, state, drain_epoch, payload_bytes) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(digest) DO UPDATE SET state = excluded.state, drain_epoch = excluded.drain_epoch, payload_bytes = excluded.payload_bytes",
            )
            .bind(&art.digest)
            .bind(art.size_bytes as i64)
            .bind(state_str)
            .bind(art.drain_epoch.map(|e| e as i64))
            .bind(art.bytes.as_deref())
            .execute(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for pkg in &batch.packages {
            let tier = pkg
                .distribution_json
                .get("tier")
                .and_then(Value::as_str)
                .unwrap_or("community");
            let manifest_str = serde_json::to_string(&pkg.manifest_json)
                .map_err(|e| DeploymentError::new("SERIALIZE_ERROR", e.to_string()))?;
            let inventory_str = serde_json::to_string(&pkg.inventory_json)
                .map_err(|e| DeploymentError::new("SERIALIZE_ERROR", e.to_string()))?;
            sqlx::query(
                "INSERT INTO provider_packages (digest, publisher, provider_id, version, implementation, tier, manifest_json, inventory_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT(digest) DO NOTHING",
            )
            .bind(&pkg.package_digest)
            .bind(&pkg.publisher)
            .bind(&pkg.provider_id)
            .bind(&pkg.version)
            .bind(pkg.implementation.as_str())
            .bind(tier)
            .bind(manifest_str)
            .bind(inventory_str)
            .execute(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for adm in &batch.admissions {
            sqlx::query(
                "INSERT INTO provider_admissions (package_digest, policy_id, admission_fingerprint, revoked) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(package_digest) DO UPDATE SET revoked = excluded.revoked, admission_fingerprint = excluded.admission_fingerprint",
            )
            .bind(&adm.package_digest)
            .bind(&adm.policy_id)
            .bind(&adm.admission_fingerprint)
            .bind(adm.revoked)
            .execute(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for conn in &batch.connections {
            let cfg_str = serde_json::to_string(&conn.public_config)
                .map_err(|e| DeploymentError::new("SERIALIZE_ERROR", e.to_string()))?;
            sqlx::query(
                "INSERT INTO connection_revisions (connection_id, revision, provider_digest, public_config_json, secret_locator) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(connection_id, revision) DO NOTHING",
            )
            .bind(&conn.connection_id)
            .bind(conn.revision as i64)
            .bind(&conn.provider_digest)
            .bind(cfg_str)
            .bind(super::locators_column(&conn.secret_locators))
            .execute(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for (principal, idem_key, receipt) in &batch.receipts {
            let existing: Option<(String, i64, String)> = sqlx::query_as(
                "SELECT request_hash, committed_revision, manifest_digest FROM deployment_operations WHERE principal = ?1 AND idempotency_key = ?2",
            )
            .bind(principal)
            .bind(idem_key)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
            if let Some((db_hash, db_rev, db_digest)) = existing {
                if db_hash != receipt.request_hash {
                    return Err(DeploymentError::new(
                        "IDEMPOTENCY_KEY_CONFLICT",
                        "idempotency_key already used by principal with different request parameters",
                    ));
                }
                if db_rev != receipt.committed_revision as i64
                    || db_digest != receipt.manifest_digest
                {
                    return Err(DeploymentError::new(
                        "DEPLOYMENT_HEAD_CONFLICT",
                        format!("idempotency receipt conflicts with committed revision {db_rev}"),
                    ));
                }
            } else {
                sqlx::query(
                    "INSERT INTO deployment_operations (principal, idempotency_key, request_hash, committed_revision, manifest_digest, \
                     operation_id, epoch, receiving_node_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )
                .bind(principal)
                .bind(idem_key)
                .bind(&receipt.request_hash)
                .bind(receipt.committed_revision as i64)
                .bind(&receipt.manifest_digest)
                .bind(&receipt.operation_id)
                .bind(receipt.epoch as i64)
                .bind(&receipt.receiving_node_id)
                .execute(&mut **tx)
                .await
                .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
            }
        }

        for rev in &batch.revisions {
            for p in &rev.manifest.providers {
                let db_adm: Option<(bool,)> = sqlx::query_as(
                    "SELECT revoked FROM provider_admissions WHERE package_digest = ?1",
                )
                .bind(&p.digest)
                .fetch_optional(&mut **tx)
                .await
                .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
                match db_adm {
                    Some((true,)) => {
                        return Err(DeploymentError::new(
                            "ADMISSION_INVALIDATED",
                            format!("admission for '{}' is revoked in database", p.digest),
                        ));
                    }
                    Some((false,)) => {}
                    None => {
                        if let Some(adm_map) = in_memory_admissions
                            && let Some(adm) = adm_map.get(&p.digest)
                            && !adm.revoked
                        {
                            sqlx::query(
                                "INSERT INTO provider_admissions (package_digest, policy_id, admission_fingerprint, revoked) \
                                 VALUES (?1, ?2, ?3, ?4)",
                            )
                            .bind(&adm.package_digest)
                            .bind(&adm.policy_id)
                            .bind(&adm.admission_fingerprint)
                            .bind(adm.revoked)
                            .execute(&mut **tx)
                            .await
                            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
                        } else {
                            return Err(DeploymentError::new("PROVIDER_NOT_ADMITTED", &p.digest));
                        }
                    }
                }
            }
        }

        if batch.write_head {
            let head_row: Option<(i64, i64)> = sqlx::query_as(
                "SELECT head_revision, epoch FROM deployment_head WHERE singleton_id = 1",
            )
            .fetch_optional(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;
            let db_head = head_row.map(|(h, _)| h.max(0) as u64).unwrap_or(0);
            if let Some(expected_base) = batch.expected_head_revision
                && db_head != expected_base
            {
                return Err(DeploymentError::new(
                    "DEPLOYMENT_HEAD_CONFLICT",
                    format!(
                        "expected base_revision {expected_base}, but current database head_revision is {db_head}"
                    ),
                ));
            }
            if head_row.is_none() {
                sqlx::query(
                    "INSERT INTO deployment_head (singleton_id, head_revision, epoch) VALUES (1, ?1, ?2)",
                )
                .bind(batch.head_revision as i64)
                .bind(batch.epoch as i64)
                .execute(&mut **tx)
                .await
                .map_err(head_insert_error)?;
            } else {
                let res = sqlx::query(
                    "UPDATE deployment_head SET head_revision = ?1, epoch = ?2 WHERE singleton_id = 1 AND head_revision = ?3",
                )
                .bind(batch.head_revision as i64)
                .bind(batch.epoch as i64)
                .bind(db_head as i64)
                .execute(&mut **tx)
                .await
                .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
                if res.rows_affected() != 1 {
                    return Err(DeploymentError::new(
                        "DEPLOYMENT_HEAD_CONFLICT",
                        "concurrent modification of deployment_head detected",
                    ));
                }
            }
        }

        for rev in &batch.revisions {
            let existing_rev: Option<(String,)> = sqlx::query_as(
                "SELECT manifest_digest FROM deployment_revisions WHERE revision = ?1",
            )
            .bind(rev.revision as i64)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;

            if let Some((existing_digest,)) = existing_rev {
                if existing_digest != rev.manifest_digest {
                    return Err(DeploymentError::new(
                        "DEPLOYMENT_HEAD_CONFLICT",
                        format!(
                            "conflicting deployment revision {} already exists in database",
                            rev.revision
                        ),
                    ));
                }
                sqlx::query(
                    "UPDATE deployment_revisions SET members_compacted = ?1 WHERE revision = ?2 AND manifest_digest = ?3",
                )
                .bind(rev.members_compacted)
                .bind(rev.revision as i64)
                .bind(&rev.manifest_digest)
                .execute(&mut **tx)
                .await
                .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
            } else {
                let manifest_str = serde_json::to_string(&rev.manifest)
                    .map_err(|e| DeploymentError::new("SERIALIZE_ERROR", e.to_string()))?;
                sqlx::query(
                    "INSERT INTO deployment_revisions (revision, parent_revision, manifest_digest, manifest_json, principal, reason, members_compacted) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .bind(rev.revision as i64)
                .bind(rev.parent_revision.map(|r| r as i64))
                .bind(&rev.manifest_digest)
                .bind(manifest_str)
                .bind(&rev.created_by)
                .bind(&rev.reason)
                .bind(rev.members_compacted)
                .execute(&mut **tx)
                .await
                .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
            }

            if !rev.members_compacted {
                for m in &rev.manifest.members {
                    sqlx::query(
                        "INSERT INTO deployment_members (deployment_revision, entity_kind, entity_id, entity_revision, owning_package, definition_digest) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                         ON CONFLICT(deployment_revision, entity_kind, entity_id, entity_revision) DO NOTHING",
                    )
                    .bind(rev.revision as i64)
                    .bind(m.entity.kind.as_str())
                    .bind(&m.entity.id)
                    .bind(m.entity.revision as i64)
                    .bind(m.owning_package.as_deref().unwrap_or(""))
                    .bind(&m.definition_digest)
                    .execute(&mut **tx)
                    .await
                    .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
                }
            }
        }

        for rev_id in &batch.compacted_member_revisions {
            sqlx::query("DELETE FROM deployment_members WHERE deployment_revision = ?1")
                .bind(*rev_id as i64)
                .execute(&mut **tx)
                .await
                .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        Ok(())
    }

    async fn transactional_mutate<T, F>(
        &self,
        pool: &crate::storage::DbPool,
        mutate: F,
    ) -> Result<T, DeploymentError>
    where
        F: FnOnce(&mut DeploymentState) -> Result<T, DeploymentError>,
    {
        let _write_guard = self.write_lock.lock().await;
        match pool {
            crate::storage::DbPool::Sqlite(p) => {
                // IMMEDIATE: the mutation reads the store before it writes,
                // and a deferred transaction that does so on SQLite fails
                // with SQLITE_BUSY when another writer gets in between —
                // the case `DbPool::begin_write_tx` exists to prevent.
                let mut tx = p.begin_with("BEGIN IMMEDIATE").await.map_err(|e| {
                    DeploymentError::new("DB_TRANSACTION_ERROR", format!("failed to begin tx: {e}"))
                })?;
                let mut staged = {
                    let g = self.inner.lock().expect("store lock");
                    let mut cloned = g.clone();
                    cloned.dirty = DeploymentDirtySet::default();
                    cloned
                };
                Self::sync_staged_from_sqlite_tx(&mut staged, &mut tx).await?;
                let out = mutate(&mut staged)?;
                let batch = staged.extract_dirty_batch();
                if !batch.is_empty() {
                    Self::write_batch_to_sqlite_tx(&mut tx, &batch, Some(&staged.admissions))
                        .await?;
                    tx.commit().await.map_err(|e| {
                        DeploymentError::new("DB_COMMIT_ERROR", format!("failed to commit tx: {e}"))
                    })?;
                }
                self.publish_committed_batch(&staged, &batch);
                Ok(out)
            }
            crate::storage::DbPool::Postgres(p) => {
                let mut tx = p.begin().await.map_err(|e| {
                    DeploymentError::new("DB_TRANSACTION_ERROR", format!("failed to begin tx: {e}"))
                })?;
                let mut staged = {
                    let g = self.inner.lock().expect("store lock");
                    let mut cloned = g.clone();
                    cloned.dirty = DeploymentDirtySet::default();
                    cloned
                };
                Self::sync_staged_from_postgres_tx(&mut staged, &mut tx).await?;
                let out = mutate(&mut staged)?;
                let batch = staged.extract_dirty_batch();
                if !batch.is_empty() {
                    Self::write_batch_to_postgres_tx(&mut tx, &batch, Some(&staged.admissions))
                        .await?;
                    tx.commit().await.map_err(|e| {
                        DeploymentError::new("DB_COMMIT_ERROR", format!("failed to commit tx: {e}"))
                    })?;
                }
                self.publish_committed_batch(&staged, &batch);
                Ok(out)
            }
            crate::storage::DbPool::Mysql(p) => {
                let mut tx = p.begin().await.map_err(|e| {
                    DeploymentError::new("DB_TRANSACTION_ERROR", format!("failed to begin tx: {e}"))
                })?;
                let mut staged = {
                    let g = self.inner.lock().expect("store lock");
                    let mut cloned = g.clone();
                    cloned.dirty = DeploymentDirtySet::default();
                    cloned
                };
                Self::sync_staged_from_mysql_tx(&mut staged, &mut tx).await?;
                let out = mutate(&mut staged)?;
                let batch = staged.extract_dirty_batch();
                if !batch.is_empty() {
                    Self::write_batch_to_mysql_tx(&mut tx, &batch, Some(&staged.admissions))
                        .await?;
                    tx.commit().await.map_err(|e| {
                        DeploymentError::new("DB_COMMIT_ERROR", format!("failed to commit tx: {e}"))
                    })?;
                }
                self.publish_committed_batch(&staged, &batch);
                Ok(out)
            }
        }
    }

    pub async fn ingest_verified_archive_in_db(
        &self,
        pool: &crate::storage::DbPool,
        entries: &[ArchiveEntryInput],
    ) -> Result<ProviderPackageRow, DeploymentError> {
        let (id_key, package_digest, row, raw_inv, hashed_payloads) =
            Self::prepare_archive_ingestion(entries)?;
        self.transactional_mutate(pool, move |staged| {
            Self::finish_ingest_verified_archive(
                staged,
                id_key,
                package_digest,
                row,
                raw_inv,
                hashed_payloads,
            )
        })
        .await
    }

    pub async fn admit_package_in_db(
        &self,
        pool: &crate::storage::DbPool,
        package_digest: &str,
        policy: &dyn AdmissionPolicy,
        lock: Option<&Value>,
    ) -> Result<ProviderAdmissionRow, DeploymentError> {
        self.transactional_mutate(pool, |staged| {
            Self::admit_package_into_state(staged, package_digest, policy, lock)
        })
        .await
    }

    pub async fn revoke_package_in_db(
        &self,
        pool: &crate::storage::DbPool,
        package_digest: &str,
    ) -> Result<(), DeploymentError> {
        self.transactional_mutate(pool, |staged| {
            Self::revoke_package_into_state(staged, package_digest)
        })
        .await
    }

    pub async fn store_definition_artifact_in_db(
        &self,
        pool: &crate::storage::DbPool,
        bytes: Vec<u8>,
    ) -> Result<ArtifactObjectRow, DeploymentError> {
        self.transactional_mutate(pool, move |staged| {
            Self::store_definition_artifact_into_state(staged, bytes)
        })
        .await
    }

    pub async fn store_connection_revision_in_db(
        &self,
        pool: &crate::storage::DbPool,
        connection_id: &str,
        revision: u64,
        provider_digest: &str,
        public_config: Value,
        secret_locators: std::collections::BTreeMap<String, String>,
    ) -> Result<ConnectionRevisionRow, DeploymentError> {
        let (row, cfg_digest, cfg_bytes) = Self::prepare_connection_revision(
            connection_id,
            revision,
            provider_digest,
            public_config,
            secret_locators,
        )?;
        self.transactional_mutate(pool, move |staged| {
            Self::finish_store_connection_revision(staged, row, cfg_digest, cfg_bytes)
        })
        .await
    }

    pub async fn plan_candidate_in_db(
        &self,
        pool: &crate::storage::DbPool,
        base_revision: u64,
        candidate_manifest: CanonicalManifest,
    ) -> Result<CandidatePlan, DeploymentError> {
        self.transactional_mutate(pool, move |staged| {
            Self::plan_candidate_on_state(staged, base_revision, candidate_manifest)
        })
        .await
    }

    pub async fn apply_candidate_in_db(
        &self,
        pool: &crate::storage::DbPool,
        principal: &str,
        idempotency_key: &str,
        plan: &CandidatePlan,
        reason: &str,
        policy: &dyn AdmissionPolicy,
    ) -> Result<DeploymentReceipt, DeploymentError> {
        self.transactional_mutate(pool, |staged| {
            Self::apply_candidate_into_state(
                staged,
                principal,
                idempotency_key,
                plan,
                reason,
                policy,
            )
        })
        .await
    }

    pub async fn rollback_to_revision_in_db(
        &self,
        pool: &crate::storage::DbPool,
        rollback: &Rollback<'_>,
        policy: &dyn AdmissionPolicy,
    ) -> Result<(DeploymentReceipt, Vec<String>), DeploymentError> {
        self.transactional_mutate(pool, |staged| {
            Self::rollback_to_revision_into_state(staged, rollback, policy)
        })
        .await
    }

    /// Persist the current deployment store state across any configured [`crate::storage::DbPool`]
    /// backend (`SQLite`, `PostgreSQL`, or `MySQL` — `ARCH-02`).
    pub async fn persist_to_db_pool(
        &self,
        pool: &crate::storage::DbPool,
    ) -> Result<(), DeploymentError> {
        match pool {
            crate::storage::DbPool::Sqlite(p) => self.persist_to_sqlite(p).await,
            crate::storage::DbPool::Postgres(p) => self.persist_to_postgres(p).await,
            crate::storage::DbPool::Mysql(p) => self.persist_to_mysql(p).await,
        }
    }

    /// Persist dirty deployment store changes to a PostgreSQL pool (`027_v2_immutable_deployments.sql`).
    pub async fn persist_to_postgres(&self, pool: &sqlx::PgPool) -> Result<(), DeploymentError> {
        let batch = self.extract_dirty_batch();
        if batch.is_empty() {
            return Ok(());
        }

        let mut tx = pool.begin().await.map_err(|e| {
            DeploymentError::new("DB_TRANSACTION_ERROR", format!("failed to begin tx: {e}"))
        })?;
        Self::write_batch_to_postgres_tx(&mut tx, &batch, None).await?;
        tx.commit().await.map_err(|e| {
            DeploymentError::new("DB_COMMIT_ERROR", format!("failed to commit tx: {e}"))
        })?;
        self.acknowledge_persisted_batch(&batch);
        Ok(())
    }

    /// Persist dirty deployment store changes to a MySQL pool (`026_v2_immutable_deployments.sql`).
    pub async fn persist_to_mysql(&self, pool: &sqlx::MySqlPool) -> Result<(), DeploymentError> {
        let batch = self.extract_dirty_batch();
        if batch.is_empty() {
            return Ok(());
        }

        let mut tx = pool.begin().await.map_err(|e| {
            DeploymentError::new("DB_TRANSACTION_ERROR", format!("failed to begin tx: {e}"))
        })?;
        Self::write_batch_to_mysql_tx(&mut tx, &batch, None).await?;
        tx.commit().await.map_err(|e| {
            DeploymentError::new("DB_COMMIT_ERROR", format!("failed to commit tx: {e}"))
        })?;
        self.acknowledge_persisted_batch(&batch);
        Ok(())
    }

    /// Persist dirty deployment store changes (`deployment_head`, `deployment_revisions`,
    /// `deployment_members`, `deployment_operations`, `artifact_objects`, `provider_packages`,
    /// `provider_admissions`, and `connection_revisions`) to a SQLite pool migrated with
    /// `023_v2_immutable_deployments.sql` (`V2-03.2`–`V2-03.4`, `TD-14`, `TD-23`).
    pub async fn persist_to_sqlite(&self, pool: &sqlx::SqlitePool) -> Result<(), DeploymentError> {
        let batch = self.extract_dirty_batch();
        if batch.is_empty() {
            return Ok(());
        }

        let mut tx = pool.begin().await.map_err(|e| {
            DeploymentError::new("DB_TRANSACTION_ERROR", format!("failed to begin tx: {e}"))
        })?;
        Self::write_batch_to_sqlite_tx(&mut tx, &batch, None).await?;
        tx.commit().await.map_err(|e| {
            DeploymentError::new("DB_COMMIT_ERROR", format!("failed to commit tx: {e}"))
        })?;
        self.acknowledge_persisted_batch(&batch);
        Ok(())
    }

    /// The persisted `(head_revision, epoch)`, one row: what a node polls to
    /// learn whether anything it serves from has changed. `None` before the
    /// first commit, or before the table exists.
    pub async fn persisted_head(
        pool: &crate::storage::DbPool,
    ) -> Result<Option<(u64, u64)>, DeploymentError> {
        const SQL: &str = "SELECT head_revision, epoch FROM deployment_head WHERE singleton_id = 1";
        let row: Result<Option<(i64, i64)>, sqlx::Error> = match pool {
            crate::storage::DbPool::Sqlite(p) => sqlx::query_as(SQL).fetch_optional(p).await,
            crate::storage::DbPool::Postgres(p) => sqlx::query_as(SQL).fetch_optional(p).await,
            crate::storage::DbPool::Mysql(p) => sqlx::query_as(SQL).fetch_optional(p).await,
        };
        match row {
            Ok(row) => Ok(row.map(|(h, e)| (h.max(0) as u64, e.max(0) as u64))),
            Err(e) => {
                let text = e.to_string();
                if ["no such table", "does not exist", "doesn't exist"]
                    .iter()
                    .any(|missing| text.contains(missing))
                {
                    Ok(None)
                } else {
                    Err(DeploymentError::new("DB_READ_ERROR", text))
                }
            }
        }
    }

    /// Hydrate the deployment store from any configured [`crate::storage::DbPool`] backend (`ARCH-02`).
    pub async fn load_from_db_pool(
        &self,
        pool: &crate::storage::DbPool,
    ) -> Result<(), DeploymentError> {
        match pool {
            crate::storage::DbPool::Sqlite(p) => self.load_from_sqlite(p).await,
            crate::storage::DbPool::Postgres(p) => self.load_from_postgres(p).await,
            crate::storage::DbPool::Mysql(p) => self.load_from_mysql(p).await,
        }
    }

    /// Hydrate from PostgreSQL into `self.inner` (`ARCH-02`, `TD-15`, `TD-16`).
    pub async fn load_from_postgres(&self, pool: &sqlx::PgPool) -> Result<(), DeploymentError> {
        let _guard = self.write_lock.lock().await;
        let head_row: Result<Option<(i64, i64)>, sqlx::Error> = sqlx::query_as(
            "SELECT head_revision, epoch FROM deployment_head WHERE singleton_id = 1",
        )
        .fetch_optional(pool)
        .await;

        let (head_rev, epoch) = match head_row {
            Ok(Some((h, e))) => (h.max(0) as u64, e.max(0) as u64),
            Ok(None) => (0, 0),
            Err(e) if e.to_string().contains("does not exist") => return Ok(()),
            Err(e) => return Err(DeploymentError::new("DB_READ_ERROR", e.to_string())),
        };

        let art_rows: Vec<PersistedArtifactTuple> = sqlx::query_as(
            "SELECT digest, size_bytes, state, drain_epoch, payload_bytes FROM artifact_objects",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;

        let rev_rows: Vec<PersistedRevisionTuple> = sqlx::query_as(
            "SELECT revision, parent_revision, manifest_digest, manifest_json, principal, reason, members_compacted \
             FROM deployment_revisions ORDER BY revision ASC",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;

        let pkg_rows: Vec<PersistedPackageTuple> = sqlx::query_as(
            "SELECT digest, publisher, provider_id, version, implementation, tier, manifest_json, inventory_json \
             FROM provider_packages",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;

        let adm_rows: Vec<(String, String, String, bool)> = sqlx::query_as(
            "SELECT package_digest, policy_id, admission_fingerprint, revoked FROM provider_admissions",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;

        let conn_rows: Vec<(String, i64, String, String, Option<String>)> = sqlx::query_as(
            "SELECT connection_id, revision, provider_digest, public_config_json, secret_locator FROM connection_revisions",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;

        let op_rows: Vec<PersistedOperationTuple> = sqlx::query_as(SELECT_OPERATIONS)
            .fetch_all(pool)
            .await
            .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;

        self.hydrate_rows(
            head_rev, epoch, art_rows, rev_rows, pkg_rows, adm_rows, conn_rows, op_rows,
        )
    }

    /// Hydrate from MySQL into `self.inner` (`ARCH-02`, `TD-15`, `TD-16`).
    pub async fn load_from_mysql(&self, pool: &sqlx::MySqlPool) -> Result<(), DeploymentError> {
        let _guard = self.write_lock.lock().await;
        let head_row: Result<Option<(i64, i64)>, sqlx::Error> = sqlx::query_as(
            "SELECT head_revision, epoch FROM deployment_head WHERE singleton_id = 1",
        )
        .fetch_optional(pool)
        .await;

        let (head_rev, epoch) = match head_row {
            Ok(Some((h, e))) => (h.max(0) as u64, e.max(0) as u64),
            Ok(None) => (0, 0),
            Err(e) if e.to_string().contains("doesn't exist") => return Ok(()),
            Err(e) => return Err(DeploymentError::new("DB_READ_ERROR", e.to_string())),
        };

        let art_rows: Vec<PersistedArtifactTuple> = sqlx::query_as(
            "SELECT digest, size_bytes, state, drain_epoch, payload_bytes FROM artifact_objects",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;

        let rev_rows: Vec<PersistedRevisionTuple> = sqlx::query_as(
            "SELECT revision, parent_revision, manifest_digest, manifest_json, principal, reason, members_compacted \
             FROM deployment_revisions ORDER BY revision ASC",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;

        let pkg_rows: Vec<PersistedPackageTuple> = sqlx::query_as(
            "SELECT digest, publisher, provider_id, version, implementation, tier, manifest_json, inventory_json \
             FROM provider_packages",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;

        let adm_rows: Vec<(String, String, String, bool)> = sqlx::query_as(
            "SELECT package_digest, policy_id, admission_fingerprint, revoked FROM provider_admissions",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;

        let conn_rows: Vec<(String, i64, String, String, Option<String>)> = sqlx::query_as(
            "SELECT connection_id, revision, provider_digest, public_config_json, secret_locator FROM connection_revisions",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;

        let op_rows: Vec<PersistedOperationTuple> = sqlx::query_as(SELECT_OPERATIONS)
            .fetch_all(pool)
            .await
            .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;

        self.hydrate_rows(
            head_rev, epoch, art_rows, rev_rows, pkg_rows, adm_rows, conn_rows, op_rows,
        )
    }

    /// Hydrate `deployment_head`, `artifact_objects`, `deployment_revisions`,
    /// `provider_packages`, `provider_admissions`, `connection_revisions`, and
    /// `deployment_operations` from SQLite into `self.inner` (`ARCH-02`, `TD-15`, `TD-16`).
    ///
    /// Gracefully ignores missing-table errors if v2 migrations have not been run on a bare test DB.
    pub async fn load_from_sqlite(&self, pool: &sqlx::SqlitePool) -> Result<(), DeploymentError> {
        let _guard = self.write_lock.lock().await;
        let head_row: Result<Option<(i64, i64)>, sqlx::Error> = sqlx::query_as(
            "SELECT head_revision, epoch FROM deployment_head WHERE singleton_id = 1",
        )
        .fetch_optional(pool)
        .await;

        let (head_rev, epoch) = match head_row {
            Ok(Some((h, e))) => (h.max(0) as u64, e.max(0) as u64),
            Ok(None) => (0, 0),
            Err(e) if e.to_string().contains("no such table") => return Ok(()),
            Err(e) => return Err(DeploymentError::new("DB_READ_ERROR", e.to_string())),
        };

        let art_rows: Vec<PersistedArtifactTuple> = sqlx::query_as(
            "SELECT digest, size_bytes, state, drain_epoch, payload_bytes FROM artifact_objects",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;

        let rev_rows: Vec<PersistedRevisionTuple> = sqlx::query_as(
            "SELECT revision, parent_revision, manifest_digest, manifest_json, principal, reason, members_compacted \
             FROM deployment_revisions ORDER BY revision ASC",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;

        let pkg_rows: Vec<PersistedPackageTuple> = sqlx::query_as(
            "SELECT digest, publisher, provider_id, version, implementation, tier, manifest_json, inventory_json \
             FROM provider_packages",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;

        let adm_rows: Vec<(String, String, String, bool)> = sqlx::query_as(
            "SELECT package_digest, policy_id, admission_fingerprint, revoked FROM provider_admissions",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;

        let conn_rows: Vec<(String, i64, String, String, Option<String>)> = sqlx::query_as(
            "SELECT connection_id, revision, provider_digest, public_config_json, secret_locator FROM connection_revisions",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;

        let op_rows: Vec<PersistedOperationTuple> = sqlx::query_as(SELECT_OPERATIONS)
            .fetch_all(pool)
            .await
            .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;

        self.hydrate_rows(
            head_rev, epoch, art_rows, rev_rows, pkg_rows, adm_rows, conn_rows, op_rows,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn hydrate_rows(
        &self,
        head_rev: u64,
        epoch: u64,
        art_rows: Vec<PersistedArtifactTuple>,
        rev_rows: Vec<PersistedRevisionTuple>,
        pkg_rows: Vec<PersistedPackageTuple>,
        adm_rows: Vec<(String, String, String, bool)>,
        conn_rows: Vec<(String, i64, String, String, Option<String>)>,
        op_rows: Vec<PersistedOperationTuple>,
    ) -> Result<(), DeploymentError> {
        let mut g = self.inner.lock().expect("store lock");
        if head_rev >= g.head_revision {
            g.head_revision = head_rev;
            g.epoch = epoch.max(g.epoch);
        }

        for (digest, size_bytes, state_str, drain_epoch, payload_bytes) in art_rows {
            let state = match state_str.as_str() {
                "active" | "ACTIVE" => ArtifactState::Active,
                "draining" | "DRAINING" => ArtifactState::Draining,
                "deleted" | "DELETED" => ArtifactState::Deleted,
                other => {
                    return Err(DeploymentError::new(
                        "CORRUPT_PERSISTED_STATE",
                        format!("unknown artifact state '{other}' for '{digest}'"),
                    ));
                }
            };
            if state != ArtifactState::Deleted
                && let Some(ref bytes) = payload_bytes
            {
                let computed = sha256_digest(bytes);
                if computed != digest {
                    return Err(DeploymentError::new(
                        "CORRUPT_PERSISTED_STATE",
                        format!(
                            "artifact '{digest}' payload digest mismatch (computed '{computed}')"
                        ),
                    ));
                }
            }
            g.artifacts.insert(
                digest.clone(),
                ArtifactObjectRow {
                    digest,
                    size_bytes: size_bytes.max(0) as usize,
                    state,
                    drain_epoch: drain_epoch.map(|e| e.max(0) as u64),
                    bytes: payload_bytes,
                },
            );
        }

        for (
            digest,
            publisher,
            provider_id,
            version,
            impl_str,
            tier,
            manifest_str,
            inventory_str,
        ) in pkg_rows
        {
            let implementation = match impl_str.as_str() {
                "workflow" => ImplementationKind::Workflow,
                "wasm" => ImplementationKind::Wasm,
                "rust" => ImplementationKind::Rust,
                other => {
                    return Err(DeploymentError::new(
                        "CORRUPT_PERSISTED_STATE",
                        format!("unknown package implementation '{other}' for '{digest}'"),
                    ));
                }
            };
            let manifest_json: Value = serde_json::from_str(&manifest_str).map_err(|e| {
                DeploymentError::new(
                    "CORRUPT_PERSISTED_STATE",
                    format!("corrupt manifest_json for package '{digest}': {e}"),
                )
            })?;
            let inventory_json: Value = serde_json::from_str(&inventory_str).map_err(|e| {
                DeploymentError::new(
                    "CORRUPT_PERSISTED_STATE",
                    format!("corrupt inventory_json for package '{digest}': {e}"),
                )
            })?;
            let distribution_json = json!({
                "publisher": publisher,
                "tier": tier,
            });
            g.artifacts
                .entry(digest.clone())
                .or_insert_with(|| ArtifactObjectRow {
                    digest: digest.clone(),
                    size_bytes: inventory_str.len(),
                    state: ArtifactState::Active,
                    drain_epoch: None,
                    bytes: None,
                });
            g.publisher_bindings.insert(
                (publisher.clone(), provider_id.clone(), version.clone()),
                digest.clone(),
            );
            g.packages.insert(
                digest.clone(),
                ProviderPackageRow {
                    package_digest: digest,
                    publisher,
                    provider_id,
                    version,
                    implementation,
                    manifest_json,
                    distribution_json,
                    inventory_json,
                },
            );
        }

        for (package_digest, policy_id, admission_fingerprint, revoked) in adm_rows {
            g.admissions.insert(
                package_digest.clone(),
                ProviderAdmissionRow {
                    package_digest,
                    admission_fingerprint,
                    policy_id,
                    revoked,
                },
            );
        }

        for (connection_id, rev, provider_digest, cfg_str, secret_locator) in conn_rows {
            let revision = rev.max(1) as u64;
            let public_config: Value = serde_json::from_str(&cfg_str).map_err(|e| {
                DeploymentError::new(
                    "CORRUPT_PERSISTED_STATE",
                    format!(
                        "corrupt public_config_json for connection '{connection_id}' rev {revision}: {e}"
                    ),
                )
            })?;
            let secret_locators = super::locators_from_column(secret_locator).map_err(|e| {
                DeploymentError::new(
                    "CORRUPT_PERSISTED_STATE",
                    format!("connection '{connection_id}' rev {revision}: {e}"),
                )
            })?;
            let cfg_bytes = super::connection_config_bytes(&public_config, &secret_locators);
            let configuration_digest = sha256_digest(&cfg_bytes);
            g.artifacts
                .entry(configuration_digest.clone())
                .or_insert_with(|| ArtifactObjectRow {
                    digest: configuration_digest.clone(),
                    size_bytes: cfg_bytes.len(),
                    state: ArtifactState::Active,
                    drain_epoch: None,
                    bytes: Some(cfg_bytes),
                });
            g.connections.insert(
                (connection_id.clone(), revision),
                ConnectionRevisionRow {
                    connection_id,
                    revision,
                    provider_digest,
                    public_config,
                    secret_locators,
                    configuration_digest,
                },
            );
        }

        for (rev, parent, digest, manifest_str, principal, reason, compacted) in rev_rows {
            let rev_u64 = rev.max(0) as u64;
            let manifest: CanonicalManifest = serde_json::from_str(&manifest_str).map_err(|e| {
                DeploymentError::new(
                    "CORRUPT_PERSISTED_STATE",
                    format!("corrupt manifest_json for revision {rev_u64}: {e}"),
                )
            })?;
            let (manifest_bytes, computed_digest) = manifest.canonicalize_and_verify()?;
            if computed_digest != digest {
                return Err(DeploymentError::new(
                    "CORRUPT_PERSISTED_STATE",
                    format!(
                        "revision {rev_u64} manifest_digest mismatch: stored '{digest}', computed '{computed_digest}'"
                    ),
                ));
            }
            for m in &manifest.members {
                g.artifacts
                    .entry(m.definition_digest.clone())
                    .or_insert_with(|| ArtifactObjectRow {
                        digest: m.definition_digest.clone(),
                        size_bytes: 0,
                        state: ArtifactState::Active,
                        drain_epoch: None,
                        bytes: None,
                    });
            }
            g.revisions.insert(
                rev_u64,
                DeploymentRevisionRow {
                    revision: rev_u64,
                    parent_revision: parent.map(|p| p.max(0) as u64),
                    manifest_digest: digest,
                    manifest_bytes,
                    manifest,
                    reason,
                    created_by: principal,
                    members_compacted: compacted,
                },
            );
        }

        if head_rev > 0 && !g.revisions.contains_key(&head_rev) {
            return Err(DeploymentError::new(
                "CORRUPT_PERSISTED_STATE",
                format!(
                    "deployment_head references revision {head_rev}, which is missing from deployment_revisions"
                ),
            ));
        }

        let node_id = g.node_id.clone();
        for row in op_rows {
            let receipt = receipt_from_row(row, &node_id);
            g.receipts.insert(
                (receipt.principal.clone(), receipt.idempotency_key.clone()),
                receipt,
            );
        }

        g.dirty = DeploymentDirtySet::default();
        Ok(())
    }
}
