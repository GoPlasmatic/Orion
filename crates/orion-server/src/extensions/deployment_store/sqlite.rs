//! SQLite, PostgreSQL, and MySQL persistence and startup hydration for [`ImmutableDeploymentStore`] (`V2-03.2`..`V2-03.4`, `ARCH-02`, `MOD-02`).

use serde_json::{Value, json};

use super::{
    ArtifactObjectRow, ArtifactState, CanonicalManifest, ConnectionRevisionRow,
    DeploymentDirtySet, DeploymentError, DeploymentReceipt, DeploymentRevisionRow,
    ImmutableDeploymentStore, ProviderAdmissionRow, ProviderPackageRow,
};
use crate::extensions::contract::ImplementationKind;
use crate::extensions::package_identity::{canonical_bytes, sha256_digest};

type PersistedArtifactTuple = (String, i64, String, Option<i64>, Option<Vec<u8>>);
type PersistedRevisionTuple = (i64, Option<i64>, String, String, String, String, bool);
type PersistedOperationTuple = (String, String, String, i64, String);
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

impl ImmutableDeploymentStore {
    fn extract_dirty_batch(&self) -> PersistenceBatch {
        let g = self.inner.lock().expect("store lock");
        PersistenceBatch {
            write_head: g.dirty.head,
            head_revision: g.head_revision,
            epoch: g.epoch,
            artifacts: g
                .dirty
                .artifacts
                .iter()
                .filter_map(|d| g.artifacts.get(d).cloned())
                .collect(),
            packages: g
                .dirty
                .packages
                .iter()
                .filter_map(|d| g.packages.get(d).cloned())
                .collect(),
            admissions: g
                .dirty
                .admissions
                .iter()
                .filter_map(|d| g.admissions.get(d).cloned())
                .collect(),
            connections: g
                .dirty
                .connections
                .iter()
                .filter_map(|k| g.connections.get(k).cloned())
                .collect(),
            revisions: g
                .dirty
                .revisions
                .iter()
                .filter_map(|r| g.revisions.get(r).cloned())
                .collect(),
            receipts: g
                .dirty
                .receipts
                .iter()
                .filter_map(|k| {
                    g.receipts
                        .get(k)
                        .map(|r| (k.0.clone(), k.1.clone(), r.clone()))
                })
                .collect(),
            compacted_member_revisions: g
                .dirty
                .compacted_member_revisions
                .iter()
                .copied()
                .collect(),
        }
    }

    fn acknowledge_persisted_batch(&self, batch: &PersistenceBatch) {
        let mut g = self.inner.lock().expect("store lock");
        if batch.write_head && g.head_revision == batch.head_revision && g.epoch == batch.epoch {
            g.dirty.head = false;
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

        if batch.write_head {
            sqlx::query(
                "INSERT INTO deployment_head (singleton_id, head_revision, epoch) VALUES (1, $1, $2) \
                 ON CONFLICT(singleton_id) DO UPDATE SET head_revision = EXCLUDED.head_revision, epoch = EXCLUDED.epoch",
            )
            .bind(batch.head_revision as i64)
            .bind(batch.epoch as i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

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
            .execute(&mut *tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for pkg in &batch.packages {
            let tier = pkg
                .distribution_json
                .get("tier")
                .and_then(Value::as_str)
                .unwrap_or("community");
            let manifest_str =
                serde_json::to_string(&pkg.manifest_json).unwrap_or_else(|_| "{}".into());
            let inventory_str =
                serde_json::to_string(&pkg.inventory_json).unwrap_or_else(|_| "{}".into());
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
            .execute(&mut *tx)
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
            .execute(&mut *tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for conn in &batch.connections {
            let cfg_str =
                serde_json::to_string(&conn.public_config).unwrap_or_else(|_| "{}".into());
            sqlx::query(
                "INSERT INTO connection_revisions (connection_id, revision, provider_digest, public_config_json, secret_locator) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT(connection_id, revision) DO NOTHING",
            )
            .bind(&conn.connection_id)
            .bind(conn.revision as i64)
            .bind(&conn.provider_digest)
            .bind(cfg_str)
            .bind(conn.secret_locator.as_deref())
            .execute(&mut *tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for rev in &batch.revisions {
            let manifest_str = serde_json::to_string(&rev.manifest).unwrap_or_else(|_| "{}".into());
            sqlx::query(
                "INSERT INTO deployment_revisions (revision, parent_revision, manifest_digest, manifest_json, principal, reason, members_compacted) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7) \
                 ON CONFLICT(revision) DO UPDATE SET members_compacted = EXCLUDED.members_compacted",
            )
            .bind(rev.revision as i64)
            .bind(rev.parent_revision.map(|r| r as i64))
            .bind(&rev.manifest_digest)
            .bind(manifest_str)
            .bind(&rev.created_by)
            .bind(&rev.reason)
            .bind(rev.members_compacted)
            .execute(&mut *tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;

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
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
                }
            }
        }

        for rev_id in &batch.compacted_member_revisions {
            sqlx::query("DELETE FROM deployment_members WHERE deployment_revision = $1")
                .bind(*rev_id as i64)
                .execute(&mut *tx)
                .await
                .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for (principal, idem_key, receipt) in &batch.receipts {
            sqlx::query(
                "INSERT INTO deployment_operations (principal, idempotency_key, request_hash, committed_revision, manifest_digest) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT(principal, idempotency_key) DO NOTHING",
            )
            .bind(principal)
            .bind(idem_key)
            .bind(&receipt.request_hash)
            .bind(receipt.committed_revision as i64)
            .bind(&receipt.manifest_digest)
            .execute(&mut *tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

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

        if batch.write_head {
            sqlx::query(
                "INSERT INTO deployment_head (singleton_id, head_revision, epoch) VALUES (1, ?, ?) \
                 ON DUPLICATE KEY UPDATE head_revision = VALUES(head_revision), epoch = VALUES(epoch)",
            )
            .bind(batch.head_revision as i64)
            .bind(batch.epoch as i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

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
            .execute(&mut *tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for pkg in &batch.packages {
            let tier = pkg
                .distribution_json
                .get("tier")
                .and_then(Value::as_str)
                .unwrap_or("community");
            let manifest_str =
                serde_json::to_string(&pkg.manifest_json).unwrap_or_else(|_| "{}".into());
            let inventory_str =
                serde_json::to_string(&pkg.inventory_json).unwrap_or_else(|_| "{}".into());
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
            .execute(&mut *tx)
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
            .execute(&mut *tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for conn in &batch.connections {
            let cfg_str =
                serde_json::to_string(&conn.public_config).unwrap_or_else(|_| "{}".into());
            sqlx::query(
                "INSERT IGNORE INTO connection_revisions (connection_id, revision, provider_digest, public_config_json, secret_locator) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(&conn.connection_id)
            .bind(conn.revision as i64)
            .bind(&conn.provider_digest)
            .bind(cfg_str)
            .bind(conn.secret_locator.as_deref())
            .execute(&mut *tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for rev in &batch.revisions {
            let manifest_str = serde_json::to_string(&rev.manifest).unwrap_or_else(|_| "{}".into());
            sqlx::query(
                "INSERT INTO deployment_revisions (revision, parent_revision, manifest_digest, manifest_json, principal, reason, members_compacted) \
                 VALUES (?, ?, ?, ?, ?, ?, ?) \
                 ON DUPLICATE KEY UPDATE members_compacted = VALUES(members_compacted)",
            )
            .bind(rev.revision as i64)
            .bind(rev.parent_revision.map(|r| r as i64))
            .bind(&rev.manifest_digest)
            .bind(manifest_str)
            .bind(&rev.created_by)
            .bind(&rev.reason)
            .bind(rev.members_compacted)
            .execute(&mut *tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;

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
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
                }
            }
        }

        for rev_id in &batch.compacted_member_revisions {
            sqlx::query("DELETE FROM deployment_members WHERE deployment_revision = ?")
                .bind(*rev_id as i64)
                .execute(&mut *tx)
                .await
                .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for (principal, idem_key, receipt) in &batch.receipts {
            sqlx::query(
                "INSERT IGNORE INTO deployment_operations (principal, idempotency_key, request_hash, committed_revision, manifest_digest) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(principal)
            .bind(idem_key)
            .bind(&receipt.request_hash)
            .bind(receipt.committed_revision as i64)
            .bind(&receipt.manifest_digest)
            .execute(&mut *tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

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

        if batch.write_head {
            sqlx::query(
                "INSERT INTO deployment_head (singleton_id, head_revision, epoch) VALUES (1, ?1, ?2) \
                 ON CONFLICT(singleton_id) DO UPDATE SET head_revision = excluded.head_revision, epoch = excluded.epoch",
            )
            .bind(batch.head_revision as i64)
            .bind(batch.epoch as i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

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
            .execute(&mut *tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for pkg in &batch.packages {
            let tier = pkg
                .distribution_json
                .get("tier")
                .and_then(Value::as_str)
                .unwrap_or("community");
            let manifest_str =
                serde_json::to_string(&pkg.manifest_json).unwrap_or_else(|_| "{}".into());
            let inventory_str =
                serde_json::to_string(&pkg.inventory_json).unwrap_or_else(|_| "{}".into());
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
            .execute(&mut *tx)
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
            .execute(&mut *tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for conn in &batch.connections {
            let cfg_str =
                serde_json::to_string(&conn.public_config).unwrap_or_else(|_| "{}".into());
            sqlx::query(
                "INSERT INTO connection_revisions (connection_id, revision, provider_digest, public_config_json, secret_locator) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(connection_id, revision) DO NOTHING",
            )
            .bind(&conn.connection_id)
            .bind(conn.revision as i64)
            .bind(&conn.provider_digest)
            .bind(cfg_str)
            .bind(conn.secret_locator.as_deref())
            .execute(&mut *tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for rev in &batch.revisions {
            let manifest_str = serde_json::to_string(&rev.manifest).unwrap_or_else(|_| "{}".into());
            sqlx::query(
                "INSERT INTO deployment_revisions (revision, parent_revision, manifest_digest, manifest_json, principal, reason, members_compacted) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT(revision) DO UPDATE SET members_compacted = excluded.members_compacted",
            )
            .bind(rev.revision as i64)
            .bind(rev.parent_revision.map(|r| r as i64))
            .bind(&rev.manifest_digest)
            .bind(manifest_str)
            .bind(&rev.created_by)
            .bind(&rev.reason)
            .bind(rev.members_compacted)
            .execute(&mut *tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;

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
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
                }
            }
        }

        for rev_id in &batch.compacted_member_revisions {
            sqlx::query("DELETE FROM deployment_members WHERE deployment_revision = ?1")
                .bind(*rev_id as i64)
                .execute(&mut *tx)
                .await
                .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        for (principal, idem_key, receipt) in &batch.receipts {
            sqlx::query(
                "INSERT INTO deployment_operations (principal, idempotency_key, request_hash, committed_revision, manifest_digest) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(principal, idempotency_key) DO NOTHING",
            )
            .bind(principal)
            .bind(idem_key)
            .bind(&receipt.request_hash)
            .bind(receipt.committed_revision as i64)
            .bind(&receipt.manifest_digest)
            .execute(&mut *tx)
            .await
            .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
        }

        tx.commit().await.map_err(|e| {
            DeploymentError::new("DB_COMMIT_ERROR", format!("failed to commit tx: {e}"))
        })?;
        self.acknowledge_persisted_batch(&batch);
        Ok(())
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

        let op_rows: Vec<PersistedOperationTuple> = sqlx::query_as(
            "SELECT principal, idempotency_key, request_hash, committed_revision, manifest_digest FROM deployment_operations",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| DeploymentError::new("DB_READ_ERROR", e.to_string()))?;

        self.hydrate_rows(
            head_rev, epoch, art_rows, rev_rows, pkg_rows, adm_rows, conn_rows, op_rows,
        )
    }

    /// Hydrate from MySQL into `self.inner` (`ARCH-02`, `TD-15`, `TD-16`).
    pub async fn load_from_mysql(&self, pool: &sqlx::MySqlPool) -> Result<(), DeploymentError> {
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

        let op_rows: Vec<PersistedOperationTuple> = sqlx::query_as(
            "SELECT principal, idempotency_key, request_hash, committed_revision, manifest_digest FROM deployment_operations",
        )
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

        let op_rows: Vec<PersistedOperationTuple> = sqlx::query_as(
            "SELECT principal, idempotency_key, request_hash, committed_revision, manifest_digest FROM deployment_operations",
        )
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
        g.head_revision = head_rev;
        g.epoch = epoch;

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
            let cfg_bytes = canonical_bytes(&public_config).map_err(|e| {
                DeploymentError::new(
                    "CORRUPT_PERSISTED_STATE",
                    format!(
                        "non-canonical public_config_json for connection '{connection_id}' rev {revision}: {}",
                        e.code
                    ),
                )
            })?;
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
                    secret_locator,
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
        for (principal, idempotency_key, request_hash, committed_rev, manifest_digest) in op_rows {
            let rev_u64 = committed_rev.max(0) as u64;
            g.receipts.insert(
                (principal.clone(), idempotency_key.clone()),
                DeploymentReceipt {
                    operation_id: format!("op-{rev_u64}-{epoch}"),
                    principal,
                    idempotency_key,
                    request_hash,
                    committed_revision: rev_u64,
                    manifest_digest,
                    epoch,
                    receiving_node_id: node_id.clone(),
                },
            );
        }

        g.dirty = DeploymentDirtySet::default();
        Ok(())
    }
}
