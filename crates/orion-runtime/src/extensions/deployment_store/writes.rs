//! Every mutation of the store, each one database transaction that reads
//! what it judges with targeted SELECTs, decides through [`super::rules`],
//! writes, and commits; then the catalog is refreshed.
//!
//! The transactions that move the head or the epoch (apply, rollback,
//! revocation, collection) take the head row first ([`sql::lock_head`]: `FOR
//! UPDATE` on PostgreSQL and MySQL, SQLite's `BEGIN IMMEDIATE` write lock),
//! so they queue there and each reads what the one before it committed; the
//! head moves only through the compare-and-swap in [`sql::write_head`].
//! A node's own mutations also hold the store's write lock, which keeps
//! their catalog refreshes in commit order. Planning takes neither: it reads
//! in one read-only snapshot.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value;

use super::error::DeploymentErrorKind as Kind;
use super::rules::{self, ClosureFacts, GcFacts};
use super::{
    Apply, ArchiveEntryInput, ArtifactObjectRow, ArtifactState, CandidatePlan, CanonicalManifest,
    ConnectionRevisionRow, DeploymentError, DeploymentReceipt, DeploymentRevisionRow, EntityKind,
    GcOutcome, ImmutableDeploymentStore, NewConnectionRevision, ProviderAdmissionRow,
    ProviderPackageRow, Rollback, catalog, extract_inventory_file_digests, sql,
};
use crate::extensions::Admission;
use crate::extensions::package_identity::sha256_digest;
use crate::storage::repositories::audit_logs::AuditEvent;
use crate::storage::{DbPool, DbTransaction};

async fn begin_write(pool: &DbPool) -> Result<DbTransaction, DeploymentError> {
    pool.begin_write_tx()
        .await
        .map_err(|e| DeploymentError::new(Kind::DbTransaction, format!("failed to begin tx: {e}")))
}

async fn commit(tx: DbTransaction) -> Result<(), DeploymentError> {
    tx.commit()
        .await
        .map_err(|e| DeploymentError::new(Kind::DbCommit, format!("failed to commit tx: {e}")))
}

/// What the store holds that `manifest` names, read in `tx`.
async fn closure_facts(
    tx: &mut DbTransaction,
    manifest: &CanonicalManifest,
) -> Result<ClosureFacts, DeploymentError> {
    let mut facts = ClosureFacts::default();
    let packages: Vec<String> = manifest
        .providers
        .iter()
        .map(|p| p.digest.to_string())
        .chain(
            manifest
                .members
                .iter()
                .filter_map(|m| m.owning_package.clone()),
        )
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    for adm in sql::admissions(tx, &packages).await? {
        let adm = ProviderAdmissionRow::try_from(adm)?;
        facts.admissions.insert(adm.package_digest.clone(), adm);
    }
    for record in sql::packages(tx, &packages).await? {
        let row = record.into_row(None)?;
        facts.packages.insert(row.package_digest.clone(), row);
    }
    for record in sql::artifact_meta(tx, &ClosureFacts::digests_named_by(manifest)).await? {
        let state = record.state()?;
        facts.artifacts.insert(record.digest, state);
    }
    let named: BTreeSet<(String, u64)> = manifest
        .members
        .iter()
        .filter(|m| m.entity.kind == EntityKind::Connection)
        .map(|m| (m.entity.id.clone(), m.entity.revision.get()))
        .collect();
    let ids: Vec<String> = named
        .iter()
        .map(|(id, _)| id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    for record in sql::connections_of(tx, &ids).await? {
        let row = record.into_row()?;
        let key = row.key();
        if named.contains(&key) {
            facts.connections.insert(key, row);
        }
    }
    Ok(facts)
}

/// The manifest of the committed revision `revision`, for a plan's base.
async fn base_manifest(
    tx: &mut DbTransaction,
    revision: u64,
) -> Result<Option<CanonicalManifest>, DeploymentError> {
    if revision == 0 {
        return Ok(None);
    }
    match sql::revision(tx, revision).await? {
        Some(record) => Ok(Some(record.into_row()?.manifest)),
        None => Err(DeploymentError::new(
            Kind::BaseRevisionNotFound,
            format!("base revision {revision} does not exist"),
        )),
    }
}

/// [`rules::plan`] of `candidate` on `base_revision`, read in `tx`.
async fn plan_in(
    tx: &mut DbTransaction,
    base_revision: u64,
    candidate: CanonicalManifest,
) -> Result<CandidatePlan, DeploymentError> {
    let canonical = candidate.canonicalize_and_verify()?;
    let base = base_manifest(tx, base_revision).await?;
    let facts = closure_facts(tx, &candidate).await?;
    rules::plan(&facts, base_revision, base.as_ref(), candidate, canonical)
}

/// What one head-moving commit wrote: the receipt, and — for a commit
/// rather than a replay — the revision row and the head move.
struct Committed {
    receipt: DeploymentReceipt,
    revision: Option<HeadMove>,
}

/// A committed revision, and the head it moved from and to.
struct HeadMove {
    row: DeploymentRevisionRow,
    from: Option<(u64, u64)>,
    to: (u64, u64),
}

impl ImmutableDeploymentStore {
    /// Refresh the catalog after a commit. The commit stands whatever this
    /// answers: a refresh that fails leaves the catalog behind until the
    /// reconciler's next poll, which sees the head or epoch moved.
    async fn refresh_after_commit(&self, pool: &DbPool) {
        if let Err(e) = self.refresh_locked(pool).await {
            tracing::warn!(
                error = %e,
                "deployment store: committed, but the catalog was not refreshed; \
                 the reconciler re-reads it on its next poll"
            );
        }
    }

    /// Whether the persisted head or epoch is not the one the catalog holds:
    /// one row read.
    async fn head_moved(&self, pool: &DbPool) -> bool {
        match Self::persisted_head(pool).await {
            Ok(persisted) => persisted.unwrap_or((0, 0)) != self.current_head(),
            Err(e) => {
                tracing::warn!(error = %e, "deployment store: the head could not be read");
                false
            }
        }
    }

    /// After a commit that moved neither the head nor the epoch: refresh the
    /// catalog only when it is behind the persisted head, so a node lagging
    /// its peers catches up on its own next write. The caller holds the
    /// write lock.
    async fn catch_up_locked(&self, pool: &DbPool) {
        if self.head_moved(pool).await {
            self.refresh_after_commit(pool).await;
        }
    }

    /// After a commit that moved the head from `observed` to `next`: move
    /// the catalog's head with it when the catalog was at `observed` — when
    /// no committed revision is missing between them — and otherwise, the
    /// catalog being behind a peer's commits, refresh it.
    ///
    /// A commit returns as soon as its own rows are in the catalog: what the
    /// committing caller builds next is its own revision, and a peer's
    /// uploads a build names are read then
    /// ([`Self::load_for_manifest`]).
    async fn advance_head(&self, pool: &DbPool, observed: Option<(u64, u64)>, next: (u64, u64)) {
        let advanced = {
            let mut c = self.catalog.write().expect("catalog lock");
            let observed = observed.unwrap_or((0, 0));
            let at_observed = c.head == observed && c.revisions_through >= observed.0;
            if at_observed {
                c.head = next;
                c.revisions_through = c.revisions_through.max(next.0);
            }
            at_observed
        };
        if !advanced {
            self.refresh_after_commit(pool).await;
        }
    }

    /// V2-04.1: ingest a bounded archive, refusing symlinks, path traversal,
    /// decompression ratio bombs (> 20x), duplicate entries and inventory
    /// mismatches. Upload creates no serving routes or active generations.
    pub async fn ingest_verified_archive_in_db(
        &self,
        pool: &DbPool,
        entries: &[ArchiveEntryInput],
    ) -> Result<ProviderPackageRow, DeploymentError> {
        let prepared = rules::prepare_archive(entries)?;
        let row = &prepared.row;
        let _guard = self.write_lock.lock().await;
        let mut tx = begin_write(pool).await?;
        if sql::other_package_with_identity(
            &mut tx,
            (&row.publisher, &row.provider_id, &row.version),
            &row.package_digest,
        )
        .await?
        .is_some()
        {
            return Err(DeploymentError::new(
                Kind::PublisherVersionConflict,
                "publisher/provider_id/version is already bound to a different digest",
            ));
        }
        // The digest is over the inventory, which hashes every file, so a
        // package stored under it holds these files; what is compared is
        // what the row records about them.
        if let Some(existing) = sql::packages(&mut tx, &[row.package_digest.to_string()])
            .await?
            .into_iter()
            .next()
        {
            let existing = existing.into_row(None)?;
            if existing.publisher != row.publisher
                || existing.provider_id != row.provider_id
                || existing.version != row.version
                || existing.implementation != row.implementation
                || existing.manifest_json != row.manifest_json
                || existing.inventory_json != row.inventory_json
            {
                return Err(DeploymentError::new(
                    Kind::ImmutableContentConflict,
                    "immutable package digest already exists with different metadata",
                ));
            }
        }
        for (digest, bytes) in &prepared.payloads {
            sql::put_artifact(&mut tx, digest, bytes).await?;
        }
        sql::put_artifact(&mut tx, &row.package_digest, &prepared.inventory_bytes).await?;
        sql::insert_package(&mut tx, row).await?;
        commit(tx).await?;

        self.catalog
            .write()
            .expect("catalog lock")
            .insert_package(prepared.row.clone(), Ok(Arc::new(prepared.bundle)));
        self.catch_up_locked(pool).await;
        Ok(prepared.row)
    }

    /// V2-04.2: admit and install a stored package under `policy`, after
    /// checking `lock` against every stored package, recording its admission
    /// fingerprint over runtime, profile, package and policy.
    pub async fn admit_package_in_db(
        &self,
        pool: &DbPool,
        package_digest: &str,
        policy: &Admission,
        lock: Option<&Value>,
    ) -> Result<ProviderAdmissionRow, DeploymentError> {
        let _guard = self.write_lock.lock().await;
        let mut tx = begin_write(pool).await?;
        let digest = [package_digest.to_string()];
        let Some((pkg, _)) = catalog::read_packages(&mut tx, &digest, false)
            .await?
            .into_iter()
            .next()
        else {
            return Err(DeploymentError::new(
                Kind::PackageNotFound,
                format!("package '{package_digest}' is not in store"),
            ));
        };
        let state = match sql::artifact_meta(&mut tx, &digest).await?.first() {
            Some(record) => record.state()?,
            // The package row holds its inventory.
            None => ArtifactState::Active,
        };
        if state != ArtifactState::Active {
            return Err(DeploymentError::new(
                Kind::ArtifactNotActive,
                format!("package '{package_digest}' is not ACTIVE"),
            ));
        }
        let all = match lock {
            Some(_) => sql::all_packages(&mut tx)
                .await?
                .into_iter()
                .map(|r| r.into_row(None))
                .collect::<Result<Vec<_>, _>>()?,
            None => Vec::new(),
        };
        let admission = rules::admit(&pkg, policy, lock.map(|l| (l, all.as_slice())))?;
        sql::upsert_admission(&mut tx, &admission).await?;
        commit(tx).await?;

        self.catalog
            .write()
            .expect("catalog lock")
            .insert_admission(admission.clone());
        self.catch_up_locked(pool).await;
        Ok(admission)
    }

    /// Revoke a package's admission. The epoch moves, so a peer polling the
    /// head sees it and re-reads the store, as it does for a new revision.
    pub async fn revoke_package_in_db(
        &self,
        pool: &DbPool,
        package_digest: &str,
    ) -> Result<(), DeploymentError> {
        let _guard = self.write_lock.lock().await;
        let mut tx = begin_write(pool).await?;
        let observed = sql::lock_head(&mut tx).await?;
        let Some(admission) = sql::admissions(&mut tx, &[package_digest.to_string()])
            .await?
            .into_iter()
            .next()
        else {
            return Err(DeploymentError::new(
                Kind::AdmissionNotFound,
                package_digest,
            ));
        };
        sql::revoke(&mut tx, package_digest).await?;
        let (head, epoch) = observed.unwrap_or((0, 0));
        let next = (head, epoch + 1);
        sql::write_head(&mut tx, observed, next).await?;
        commit(tx).await?;

        let mut revoked = ProviderAdmissionRow::try_from(admission)?;
        revoked.revoked = true;
        self.catalog
            .write()
            .expect("catalog lock")
            .insert_admission(revoked);
        self.advance_head(pool, observed, next).await;
        Ok(())
    }

    /// Store an immutable definition (a workflow, channel or plugin
    /// document) as an `ACTIVE` content-addressed artifact (`TD-19`).
    /// Storing bytes already stored is a no-op; storing bytes the collector
    /// drained or deleted revives them, with a fresh grace window.
    pub async fn store_definition_artifact_in_db(
        &self,
        pool: &DbPool,
        bytes: Vec<u8>,
    ) -> Result<ArtifactObjectRow, DeploymentError> {
        let digest = sha256_digest(&bytes).into_string();
        let mut tx = begin_write(pool).await?;
        sql::put_artifact(&mut tx, &digest, &bytes).await?;
        commit(tx).await?;
        // Nothing a definition is lives in the catalog, but an upload is
        // what precedes a plan: a caller reading the head next reads the
        // committed one.
        if self.head_moved(pool).await {
            let _guard = self.write_lock.lock().await;
            self.refresh_after_commit(pool).await;
        }
        Ok(ArtifactObjectRow {
            digest,
            size_bytes: bytes.len(),
            state: ArtifactState::Active,
            drain_epoch: None,
            bytes: Some(bytes),
        })
    }

    /// V2-03.2: store an immutable connection revision with its secret
    /// locators and its public configuration digest (`TD-20`).
    pub async fn store_connection_revision_in_db(
        &self,
        pool: &DbPool,
        connection_id: &str,
        revision: u64,
        provider_digest: &str,
        public_config: Value,
        secret_locators: BTreeMap<String, String>,
    ) -> Result<ConnectionRevisionRow, DeploymentError> {
        self.store_connection_revision_with_bindings_in_db(
            pool,
            NewConnectionRevision {
                connection_id,
                revision,
                provider_digest,
                public_config,
                secret_locators,
                bindings: BTreeMap::new(),
            },
        )
        .await
    }

    /// [`Self::store_connection_revision_in_db`], with the dependency
    /// `bindings` of a connection over a `workflow` provider package.
    pub async fn store_connection_revision_with_bindings_in_db(
        &self,
        pool: &DbPool,
        new: NewConnectionRevision<'_>,
    ) -> Result<ConnectionRevisionRow, DeploymentError> {
        let NewConnectionRevision {
            connection_id,
            revision,
            provider_digest,
            public_config,
            secret_locators,
            bindings,
        } = new;
        let prepared = rules::prepare_connection(
            connection_id,
            revision,
            provider_digest,
            public_config,
            secret_locators,
            bindings,
        )?;
        let row = prepared.row;
        let conflict = || {
            DeploymentError::new(
                Kind::ImmutableConnectionConflict,
                "immutable connection revision cannot be modified",
            )
        };
        let _guard = self.write_lock.lock().await;
        let mut tx = begin_write(pool).await?;
        if let Some(existing) = sql::connection(&mut tx, connection_id, revision).await? {
            let existing = existing.into_row()?;
            if existing != row {
                return Err(conflict());
            }
            drop(tx);
            self.catalog
                .write()
                .expect("catalog lock")
                .insert_connection(existing.clone());
            return Ok(existing);
        }
        sql::put_artifact(&mut tx, &row.configuration_digest, &prepared.config_bytes).await?;
        if !sql::insert_connection(&mut tx, &row).await? {
            // A writer that got in between stored this revision first.
            let stored = sql::connection(&mut tx, connection_id, revision)
                .await?
                .map(sql::ConnectionRecord::into_row)
                .transpose()?;
            if stored.as_ref() != Some(&row) {
                return Err(conflict());
            }
        }
        commit(tx).await?;

        self.catalog
            .write()
            .expect("catalog lock")
            .insert_connection(row.clone());
        self.catch_up_locked(pool).await;
        Ok(row)
    }

    /// V2-04.3: plan `candidate_manifest` on `base_revision` — its closure
    /// over what is stored and its diff against the base — in one read-only
    /// snapshot, outside any write transaction.
    pub async fn plan_candidate_in_db(
        &self,
        pool: &DbPool,
        base_revision: u64,
        candidate_manifest: CanonicalManifest,
    ) -> Result<CandidatePlan, DeploymentError> {
        let mut tx = pool.begin_snapshot_tx().await.map_err(|e| {
            DeploymentError::new(Kind::DbTransaction, format!("failed to begin tx: {e}"))
        })?;
        let plan = plan_in(&mut tx, base_revision, candidate_manifest).await?;
        tx.commit().await.map_err(DeploymentError::read)?;
        Ok(plan)
    }

    /// V2-04.4: commit `plan` as the next revision under compare-and-swap on
    /// its base, idempotently on `(principal, idempotency_key)`.
    pub async fn apply_candidate_in_db(
        &self,
        pool: &DbPool,
        apply: &Apply<'_>,
        policy: &Admission,
    ) -> Result<DeploymentReceipt, DeploymentError> {
        self.apply_candidate_audited_in_db(pool, apply, policy, |_| None::<AuditEvent>)
            .await
            .map(|(receipt, _)| receipt)
    }

    /// [`Self::apply_candidate_in_db`], with the audit row `audit` builds
    /// from the receipt written in the commit's transaction: it lands with
    /// the head move or not at all. `None` beside the receipt when nothing
    /// was committed (an idempotent replay of a committed apply).
    pub async fn apply_candidate_audited_in_db<A, E>(
        &self,
        pool: &DbPool,
        apply: &Apply<'_>,
        policy: &Admission,
        audit: A,
    ) -> Result<(DeploymentReceipt, Option<AuditEvent>), DeploymentError>
    where
        A: FnOnce(&DeploymentReceipt) -> E,
        E: Into<Option<AuditEvent>>,
    {
        let _guard = self.write_lock.lock().await;
        let mut tx = begin_write(pool).await?;
        let observed = sql::lock_head(&mut tx).await?;
        let committed = self.commit_plan(&mut tx, observed, apply, policy).await?;
        self.finish_commit(pool, tx, committed, |r| audit(r).into())
            .await
    }

    /// V2-04.5: re-deploy `target_revision`'s manifest as a NEW revision on
    /// top of `expected_head_revision`, re-verifying admissions, revocations
    /// and what is stored as of now.
    pub async fn rollback_to_revision_in_db(
        &self,
        pool: &DbPool,
        rollback: &Rollback<'_>,
        policy: &Admission,
    ) -> Result<(DeploymentReceipt, Vec<String>), DeploymentError> {
        self.rollback_to_revision_audited_in_db(pool, rollback, policy, |_| None::<AuditEvent>)
            .await
            .map(|(out, _)| out)
    }

    /// [`Self::rollback_to_revision_in_db`], with its audit row written in
    /// the commit's transaction (see [`Self::apply_candidate_audited_in_db`]).
    pub async fn rollback_to_revision_audited_in_db<A, E>(
        &self,
        pool: &DbPool,
        rollback: &Rollback<'_>,
        policy: &Admission,
        audit: A,
    ) -> Result<((DeploymentReceipt, Vec<String>), Option<AuditEvent>), DeploymentError>
    where
        A: FnOnce(&DeploymentReceipt) -> E,
        E: Into<Option<AuditEvent>>,
    {
        let &Rollback {
            principal,
            idempotency_key,
            expected_head_revision,
            target_revision,
            reason,
        } = rollback;
        let _guard = self.write_lock.lock().await;
        let mut tx = begin_write(pool).await?;
        let observed = sql::lock_head(&mut tx).await?;
        let Some(target) = sql::revision(&mut tx, target_revision).await? else {
            return Err(DeploymentError::new(
                Kind::RollbackTargetNotFound,
                format!("target revision {target_revision} does not exist"),
            ));
        };
        let target = target.into_row()?;
        if target.members_compacted {
            return Err(DeploymentError::new(
                Kind::RollbackTargetCompacted,
                format!("target revision {target_revision} has been compacted"),
            ));
        }
        let mut plan = plan_in(&mut tx, expected_head_revision, target.manifest).await?;
        plan.external_effect_warnings.push(
            "Configuration rollback creates a new deployment revision; already-emitted external side effects are not reversed."
                .to_string(),
        );
        let warnings = plan.external_effect_warnings.clone();
        let committed = self
            .commit_plan(
                &mut tx,
                observed,
                &Apply {
                    principal,
                    idempotency_key,
                    plan: &plan,
                    reason,
                },
                policy,
            )
            .await?;
        let (receipt, event) = self
            .finish_commit(pool, tx, committed, |r| audit(r).into())
            .await?;
        Ok(((receipt, warnings), event))
    }

    /// The commit both an apply and a rollback make, in `tx` after the head
    /// was locked at `observed`: the stored receipt for a replay; otherwise
    /// the base checked against the head, the providers' admissions and the
    /// manifest's closure re-read and re-checked, then the head moved and
    /// the revision, its members and the receipt written.
    async fn commit_plan(
        &self,
        tx: &mut DbTransaction,
        observed: Option<(u64, u64)>,
        apply: &Apply<'_>,
        policy: &Admission,
    ) -> Result<Committed, DeploymentError> {
        let &Apply {
            principal,
            idempotency_key,
            plan,
            reason,
        } = apply;
        let request_hash = plan.compute_request_hash("apply", reason)?;
        if let Some(stored) = sql::receipt(tx, principal, idempotency_key).await? {
            if stored.request_hash == request_hash {
                return Ok(Committed {
                    receipt: stored.into_receipt(&self.node_id()),
                    revision: None,
                });
            }
            return Err(DeploymentError::new(
                Kind::IdempotencyKeyConflict,
                "idempotency_key already used by principal with different request parameters",
            ));
        }

        let (head, epoch) = observed.unwrap_or((0, 0));
        if head != plan.base_revision {
            return Err(DeploymentError::new(
                Kind::DeploymentHeadConflict,
                format!(
                    "expected base_revision {}, but current head_revision is {head}",
                    plan.base_revision
                ),
            ));
        }

        let facts = closure_facts(tx, &plan.candidate_manifest).await?;
        rules::recheck_admissions(&facts, &plan.candidate_manifest, policy)?;
        rules::verify_closure(
            &facts,
            &plan.candidate_manifest,
            Kind::ArtifactDrainingOrDeleted,
        )?;

        let next = (head + 1, epoch + 1);
        sql::write_head(tx, observed, next).await?;
        let mut revision = DeploymentRevisionRow {
            revision: next.0,
            parent_revision: (plan.base_revision > 0).then_some(plan.base_revision),
            manifest_digest: plan.candidate_digest.clone(),
            manifest_bytes: plan.candidate_bytes.clone(),
            manifest: plan.candidate_manifest.clone(),
            reason: reason.to_string(),
            created_by: principal.to_string(),
            members_compacted: false,
            created_at: None,
        };
        revision.created_at = Some(sql::insert_revision(tx, &revision).await?);
        let mut receipt = DeploymentReceipt {
            operation_id: format!("op-{}-{}", next.0, next.1),
            principal: principal.to_string(),
            idempotency_key: idempotency_key.to_string(),
            request_hash,
            committed_revision: next.0,
            manifest_digest: plan.candidate_digest.clone(),
            epoch: next.1,
            receiving_node_id: self.node_id(),
            created_at: None,
        };
        receipt.created_at = Some(sql::insert_receipt(tx, &receipt).await?);
        Ok(Committed {
            receipt,
            revision: Some(HeadMove {
                row: revision,
                from: observed,
                to: next,
            }),
        })
    }

    /// Write the audit row of a commit that moved the head, commit, and
    /// publish the new revision to the catalog. A replay commits nothing.
    async fn finish_commit(
        &self,
        pool: &DbPool,
        mut tx: DbTransaction,
        committed: Committed,
        audit: impl FnOnce(&DeploymentReceipt) -> Option<AuditEvent>,
    ) -> Result<(DeploymentReceipt, Option<AuditEvent>), DeploymentError> {
        let Some(HeadMove {
            row: revision,
            from: observed,
            to: next,
        }) = committed.revision
        else {
            return Ok((committed.receipt, None));
        };
        let event = audit(&committed.receipt);
        if let Some(event) = &event {
            crate::storage::repositories::audit_logs::insert_in_tx(&mut tx, event)
                .await
                .map_err(|e| DeploymentError::new(Kind::DbWrite, e.to_string()))?;
        }
        commit(tx).await?;

        self.catalog
            .write()
            .expect("catalog lock")
            .revisions
            .insert(revision.revision, Arc::new(revision));
        self.advance_head(pool, observed, next).await;
        Ok((committed.receipt, event))
    }

    /// One collection pass (`rules::plan_collection`) in one transaction:
    /// the grace window is read on the database clock in that transaction,
    /// and the pass writes its epoch move through the same head CAS an apply
    /// does, so it serialises against every other mutation of the store.
    /// Nothing runs before the first deployment: without a head there is
    /// nothing for the pass to serialise against.
    pub async fn mark_and_collect_garbage_in_db(
        &self,
        pool: &DbPool,
        retain_recent_revisions: usize,
        grace_secs: u64,
    ) -> Result<GcOutcome, DeploymentError> {
        let _guard = self.write_lock.lock().await;
        let mut tx = begin_write(pool).await?;
        let observed = sql::lock_head(&mut tx).await?;
        let Some((head, epoch)) = observed.filter(|(head, _)| *head > 0) else {
            return Ok(GcOutcome::default());
        };

        let flags = sql::revision_flags(&mut tx).await?;
        let held: Vec<u64> = self
            .held
            .lock()
            .expect("held revisions lock")
            .values()
            .flatten()
            .copied()
            .collect();
        let retained = rules::retained_revisions(head, &flags, retain_recent_revisions, held);
        let stored: Vec<u64> = flags
            .iter()
            .map(|(r, _)| *r)
            .filter(|r| retained.contains(r))
            .collect();
        let retained_manifests = sql::manifests(&mut tx, &stored)
            .await?
            .into_iter()
            .map(|(_, m)| m)
            .collect();
        let compactable = flags
            .iter()
            .filter(|(r, compacted)| !compacted && !retained.contains(r))
            .map(|(r, _)| *r)
            .collect();
        let mut packages = BTreeMap::new();
        for (digest, inventory) in sql::package_inventories(&mut tx).await? {
            let inventory: Value = serde_json::from_str(&inventory).map_err(|e| {
                DeploymentError::corrupt(format!(
                    "corrupt inventory_json for package '{digest}': {e}"
                ))
            })?;
            packages.insert(digest, extract_inventory_file_digests(&inventory));
        }
        let revoked: HashSet<String> = sql::all_admissions(&mut tx)
            .await?
            .into_iter()
            .filter(|a| a.revoked)
            .map(|a| a.package_digest)
            .collect();
        let configurations = sql::all_connections(&mut tx)
            .await?
            .into_iter()
            .map(|r| {
                r.into_row()
                    .map(|row| row.configuration_digest.into_string())
            })
            .collect::<Result<HashSet<_>, _>>()?;
        let artifacts = sql::live_artifacts(&mut tx)
            .await?
            .into_iter()
            .map(|r| {
                let state = r.state()?;
                Ok((
                    r.digest,
                    state,
                    r.drain_epoch.map(sql::to_u64),
                    sql::to_u64(r.size_bytes),
                ))
            })
            .collect::<Result<Vec<_>, DeploymentError>>()?;
        let recent = sql::recent_artifacts(&mut tx, grace_secs).await?;

        let out = rules::plan_collection(&GcFacts {
            epoch,
            retained: retained_manifests,
            compactable,
            packages,
            revoked,
            configurations,
            artifacts,
            recent,
        });
        if out.is_empty() {
            return Ok(out);
        }
        sql::compact(&mut tx, &out.compacted).await?;
        sql::move_artifacts(&mut tx, epoch, &out.draining, &out.deleted, &out.revived).await?;
        let next = (head, epoch + 1);
        sql::write_head(&mut tx, observed, next).await?;
        commit(tx).await?;

        {
            let mut c = self.catalog.write().expect("catalog lock");
            for revision in &out.compacted {
                if let Some(row) = c.revisions.get_mut(revision) {
                    Arc::make_mut(row).members_compacted = true;
                }
            }
        }
        self.advance_head(pool, observed, next).await;
        Ok(out)
    }

    /// The artifact `digest` as stored, with its bytes (`None` for a deleted
    /// one). Bytes that do not hash to their digest are refused.
    pub async fn fetch_artifact(
        &self,
        pool: &DbPool,
        digest: &str,
    ) -> Result<Option<ArtifactObjectRow>, DeploymentError> {
        Ok(self
            .fetch_artifacts(pool, &[digest.to_string()])
            .await?
            .remove(digest))
    }

    /// [`Self::fetch_artifact`] for several digests, in one read.
    pub async fn fetch_artifacts(
        &self,
        pool: &DbPool,
        digests: &[String],
    ) -> Result<HashMap<String, ArtifactObjectRow>, DeploymentError> {
        sql::artifacts_from_pool(pool, digests)
            .await?
            .into_iter()
            .map(|r| r.into_row().map(|row| (row.digest.clone(), row)))
            .collect()
    }

    /// The state of each artifact `digests` names that is stored, without
    /// its bytes, in one read.
    pub async fn artifact_states(
        &self,
        pool: &DbPool,
        digests: &[String],
    ) -> Result<HashMap<String, ArtifactState>, DeploymentError> {
        sql::artifact_meta_from_pool(pool, digests)
            .await?
            .into_iter()
            .map(|r| r.state().map(|state| (r.digest, state)))
            .collect()
    }

    /// The bytes of the files `paths` that `package_digest`'s inventory
    /// lists, by path, in one read. A path the inventory does not list, or
    /// whose bytes are no longer stored, is absent.
    pub async fn package_files(
        &self,
        pool: &DbPool,
        package_digest: &str,
        paths: &[&str],
    ) -> Result<BTreeMap<String, Vec<u8>>, DeploymentError> {
        let Some(package) = self.get_package(package_digest) else {
            return Ok(BTreeMap::new());
        };
        let wanted: Vec<(String, String)> = rules::inventory_files(&package.inventory_json)
            .into_iter()
            .filter(|(path, _)| paths.contains(&path.as_str()))
            .collect();
        let digests: Vec<String> = wanted.iter().map(|(_, d)| d.clone()).collect();
        let mut stored = self.fetch_artifacts(pool, &digests).await?;
        Ok(wanted
            .into_iter()
            .filter_map(|(path, digest)| {
                let bytes = stored.get_mut(&digest)?.bytes.clone()?;
                Some((path, bytes))
            })
            .collect())
    }

    /// The committed revision `revision`: from the catalog, or read from the
    /// database when the catalog has not caught up with it yet.
    pub async fn revision(
        &self,
        pool: &DbPool,
        revision: u64,
    ) -> Result<Option<Arc<DeploymentRevisionRow>>, DeploymentError> {
        if let Some(row) = self.get_revision(revision) {
            return Ok(Some(row));
        }
        Ok(sql::revision_from_pool(pool, revision)
            .await?
            .map(sql::RevisionRecord::into_row)
            .transpose()?
            .map(Arc::new))
    }
}
