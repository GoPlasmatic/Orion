//! SQLite persistence and startup hydration for [`ImmutableDeploymentStore`] (`V2-03.2`..`V2-03.4`, `ARCH-02`, `MOD-02`).

use serde_json::{Value, json};

use super::{
    ArtifactState, CanonicalManifest, ConnectionRevisionRow, DeploymentError,
    DeploymentRevisionRow, ImmutableDeploymentStore, ProviderAdmissionRow, ProviderPackageRow,
};
use crate::extensions::contract::ImplementationKind;
use crate::extensions::package_identity::sha256_digest;

type PersistedRevisionTuple = (i64, Option<i64>, String, String, String, String, bool);
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

impl ImmutableDeploymentStore {
    /// Persist the current deployment store state (`deployment_head`, `deployment_revisions`,
    /// `deployment_members`, `deployment_operations`, `artifact_objects`, `provider_packages`,
    /// `provider_admissions`, and `connection_revisions`) to a SQLite pool migrated with
    /// `023_v2_immutable_deployments.sql` (`V2-03.2`–`V2-03.4`).
    pub async fn persist_to_sqlite(&self, pool: &sqlx::SqlitePool) -> Result<(), DeploymentError> {
        let snapshot = {
            let g = self.inner.lock().expect("store lock");
            (
                g.head_revision,
                g.epoch,
                g.artifacts.values().cloned().collect::<Vec<_>>(),
                g.packages.values().cloned().collect::<Vec<_>>(),
                g.admissions.values().cloned().collect::<Vec<_>>(),
                g.connections.values().cloned().collect::<Vec<_>>(),
                g.revisions.values().cloned().collect::<Vec<_>>(),
                g.receipts
                    .iter()
                    .map(|((p, k), r)| (p.clone(), k.clone(), r.clone()))
                    .collect::<Vec<_>>(),
            )
        };

        let mut tx = pool.begin().await.map_err(|e| {
            DeploymentError::new("DB_TRANSACTION_ERROR", format!("failed to begin tx: {e}"))
        })?;

        sqlx::query(
            "INSERT INTO deployment_head (singleton_id, head_revision, epoch) VALUES (1, ?1, ?2) \
             ON CONFLICT(singleton_id) DO UPDATE SET head_revision = excluded.head_revision, epoch = excluded.epoch",
        )
        .bind(snapshot.0 as i64)
        .bind(snapshot.1 as i64)
        .execute(&mut *tx)
        .await
        .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;

        for art in &snapshot.2 {
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

        for pkg in &snapshot.3 {
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

        for adm in &snapshot.4 {
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

        for conn in &snapshot.5 {
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

        for rev in &snapshot.6 {
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

        for (principal, idem_key, receipt) in &snapshot.7 {
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
        Ok(())
    }

    /// Hydrate `deployment_head`, `deployment_revisions`, `provider_packages`,
    /// `provider_admissions`, and `connection_revisions` from SQLite into `self.inner` (`ARCH-02`).
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
            Ok(None) => return Ok(()),
            Err(e) if e.to_string().contains("no such table") => return Ok(()),
            Err(e) => return Err(DeploymentError::new("DB_READ_ERROR", e.to_string())),
        };

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

        let mut g = self.inner.lock().expect("store lock");
        g.head_revision = head_rev;
        g.epoch = epoch;

        for (rev, parent, digest, manifest_str, principal, reason, compacted) in rev_rows {
            if let Ok(manifest) = serde_json::from_str::<CanonicalManifest>(&manifest_str) {
                let rev_u64 = rev.max(0) as u64;
                g.revisions.insert(
                    rev_u64,
                    DeploymentRevisionRow {
                        revision: rev_u64,
                        parent_revision: parent.map(|p| p.max(0) as u64),
                        manifest_digest: digest,
                        manifest_bytes: manifest_str.into_bytes(),
                        manifest,
                        reason,
                        created_by: principal,
                        members_compacted: compacted,
                    },
                );
            }
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
                _ => ImplementationKind::Rust,
            };
            let manifest_json = serde_json::from_str(&manifest_str).unwrap_or(Value::Null);
            let inventory_json = serde_json::from_str(&inventory_str).unwrap_or(Value::Null);
            let distribution_json = json!({
                "publisher": publisher,
                "tier": tier,
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
            let public_config = serde_json::from_str(&cfg_str).unwrap_or(Value::Null);
            let configuration_digest = sha256_digest(cfg_str.as_bytes());
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

        Ok(())
    }
}
