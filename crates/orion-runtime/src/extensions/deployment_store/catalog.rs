//! The catalog: what a node reads from the store on its serving path,
//! cached, and refreshed incrementally from the database.
//!
//! `connector_call`, the build and the provider registry read the head, the
//! committed revisions, and the packages, admissions and connection
//! revisions they name — some on every request — through synchronous
//! accessors. Those rows are small and, but for an admission's `revoked`
//! flag and a revision's `members_compacted`, immutable by key, so the
//! catalog holds them (each behind an `Arc`, so a read clones a pointer) and
//! a refresh adds only what it does not hold yet: the revisions above the
//! newest it has, the packages and connection revisions whose keys are new,
//! and the admissions and compaction flags whole (a column or two per row).
//!
//! It never holds artifact bytes: a definition, a package file or a
//! connection configuration is read from the database by digest when it is
//! needed. And it decides nothing: every mutation reads what it judges from
//! the database inside its own transaction ([`super::writes`]), then
//! refreshes the catalog.
//!
//! A refresh runs on a node's own mutations and when the reconciler sees the
//! persisted head or epoch move. A peer's upload that moves neither (a
//! package, an admission, a connection revision) reaches this node's catalog
//! on its next refresh; what this node commits is judged against the
//! database, never against the catalog.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use super::error::DeploymentErrorKind as Kind;
use super::{
    ConnectionRevisionRow, DeploymentError, DeploymentRevisionRow, ImmutableDeploymentStore,
    ProviderAdmissionRow, ProviderPackageRow, rules, sql,
};
use crate::extensions::Digest;
use crate::extensions::schema_profile::CompiledSchemaBundle;
use crate::storage::{DbPool, DbTransaction};

/// A package's schema bundle as the catalog holds it.
pub(super) type BundleEntry = Result<Arc<CompiledSchemaBundle>, DeploymentError>;

#[derive(Default)]
pub(super) struct Catalog {
    /// `(head_revision, epoch)` as last read.
    pub head: (u64, u64),
    /// Every revision at or below this is in `revisions`.
    pub revisions_through: u64,
    pub revisions: BTreeMap<u64, Arc<DeploymentRevisionRow>>,
    pub packages: BTreeMap<Digest, Arc<ProviderPackageRow>>,
    pub admissions: BTreeMap<Digest, Arc<ProviderAdmissionRow>>,
    /// By `(connection_id, revision)` ([`ConnectionRevisionRow::key`]), so
    /// one connection's revisions are a range.
    pub connections: BTreeMap<(String, u64), Arc<ConnectionRevisionRow>>,
    /// Each package's schemas, compiled once when the package enters the
    /// catalog: a package is immutable by digest, so its bundle never goes
    /// stale. A bundle that failed (a schema file no longer stored) is
    /// retried on the next refresh.
    pub bundles: HashMap<Digest, BundleEntry>,
}

impl Catalog {
    pub fn insert_package(&mut self, row: ProviderPackageRow, bundle: BundleEntry) {
        self.bundles.insert(row.package_digest.clone(), bundle);
        self.packages
            .insert(row.package_digest.clone(), Arc::new(row));
    }

    pub fn insert_connection(&mut self, row: ConnectionRevisionRow) {
        self.connections.insert(row.key(), Arc::new(row));
    }

    pub fn insert_admission(&mut self, row: ProviderAdmissionRow) {
        self.admissions
            .insert(row.package_digest.clone(), Arc::new(row));
    }
}

/// What one refresh read, applied to the catalog in one write.
struct Delta {
    head: (u64, u64),
    revisions: Vec<DeploymentRevisionRow>,
    compacted: Vec<u64>,
    admissions: Vec<ProviderAdmissionRow>,
    packages: Vec<(ProviderPackageRow, BundleEntry)>,
    connections: Vec<ConnectionRevisionRow>,
}

/// The packages `digests` names as stored, each with its full
/// `distribution.json` when that file is still held, and — with
/// `with_schemas` — the bytes of its schema files by digest.
pub(super) async fn read_packages(
    tx: &mut DbTransaction,
    digests: &[String],
    with_schemas: bool,
) -> Result<Vec<(ProviderPackageRow, HashMap<String, Vec<u8>>)>, DeploymentError> {
    if digests.is_empty() {
        return Ok(Vec::new());
    }
    let mut rows = Vec::new();
    for record in sql::packages(tx, digests).await? {
        rows.push(record.into_row(None)?);
    }
    // One read for every file needed, across all the packages.
    let mut wanted: BTreeSet<String> = BTreeSet::new();
    for row in &rows {
        for (path, digest) in rules::inventory_files(&row.inventory_json) {
            if path == "distribution.json" || (with_schemas && rules::is_schema_file(&path)) {
                wanted.insert(digest);
            }
        }
    }
    let wanted: Vec<String> = wanted.into_iter().collect();
    let mut files: HashMap<String, Vec<u8>> = HashMap::new();
    for record in sql::artifacts(tx, &wanted).await? {
        let artifact = record.into_row()?;
        if let Some(bytes) = artifact.bytes {
            files.insert(artifact.digest, bytes);
        }
    }
    let mut out = Vec::with_capacity(rows.len());
    for mut row in rows {
        let listed = rules::inventory_files(&row.inventory_json);
        if let Some(distribution) = listed
            .iter()
            .find(|(path, _)| path == "distribution.json")
            .and_then(|(_, digest)| files.get(digest))
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(bytes).ok())
            .filter(serde_json::Value::is_object)
        {
            row.distribution_json = distribution;
        }
        let own: HashMap<String, Vec<u8>> = listed
            .iter()
            .filter(|(path, _)| with_schemas && rules::is_schema_file(path))
            .filter_map(|(_, digest)| files.get(digest).map(|b| (digest.clone(), b.clone())))
            .collect();
        out.push((row, own));
    }
    Ok(out)
}

fn tx_error(e: sqlx::Error) -> DeploymentError {
    DeploymentError::new(Kind::DbTransaction, format!("failed to begin tx: {e}"))
}

impl ImmutableDeploymentStore {
    /// The persisted `(head_revision, epoch)`, one row: what a node polls to
    /// learn whether anything it serves from has changed. `None` before the
    /// first commit, or before the table exists.
    pub async fn persisted_head(pool: &DbPool) -> Result<Option<(u64, u64)>, DeploymentError> {
        match sql::read_head(pool).await {
            Ok(head) => Ok(head),
            Err(e) if crate::storage::is_undefined_table(&e) => Ok(None),
            Err(e) => Err(DeploymentError::read(e)),
        }
    }

    /// Refresh the catalog when the database's `(head_revision, epoch)` has
    /// moved past what it holds: one row read when it is already current.
    pub async fn catch_up(&self, pool: &DbPool) -> Result<(), DeploymentError> {
        if Self::persisted_head(pool).await?.unwrap_or((0, 0)) != self.current_head() {
            self.load_from_db_pool(pool).await?;
        }
        Ok(())
    }

    /// Bring the catalog up to what the database holds (see the module
    /// documentation): the revisions above the newest one held, new packages
    /// and connection revisions, and every admission. A store whose tables
    /// do not exist yet reads as empty.
    ///
    /// # Errors
    ///
    /// `CORRUPT_PERSISTED_STATE` for a stored row the store cannot read back
    /// (a revision whose manifest does not verify, a head naming a revision
    /// that is not stored); the catalog is left as it was.
    pub async fn load_from_db_pool(&self, pool: &DbPool) -> Result<(), DeploymentError> {
        let _guard = self.write_lock.lock().await;
        self.refresh_locked(pool).await
    }

    /// Make sure the catalog holds what `manifest` names — each provider's
    /// package and admission, each connection member's revision — reading
    /// the database only when something is missing: a package, an admission
    /// or a connection a peer stored moves no head, so a node learns of it
    /// here, when a build names it.
    pub async fn load_for_manifest(
        &self,
        pool: &DbPool,
        manifest: &super::CanonicalManifest,
    ) -> Result<(), DeploymentError> {
        let missing = {
            let c = self.catalog.read().expect("catalog lock");
            manifest.providers.iter().any(|p| {
                !c.packages.contains_key(&p.digest) || !c.admissions.contains_key(&p.digest)
            }) || manifest.members.iter().any(|m| {
                m.entity.kind == super::EntityKind::Connection
                    && !c
                        .connections
                        .contains_key(&(m.entity.id.clone(), m.entity.revision.get()))
            })
        };
        if missing {
            self.load_from_db_pool(pool).await?;
        }
        Ok(())
    }

    /// [`Self::load_from_db_pool`], for a caller already holding the write
    /// lock.
    pub(super) async fn refresh_locked(&self, pool: &DbPool) -> Result<(), DeploymentError> {
        let (through, known_packages, known_connections) = {
            let c = self.catalog.read().expect("catalog lock");
            let packages: BTreeSet<String> = c
                .packages
                .keys()
                .filter(|d| c.bundles.get(*d).is_some_and(Result::is_ok))
                .map(|d| d.to_string())
                .collect();
            let connections: BTreeSet<(String, u64)> = c.connections.keys().cloned().collect();
            (c.revisions_through, packages, connections)
        };

        let mut tx = pool.begin_snapshot_tx().await.map_err(tx_error)?;
        let head = match sql::head(&mut tx).await {
            Ok(head) => head.unwrap_or((0, 0)),
            Err(e) if crate::storage::is_undefined_table(&e) => return Ok(()),
            Err(e) => return Err(DeploymentError::read(e)),
        };
        let revisions = sql::revisions_after(&mut tx, through)
            .await?
            .into_iter()
            .map(sql::RevisionRecord::into_row)
            .collect::<Result<Vec<_>, _>>()?;
        let compacted = sql::compacted_through(&mut tx, through).await?;
        let admissions: Vec<ProviderAdmissionRow> = sql::all_admissions(&mut tx)
            .await?
            .into_iter()
            .map(ProviderAdmissionRow::try_from)
            .collect::<Result<_, _>>()?;

        let new_packages: Vec<String> = sql::package_digests(&mut tx)
            .await?
            .into_iter()
            .filter(|d| !known_packages.contains(d))
            .collect();
        let packages = read_packages(&mut tx, &new_packages, true)
            .await?
            .into_iter()
            .map(|(row, files)| {
                let bundle = rules::compile_bundle(&row, &files).map(Arc::new);
                (row, bundle)
            })
            .collect();

        let new_keys: BTreeSet<(String, u64)> = sql::connection_keys(&mut tx)
            .await?
            .into_iter()
            .filter(|k| !known_connections.contains(k))
            .collect();
        let ids: Vec<String> = new_keys
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut connections = Vec::new();
        for record in sql::connections_of(&mut tx, &ids).await? {
            let row = record.into_row()?;
            if new_keys.contains(&row.key()) {
                connections.push(row);
            }
        }
        tx.commit().await.map_err(DeploymentError::read)?;

        self.apply_delta(Delta {
            head,
            revisions,
            compacted,
            admissions,
            packages,
            connections,
        })
    }

    fn apply_delta(&self, delta: Delta) -> Result<(), DeploymentError> {
        let mut c = self.catalog.write().expect("catalog lock");
        let (head_revision, _) = delta.head;
        if head_revision > 0
            && !c.revisions.contains_key(&head_revision)
            && !delta.revisions.iter().any(|r| r.revision == head_revision)
        {
            return Err(DeploymentError::corrupt(format!(
                "deployment_head references revision {head_revision}, which is missing from deployment_revisions"
            )));
        }
        c.head = delta.head;
        for row in delta.revisions {
            c.revisions_through = c.revisions_through.max(row.revision);
            c.revisions.insert(row.revision, Arc::new(row));
        }
        c.revisions_through = c.revisions_through.max(head_revision);
        // Compaction is one-way.
        for revision in delta.compacted {
            if let Some(row) = c.revisions.get_mut(&revision)
                && !row.members_compacted
            {
                Arc::make_mut(row).members_compacted = true;
            }
        }
        c.admissions = delta
            .admissions
            .into_iter()
            .map(|a| (a.package_digest.clone(), Arc::new(a)))
            .collect();
        for (row, bundle) in delta.packages {
            c.insert_package(row, bundle);
        }
        for row in delta.connections {
            c.insert_connection(row);
        }
        Ok(())
    }
}
