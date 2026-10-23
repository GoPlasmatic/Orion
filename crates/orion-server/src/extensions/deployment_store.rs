//! V2-03 & V2-04 Immutable storage, canonical manifests, bounded archive
//! ingestion, candidate planning, idempotent CAS apply, rollback by new
//! revision, two-phase `ACTIVE -> DRAINING -> DELETED` artifact GC, and legacy
//! snapshot backfill.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::admission::{AdmissionPolicy, PackageAdmissionRequest};
use super::contract::{ImplementationKind, ProviderReference};
use super::migration::{EditionTier, map_legacy_connector};
use super::package_identity::{
    VerifiedArtifactMetadata, canonical_bytes, check_dependency_lock, sha256_digest,
    verify_inventory,
};
use super::schema_profile::{check_manifest_bundle, validate_safe_package_path};

// ============================================================
// V2-03.1: Typed Entity References, Rollouts & Canonical Manifests
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Workflow,
    Channel,
    Connection,
    Source,
    Model,
    Plugin,
}

impl EntityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Workflow => "workflow",
            Self::Channel => "channel",
            Self::Connection => "connection",
            Self::Source => "source",
            Self::Model => "model",
            Self::Plugin => "plugin",
        }
    }
}

/// Tagged integer-revision reference to an immutable deployment entity (`V2-03.1`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EntityRef {
    pub kind: EntityKind,
    pub id: String,
    pub revision: u64,
}

impl EntityRef {
    pub fn validate(&self) -> Result<(), DeploymentError> {
        if self.id.is_empty() || self.id.starts_with("sha256:") {
            return Err(DeploymentError::new(
                "ENTITY_REF_INVALID",
                "entity id must be non-empty and must not be a digest",
            ));
        }
        if self.revision == 0 {
            return Err(DeploymentError::new(
                "ENTITY_REF_INVALID",
                "entity integer revision must be >= 1",
            ));
        }
        Ok(())
    }
}

/// Semantic-version + content-digest reference to a provider package (`V2-03.1`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProviderRef {
    pub publisher: String,
    pub provider_id: String,
    pub version: String,
    pub digest: String,
}

impl ProviderRef {
    pub fn validate(&self) -> Result<(), DeploymentError> {
        if self.publisher.is_empty() || self.provider_id.is_empty() {
            return Err(DeploymentError::new(
                "PROVIDER_REF_INVALID",
                "publisher and provider_id are required",
            ));
        }
        // Reject integer revision confusion: version must contain '.' (SemVer) and not be a bare integer or digest.
        if !self.version.contains('.') || self.version.starts_with("sha256:") {
            return Err(DeploymentError::new(
                "PROVIDER_REF_INVALID",
                "provider version must be a semantic version string, not an integer revision or digest",
            ));
        }
        if !self.digest.starts_with("sha256:") || self.digest.len() != 71 {
            return Err(DeploymentError::new(
                "PROVIDER_REF_INVALID",
                "provider digest must be sha256:<64 hex chars>",
            ));
        }
        Ok(())
    }
}

/// Disjoint half-open rollout bucket interval `[start_bucket, end_bucket)` within `0..100` (`V2-03.1`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RolloutInterval {
    pub workflow_revision: u64,
    pub start_bucket: u8,
    pub end_bucket: u8,
}

/// Validate that a set of rollout intervals partitions `[0, 100)` with no gaps and no overlaps.
pub fn validate_rollout_partition(intervals: &[RolloutInterval]) -> Result<(), DeploymentError> {
    if intervals.is_empty() {
        return Err(DeploymentError::new(
            "ROLLOUT_PARTITION_INVALID",
            "at least one rollout interval is required",
        ));
    }
    let mut sorted = intervals.to_vec();
    sorted.sort_by_key(|i| (i.start_bucket, i.end_bucket));
    let mut seen_revisions = HashSet::new();
    let mut cursor = 0u8;
    for interval in &sorted {
        if interval.workflow_revision == 0 || !seen_revisions.insert(interval.workflow_revision) {
            return Err(DeploymentError::new(
                "ROLLOUT_PARTITION_INVALID",
                "duplicate or zero workflow revision in rollout",
            ));
        }
        if interval.start_bucket >= interval.end_bucket || interval.end_bucket > 100 {
            return Err(DeploymentError::new(
                "ROLLOUT_PARTITION_INVALID",
                "invalid bucket bounds",
            ));
        }
        if interval.start_bucket != cursor {
            return Err(DeploymentError::new(
                "ROLLOUT_PARTITION_INVALID",
                format!(
                    "rollout intervals have gap or overlap at bucket {cursor} vs {}",
                    interval.start_bucket
                ),
            ));
        }
        cursor = interval.end_bucket;
    }
    if cursor != 100 {
        return Err(DeploymentError::new(
            "ROLLOUT_PARTITION_INVALID",
            format!("rollout intervals end at {cursor}, expected 100"),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestMember {
    pub entity: EntityRef,
    pub definition_digest: String,
    pub owning_package: Option<String>,
    pub requires: Vec<EntityRef>,
    pub rollouts: Vec<RolloutInterval>,
}

/// Versioned canonical deployment manifest (`V2-03.1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalManifest {
    pub schema_version: u64,
    pub providers: Vec<ProviderRef>,
    pub provider_lock_digest: Option<String>,
    pub members: Vec<ManifestMember>,
}

impl CanonicalManifest {
    /// Validate ownership, singleton constraints, rollout partitions, and
    /// `requires` closure, returning canonical JSON bytes and `sha256:` digest.
    pub fn canonicalize_and_verify(&self) -> Result<(Vec<u8>, String), DeploymentError> {
        if self.schema_version != 2 {
            return Err(DeploymentError::new(
                "MANIFEST_VERSION_UNSUPPORTED",
                "deployment manifest schema_version must be 2",
            ));
        }

        let mut provider_keys = HashSet::new();
        let mut provider_digests = HashSet::new();
        for p in &self.providers {
            p.validate()?;
            if !provider_keys.insert((p.publisher.clone(), p.provider_id.clone(), p.version.clone()))
            {
                return Err(DeploymentError::new(
                    "MANIFEST_DUPLICATE_PROVIDER",
                    format!("duplicate provider {}/{}@{}", p.publisher, p.provider_id, p.version),
                ));
            }
            provider_digests.insert(p.digest.clone());
        }

        // Singleton entity check (except workflows under rollout, which are referenced via Channel rollouts)
        let mut member_set: HashMap<(EntityKind, String), u64> = HashMap::new();
        let mut all_entities: HashSet<EntityRef> = HashSet::new();
        let mut owner_by_id: HashMap<(EntityKind, String), Option<String>> = HashMap::new();

        for m in &self.members {
            m.entity.validate()?;
            if let Some(pkg) = &m.owning_package
                && !provider_digests.contains(pkg)
            {
                return Err(DeploymentError::new(
                    "MANIFEST_OWNER_MISSING",
                    format!("owning_package '{pkg}' is not in manifest providers"),
                ));
            }
            if let Some(prev_owner) = owner_by_id.get(&(m.entity.kind, m.entity.id.clone()))
                && prev_owner != &m.owning_package
            {
                return Err(DeploymentError::new(
                    "MANIFEST_OWNERSHIP_CONFLICT",
                    format!("conflicting ownership for {}:{}", m.entity.kind.as_str(), m.entity.id),
                ));
            }
            owner_by_id.insert(
                (m.entity.kind, m.entity.id.clone()),
                m.owning_package.clone(),
            );

            if m.entity.kind != EntityKind::Workflow {
                if member_set
                    .insert((m.entity.kind, m.entity.id.clone()), m.entity.revision)
                    .is_some()
                {
                    return Err(DeploymentError::new(
                        "MANIFEST_DUPLICATE_SINGLETON",
                        format!(
                            "multiple revisions of singleton {}:{}",
                            m.entity.kind.as_str(),
                            m.entity.id
                        ),
                    ));
                }
            } else if !all_entities.insert(m.entity.clone()) {
                return Err(DeploymentError::new(
                    "MANIFEST_DUPLICATE_MEMBER",
                    format!("duplicate workflow revision {}:{}", m.entity.id, m.entity.revision),
                ));
            }
            all_entities.insert(m.entity.clone());

            if m.entity.kind == EntityKind::Channel && !m.rollouts.is_empty() {
                validate_rollout_partition(&m.rollouts)?;
            }
        }

        // Verify all `requires` edges exist in `all_entities`
        for m in &self.members {
            for req in &m.requires {
                if !all_entities.contains(req) {
                    return Err(DeploymentError::new(
                        "MANIFEST_MISSING_REQUIREMENT",
                        format!(
                            "{}:{} requires missing {}:{}:v{}",
                            m.entity.kind.as_str(),
                            m.entity.id,
                            req.kind.as_str(),
                            req.id,
                            req.revision
                        ),
                    ));
                }
            }
        }

        // Canonical projection sorting
        let mut sorted_providers = self.providers.clone();
        sorted_providers.sort();
        let mut sorted_members = self.members.clone();
        sorted_members.sort_by(|a, b| a.entity.cmp(&b.entity));

        let providers_json: Vec<Value> = sorted_providers
            .iter()
            .map(|p| {
                json!({
                    "digest": p.digest,
                    "provider_id": p.provider_id,
                    "publisher": p.publisher,
                    "version": p.version,
                })
            })
            .collect();

        let members_json: Vec<Value> = sorted_members
            .iter()
            .map(|m| {
                let mut requires = m.requires.clone();
                requires.sort();
                let req_json: Vec<Value> = requires
                    .iter()
                    .map(|r| {
                        json!({
                            "id": r.id,
                            "kind": r.kind.as_str(),
                            "revision": r.revision,
                        })
                    })
                    .collect();
                let mut rollouts = m.rollouts.clone();
                rollouts.sort();
                let roll_json: Vec<Value> = rollouts
                    .iter()
                    .map(|ro| {
                        json!({
                            "end_bucket": ro.end_bucket,
                            "start_bucket": ro.start_bucket,
                            "workflow_revision": ro.workflow_revision,
                        })
                    })
                    .collect();
                let mut obj = serde_json::Map::new();
                obj.insert("definition_digest".into(), json!(m.definition_digest));
                obj.insert(
                    "entity".into(),
                    json!({
                        "id": m.entity.id,
                        "kind": m.entity.kind.as_str(),
                        "revision": m.entity.revision,
                    }),
                );
                if let Some(pkg) = &m.owning_package {
                    obj.insert("owning_package".into(), json!(pkg));
                }
                obj.insert("requires".into(), Value::Array(req_json));
                if !roll_json.is_empty() {
                    obj.insert("rollouts".into(), Value::Array(roll_json));
                }
                Value::Object(obj)
            })
            .collect();

        let mut root = serde_json::Map::new();
        root.insert("members".into(), Value::Array(members_json));
        if let Some(lock_digest) = &self.provider_lock_digest {
            root.insert("provider_lock_digest".into(), json!(lock_digest));
        }
        root.insert("providers".into(), Value::Array(providers_json));
        root.insert("schema_version".into(), json!(self.schema_version));

        let bytes = canonical_bytes(&Value::Object(root)).map_err(|e| {
            DeploymentError::new("MANIFEST_CANONICAL_ERROR", e.code)
        })?;
        let digest = sha256_digest(&bytes);
        Ok((bytes, digest))
    }
}

// ============================================================
// V2-03.2..V2-03.4 & V2-04.1..V2-04.6: Immutable Storage & Lifecycle
// ============================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct DeploymentError {
    pub code: String,
    pub message: String,
}

impl DeploymentError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ArtifactState {
    Active,
    Draining,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactObjectRow {
    pub digest: String,
    pub size_bytes: usize,
    pub state: ArtifactState,
    pub drain_epoch: Option<u64>,
    pub bytes: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderPackageRow {
    pub package_digest: String,
    pub publisher: String,
    pub provider_id: String,
    pub version: String,
    pub implementation: ImplementationKind,
    pub manifest_json: Value,
    pub distribution_json: Value,
    pub inventory_json: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAdmissionRow {
    pub package_digest: String,
    pub admission_fingerprint: String,
    pub policy_id: String,
    pub revoked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionRevisionRow {
    pub connection_id: String,
    pub revision: u64,
    pub provider_digest: String,
    pub public_config: Value,
    /// Versioned secret locator (never raw secret bytes in public projections/hashes).
    pub secret_locator: Option<String>,
    pub configuration_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentRevisionRow {
    pub revision: u64,
    pub parent_revision: Option<u64>,
    pub manifest_digest: String,
    pub manifest_bytes: Vec<u8>,
    pub manifest: CanonicalManifest,
    pub reason: String,
    pub created_by: String,
    pub members_compacted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentReceipt {
    pub operation_id: String,
    pub principal: String,
    pub idempotency_key: String,
    pub request_hash: String,
    pub committed_revision: u64,
    pub manifest_digest: String,
    pub epoch: u64,
    pub receiving_node_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidatePlan {
    pub base_revision: u64,
    pub candidate_manifest: CanonicalManifest,
    pub candidate_digest: String,
    pub candidate_bytes: Vec<u8>,
    pub added_entities: Vec<EntityRef>,
    pub removed_entities: Vec<EntityRef>,
    pub external_effect_warnings: Vec<String>,
}

impl CandidatePlan {
    /// Compute the deterministic request hash over `(operation, base_revision,
    /// candidate_digest, removals, reason)` (`V2-03.3`, `V2-04.3`).
    pub fn compute_request_hash(&self, operation: &str, reason: &str) -> Result<String, DeploymentError> {
        let removals: Vec<Value> = self
            .removed_entities
            .iter()
            .map(|r| {
                json!({
                    "id": r.id,
                    "kind": r.kind.as_str(),
                    "revision": r.revision,
                })
            })
            .collect();
        let doc = json!({
            "base_revision": self.base_revision,
            "candidate_digest": self.candidate_digest,
            "operation": operation,
            "reason": reason,
            "removals": removals,
        });
        let bytes = canonical_bytes(&doc)
            .map_err(|e| DeploymentError::new("REQUEST_HASH_INVALID", e.code))?;
        Ok(sha256_digest(&bytes))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntryInput {
    pub path: String,
    pub bytes: Vec<u8>,
    pub compressed_size: usize,
    pub is_symlink: bool,
}

#[derive(Debug, Default)]
struct DeploymentState {
    head_revision: u64,
    epoch: u64,
    artifacts: BTreeMap<String, ArtifactObjectRow>,
    packages: BTreeMap<String, ProviderPackageRow>,
    publisher_bindings: BTreeMap<(String, String, String), String>,
    admissions: BTreeMap<String, ProviderAdmissionRow>,
    connections: BTreeMap<(String, u64), ConnectionRevisionRow>,
    revisions: BTreeMap<u64, DeploymentRevisionRow>,
    receipts: BTreeMap<(String, String), DeploymentReceipt>,
    active_node_revisions: BTreeMap<String, BTreeSet<u64>>,
}

/// Transactional immutable deployment and artifact store (`V2-03`, `V2-04`).
#[derive(Clone)]
pub struct ImmutableDeploymentStore {
    inner: Arc<Mutex<DeploymentState>>,
    node_id: String,
}

impl Default for ImmutableDeploymentStore {
    fn default() -> Self {
        Self::new("node-local-1")
    }
}

impl ImmutableDeploymentStore {
    pub fn new(node_id: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(DeploymentState::default())),
            node_id: node_id.into(),
        }
    }

    pub fn current_head(&self) -> (u64, u64) {
        let g = self.inner.lock().expect("store lock");
        (g.head_revision, g.epoch)
    }

    pub fn get_revision(&self, revision: u64) -> Option<DeploymentRevisionRow> {
        let g = self.inner.lock().expect("store lock");
        g.revisions.get(&revision).cloned()
    }

    pub fn list_revisions(&self) -> Vec<DeploymentRevisionRow> {
        let g = self.inner.lock().expect("store lock");
        g.revisions.values().cloned().collect()
    }

    pub fn list_packages(&self) -> Vec<ProviderPackageRow> {
        let g = self.inner.lock().expect("store lock");
        g.packages.values().cloned().collect()
    }

    pub fn list_admissions(&self) -> Vec<ProviderAdmissionRow> {
        let g = self.inner.lock().expect("store lock");
        g.admissions.values().cloned().collect()
    }

    pub fn list_connections(&self) -> Vec<ConnectionRevisionRow> {
        let g = self.inner.lock().expect("store lock");
        g.connections.values().cloned().collect()
    }

    /// V2-04.1: Ingest a bounded archive, rejecting symlinks, path traversal,
    /// decompression ratio bombs (> 20x), duplicate entries, and inventory
    /// mismatches. Upload creates no serving routes or active generations.
    pub fn ingest_verified_archive(
        &self,
        entries: &[ArchiveEntryInput],
    ) -> Result<ProviderPackageRow, DeploymentError> {
        if entries.is_empty() || entries.len() > 1_024 {
            return Err(DeploymentError::new(
                "ARCHIVE_ENTRY_COUNT_EXCEEDED",
                "archive entry count outside 1..=1024",
            ));
        }

        let mut seen_paths = HashSet::new();
        let mut raw_inventory: Option<Vec<u8>> = None;
        let mut payloads: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        let mut total_uncompressed = 0usize;

        for entry in entries {
            if entry.is_symlink {
                return Err(DeploymentError::new(
                    "ARCHIVE_SYMLINK_DENIED",
                    format!("symlink entry '{}' is forbidden", entry.path),
                ));
            }
            validate_safe_package_path(&entry.path, "ARCHIVE_PATH_INVALID")
                .map_err(|f| DeploymentError::new(f.code, f.location))?;
            if !seen_paths.insert(entry.path.to_ascii_lowercase()) {
                return Err(DeploymentError::new(
                    "ARCHIVE_DUPLICATE_ENTRY",
                    format!("duplicate or case-colliding archive path '{}'", entry.path),
                ));
            }
            if entry.compressed_size > 0 && entry.bytes.len() / entry.compressed_size.max(1) > 20 {
                return Err(DeploymentError::new(
                    "ARCHIVE_COMPRESSION_RATIO_EXCEEDED",
                    format!("decompression ratio exceeded on '{}'", entry.path),
                ));
            }
            total_uncompressed += entry.bytes.len();
            if entry.bytes.len() > 16 * 1024 * 1024 || total_uncompressed > 64 * 1024 * 1024 {
                return Err(DeploymentError::new(
                    "ARCHIVE_SIZE_LIMIT_EXCEEDED",
                    "archive size limit exceeded",
                ));
            }
            if entry.path == "inventory.json" {
                raw_inventory = Some(entry.bytes.clone());
            } else if !entry.path.starts_with("signatures/") {
                payloads.insert(entry.path.clone(), entry.bytes.clone());
            }
        }

        let raw_inv = raw_inventory.ok_or_else(|| {
            DeploymentError::new("ARCHIVE_INVENTORY_MISSING", "missing root inventory.json")
        })?;
        let inventory_json: Value = serde_json::from_slice(&raw_inv).map_err(|_| {
            DeploymentError::new("ARCHIVE_INVENTORY_INVALID", "invalid inventory.json")
        })?;
        let package_digest = verify_inventory(&inventory_json, &payloads, Some(&raw_inv))
            .map_err(|f| DeploymentError::new(f.code, "inventory verification failed"))?;

        let manifest_json: Value = serde_json::from_slice(&payloads["connector.json"])
            .map_err(|_| DeploymentError::new("MANIFEST_INVALID_JSON", "connector.json"))?;
        let distribution_json: Value = serde_json::from_slice(&payloads["distribution.json"])
            .map_err(|_| DeploymentError::new("DISTRIBUTION_INVALID_JSON", "distribution.json"))?;

        let file_list: Vec<String> = payloads.keys().cloned().collect();
        let mut schemas = BTreeMap::new();
        for (path, bytes) in &payloads {
            if path.starts_with("schemas/") && path.ends_with(".json") {
                let val: Value = serde_json::from_slice(bytes)
                    .map_err(|_| DeploymentError::new("SCHEMA_INVALID_JSON", path))?;
                schemas.insert(path.clone(), val);
            }
        }
        check_manifest_bundle(&manifest_json, &file_list, &schemas)
            .map_err(|f| DeploymentError::new(f.code, f.location))?;

        let publisher = distribution_json["publisher"]
            .as_str()
            .ok_or_else(|| DeploymentError::new("DISTRIBUTION_SHAPE_INVALID", "publisher"))?
            .to_string();
        let provider_id = manifest_json["id"].as_str().unwrap().to_string();
        let version = manifest_json["version"].as_str().unwrap().to_string();
        let impl_kind = match manifest_json["implementation"]["kind"].as_str().unwrap() {
            "workflow" => ImplementationKind::Workflow,
            "wasm" => ImplementationKind::Wasm,
            _ => ImplementationKind::Rust,
        };

        let row = ProviderPackageRow {
            package_digest: package_digest.clone(),
            publisher: publisher.clone(),
            provider_id: provider_id.clone(),
            version: version.clone(),
            implementation: impl_kind,
            manifest_json,
            distribution_json,
            inventory_json,
        };

        let mut g = self.inner.lock().expect("store lock");
        let id_key = (publisher, provider_id, version);
        if let Some(existing_digest) = g.publisher_bindings.get(&id_key)
            && existing_digest != &package_digest
        {
            return Err(DeploymentError::new(
                "PUBLISHER_VERSION_CONFLICT",
                "publisher/provider_id/version is already bound to a different digest",
            ));
        }
        if let Some(existing_pkg) = g.packages.get(&package_digest)
            && existing_pkg != &row
        {
            return Err(DeploymentError::new(
                "IMMUTABLE_CONTENT_CONFLICT",
                "immutable package digest already exists with different metadata",
            ));
        }

        // Store payload objects and package inventory as ACTIVE artifacts
        for bytes in payloads.values() {
            let obj_digest = sha256_digest(bytes);
            g.artifacts
                .entry(obj_digest.clone())
                .or_insert_with(|| ArtifactObjectRow {
                    digest: obj_digest,
                    size_bytes: bytes.len(),
                    state: ArtifactState::Active,
                    drain_epoch: None,
                    bytes: Some(bytes.clone()),
                });
        }
        g.artifacts.insert(
            package_digest.clone(),
            ArtifactObjectRow {
                digest: package_digest.clone(),
                size_bytes: raw_inv.len(),
                state: ArtifactState::Active,
                drain_epoch: None,
                bytes: Some(raw_inv),
            },
        );
        g.publisher_bindings.insert(id_key, package_digest.clone());
        g.packages.insert(package_digest, row.clone());
        Ok(row)
    }

    /// V2-04.2: Admit and install a locked package under `policy`, recording
    /// its admission fingerprint over runtime, profile, capability, and policy.
    pub fn admit_package(
        &self,
        package_digest: &str,
        policy: &dyn AdmissionPolicy,
        lock: Option<&Value>,
    ) -> Result<ProviderAdmissionRow, DeploymentError> {
        let mut g = self.inner.lock().expect("store lock");
        let Some(pkg) = g.packages.get(package_digest).cloned() else {
            return Err(DeploymentError::new(
                "PACKAGE_NOT_FOUND",
                format!("package '{package_digest}' is not in store"),
            ));
        };
        if g.artifacts
            .get(package_digest)
            .is_none_or(|a| a.state != ArtifactState::Active)
        {
            return Err(DeploymentError::new(
                "ARTIFACT_NOT_ACTIVE",
                format!("package '{package_digest}' is not ACTIVE"),
            ));
        }

        if let Some(lock_doc) = lock {
            let mut verified = BTreeMap::new();
            for p in g.packages.values() {
                let mut ops: Vec<String> = p.manifest_json["operations"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect();
                ops.sort();
                let impl_str = p.manifest_json["implementation"]["kind"]
                    .as_str()
                    .unwrap()
                    .to_string();
                let import_profile = if impl_str == "wasm" {
                    match p.manifest_json["implementation"]["world"].as_str().unwrap() {
                        "orion:connector/connector@1.0.0" => Some("core-v1".to_string()),
                        "orion:connector/connector-http@1.0.0" => Some("http-v1".to_string()),
                        "orion:connector/connector-composition@1.0.0" => {
                            Some("composition-v1".to_string())
                        }
                        "orion:connector/connector-sessions@1.0.0" => {
                            Some("sessions-v1".to_string())
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                verified.insert(
                    ("provider".to_string(), p.package_digest.clone()),
                    VerifiedArtifactMetadata {
                        publisher: p.publisher.clone(),
                        id: p.provider_id.clone(),
                        version: p.version.clone(),
                        contract: "orion:connector@1.0.0".to_string(),
                        schema_profile: Some("orion-json-schema/1".to_string()),
                        implementation: Some(impl_str),
                        import_profile,
                        operations: ops,
                        functions: Vec::new(),
                        dependencies: Vec::new(),
                    },
                );
            }
            check_dependency_lock(lock_doc, &verified)
                .map_err(|f| DeploymentError::new(f.code, "dependency lock check failed"))?;
        }

        let tier = pkg
            .distribution_json
            .get("tier")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                if pkg
                    .distribution_json
                    .get("policy_requirements")
                    .and_then(Value::as_array)
                    .is_some_and(|reqs| !reqs.is_empty())
                {
                    Some("enterprise".to_string())
                } else {
                    Some("community".to_string())
                }
            });

        let req = PackageAdmissionRequest {
            provider: ProviderReference {
                provider_id: pkg.provider_id.clone(),
                version: pkg.version.clone(),
                package_digest: pkg.package_digest.clone(),
            },
            implementation: pkg.implementation,
            target_id: pkg.provider_id.clone(),
            tier,
            signature: None,
        };
        let verdict = policy
            .verify_package(&req)
            .map_err(|e| DeploymentError::new(e.code, e.message))?;

        let fp_doc = json!({
            "package_digest": pkg.package_digest,
            "policy_id": verdict.policy_id,
            "runtime_abi": "orion-v2",
            "schema_profile": "orion-json-schema/1",
        });
        let fp = sha256_digest(&canonical_bytes(&fp_doc).unwrap());
        let admission = ProviderAdmissionRow {
            package_digest: pkg.package_digest,
            admission_fingerprint: fp,
            policy_id: verdict.policy_id,
            revoked: false,
        };
        g.admissions
            .insert(admission.package_digest.clone(), admission.clone());
        Ok(admission)
    }

    pub fn revoke_package(&self, package_digest: &str) -> Result<(), DeploymentError> {
        let mut g = self.inner.lock().expect("store lock");
        let Some(adm) = g.admissions.get_mut(package_digest) else {
            return Err(DeploymentError::new("ADMISSION_NOT_FOUND", package_digest));
        };
        adm.revoked = true;
        Ok(())
    }

    /// V2-03.2: Store an immutable connection revision with versioned secret
    /// locator and redacted public configuration digest.
    pub fn store_connection_revision(
        &self,
        connection_id: &str,
        revision: u64,
        provider_digest: &str,
        public_config: Value,
        secret_locator: Option<String>,
    ) -> Result<ConnectionRevisionRow, DeploymentError> {
        if revision == 0 {
            return Err(DeploymentError::new(
                "CONNECTION_REVISION_INVALID",
                "revision must be >= 1",
            ));
        }
        // Reject raw secret keys inside public_config
        if let Some(obj) = public_config.as_object()
            && obj.keys().any(|k| k.contains("secret") || k.contains("password"))
        {
            return Err(DeploymentError::new(
                "CONNECTION_PUBLIC_SECRET_FORBIDDEN",
                "secret values must use secret_locator, not public_config",
            ));
        }
        let cfg_bytes = serde_json::to_vec(&public_config)
            .map_err(|_| DeploymentError::new("CONNECTION_CONFIG_INVALID", connection_id))?;
        let cfg_digest = sha256_digest(&cfg_bytes);
        let row = ConnectionRevisionRow {
            connection_id: connection_id.to_string(),
            revision,
            provider_digest: provider_digest.to_string(),
            public_config,
            secret_locator,
            configuration_digest: cfg_digest.clone(),
        };

        let mut g = self.inner.lock().expect("store lock");
        let key = (connection_id.to_string(), revision);
        if let Some(existing) = g.connections.get(&key) {
            if existing != &row {
                return Err(DeploymentError::new(
                    "IMMUTABLE_CONNECTION_CONFLICT",
                    "immutable connection revision cannot be modified",
                ));
            }
            return Ok(existing.clone());
        }
        g.artifacts.insert(
            cfg_digest.clone(),
            ArtifactObjectRow {
                digest: cfg_digest,
                size_bytes: cfg_bytes.len(),
                state: ArtifactState::Active,
                drain_epoch: None,
                bytes: Some(cfg_bytes),
            },
        );
        g.connections.insert(key, row.clone());
        Ok(row)
    }

    /// V2-04.3: Build a `CandidatePlan` outside any write transaction, checking
    /// base revision, manifest closure, and computing added/removed diffs.
    pub fn plan_candidate(
        &self,
        base_revision: u64,
        candidate_manifest: CanonicalManifest,
    ) -> Result<CandidatePlan, DeploymentError> {
        let (candidate_bytes, candidate_digest) = candidate_manifest.canonicalize_and_verify()?;
        let g = self.inner.lock().expect("store lock");

        let base_entities: BTreeSet<EntityRef> = if base_revision == 0 {
            BTreeSet::new()
        } else {
            let Some(base_row) = g.revisions.get(&base_revision) else {
                return Err(DeploymentError::new(
                    "BASE_REVISION_NOT_FOUND",
                    format!("base revision {base_revision} does not exist"),
                ));
            };
            base_row
                .manifest
                .members
                .iter()
                .map(|m| m.entity.clone())
                .collect()
        };

        // Check that all providers in the candidate manifest are admitted and not revoked
        for p in &candidate_manifest.providers {
            let Some(adm) = g.admissions.get(&p.digest) else {
                return Err(DeploymentError::new(
                    "PROVIDER_NOT_ADMITTED",
                    format!("provider '{}' ({}) is not admitted", p.provider_id, p.digest),
                ));
            };
            if adm.revoked {
                return Err(DeploymentError::new(
                    "PROVIDER_REVOKED",
                    format!("provider '{}' ({}) is revoked", p.provider_id, p.digest),
                ));
            }
            if g.artifacts
                .get(&p.digest)
                .is_none_or(|a| a.state != ArtifactState::Active)
            {
                return Err(DeploymentError::new(
                    "ARTIFACT_NOT_ACTIVE",
                    format!("provider artifact '{}' is not ACTIVE", p.digest),
                ));
            }
        }

        let next_entities: BTreeSet<EntityRef> = candidate_manifest
            .members
            .iter()
            .map(|m| m.entity.clone())
            .collect();
        let added_entities: Vec<EntityRef> =
            next_entities.difference(&base_entities).cloned().collect();
        let removed_entities: Vec<EntityRef> =
            base_entities.difference(&next_entities).cloned().collect();

        Ok(CandidatePlan {
            base_revision,
            candidate_manifest,
            candidate_digest,
            candidate_bytes,
            added_entities,
            removed_entities,
            external_effect_warnings: Vec::new(),
        })
    }

    /// V2-04.4: Commit an idempotent CAS deployment apply under canonical lock
    /// order (`deployment_head -> deployment_operations -> provider_admissions -> artifact_objects`).
    pub fn apply_candidate(
        &self,
        principal: &str,
        idempotency_key: &str,
        plan: &CandidatePlan,
        reason: &str,
        policy: &dyn AdmissionPolicy,
    ) -> Result<DeploymentReceipt, DeploymentError> {
        let request_hash = plan.compute_request_hash("apply", reason)?;
        let mut g = self.inner.lock().expect("store lock");

        // 1. Receipt-first lookup on (principal, idempotency_key)
        let op_key = (principal.to_string(), idempotency_key.to_string());
        if let Some(existing) = g.receipts.get(&op_key) {
            if existing.request_hash == request_hash {
                return Ok(existing.clone());
            }
            return Err(DeploymentError::new(
                "IDEMPOTENCY_KEY_CONFLICT",
                "idempotency_key already used by principal with different request parameters",
            ));
        }

        // 2. CAS check on deployment_head
        if g.head_revision != plan.base_revision {
            return Err(DeploymentError::new(
                "DEPLOYMENT_HEAD_CONFLICT",
                format!(
                    "expected base_revision {}, but current head_revision is {}",
                    plan.base_revision, g.head_revision
                ),
            ));
        }

        // 3. Transactional recheck of provider_admissions, policy revocations, and ACTIVE artifacts
        for p in &plan.candidate_manifest.providers {
            if policy
                .check_runtime_lease(0, &p.provider_id, "", super::contract::EffectStatus::NotStarted)
                .is_err()
            {
                return Err(DeploymentError::new(
                    "PROVIDER_REVOKED",
                    format!("package '{}' is revoked by admission policy", p.digest),
                ));
            }
            let Some(adm) = g.admissions.get(&p.digest) else {
                return Err(DeploymentError::new("PROVIDER_NOT_ADMITTED", &p.digest));
            };
            if adm.revoked || adm.policy_id != policy.policy_id() {
                return Err(DeploymentError::new(
                    "ADMISSION_INVALIDATED",
                    format!("admission for '{}' is revoked or policy changed", p.digest),
                ));
            }
            if g.artifacts
                .get(&p.digest)
                .is_none_or(|a| a.state != ArtifactState::Active)
            {
                return Err(DeploymentError::new(
                    "ARTIFACT_DRAINING_OR_DELETED",
                    format!("artifact '{}' is not ACTIVE", p.digest),
                ));
            }
        }
        for m in &plan.candidate_manifest.members {
            if let Some(art) = g.artifacts.get(&m.definition_digest)
                && art.state != ArtifactState::Active
            {
                return Err(DeploymentError::new(
                    "ARTIFACT_DRAINING_OR_DELETED",
                    format!("member artifact '{}' is not ACTIVE", m.definition_digest),
                ));
            }
        }

        // 4. Advance head_revision and epoch atomically and persist receipt
        let next_revision = g.head_revision + 1;
        let next_epoch = g.epoch + 1;
        g.head_revision = next_revision;
        g.epoch = next_epoch;

        g.revisions.insert(
            next_revision,
            DeploymentRevisionRow {
                revision: next_revision,
                parent_revision: (plan.base_revision > 0).then_some(plan.base_revision),
                manifest_digest: plan.candidate_digest.clone(),
                manifest_bytes: plan.candidate_bytes.clone(),
                manifest: plan.candidate_manifest.clone(),
                reason: reason.to_string(),
                created_by: principal.to_string(),
                members_compacted: false,
            },
        );

        let receipt = DeploymentReceipt {
            operation_id: format!("op-{next_revision}-{next_epoch}"),
            principal: principal.to_string(),
            idempotency_key: idempotency_key.to_string(),
            request_hash,
            committed_revision: next_revision,
            manifest_digest: plan.candidate_digest.clone(),
            epoch: next_epoch,
            receiving_node_id: self.node_id.clone(),
        };
        g.receipts.insert(op_key, receipt.clone());
        Ok(receipt)
    }

    /// V2-04.5: Plan and apply a rollback to `target_revision` as a NEW
    /// monotonic revision on top of `expected_head_revision`, re-verifying
    /// current compatibility, admissions, and revocations.
    pub fn rollback_to_revision(
        &self,
        principal: &str,
        idempotency_key: &str,
        expected_head_revision: u64,
        target_revision: u64,
        reason: &str,
        policy: &dyn AdmissionPolicy,
    ) -> Result<(DeploymentReceipt, Vec<String>), DeploymentError> {
        let target_manifest = {
            let g = self.inner.lock().expect("store lock");
            let Some(target) = g.revisions.get(&target_revision) else {
                return Err(DeploymentError::new(
                    "ROLLBACK_TARGET_NOT_FOUND",
                    format!("target revision {target_revision} does not exist"),
                ));
            };
            if target.members_compacted {
                return Err(DeploymentError::new(
                    "ROLLBACK_TARGET_COMPACTED",
                    format!("target revision {target_revision} has been compacted"),
                ));
            }
            target.manifest.clone()
        };

        let mut plan = self.plan_candidate(expected_head_revision, target_manifest)?;
        plan.external_effect_warnings.push(
            "Configuration rollback creates a new deployment revision; already-emitted external side effects are not reversed."
                .to_string(),
        );
        let warnings = plan.external_effect_warnings.clone();
        let receipt = self.apply_candidate(principal, idempotency_key, &plan, reason, policy)?;
        Ok((receipt, warnings))
    }

    /// Record the revisions currently held (applied + retained in-flight) by a node.
    pub fn update_node_retained_revisions(&self, node_id: &str, revisions: BTreeSet<u64>) {
        let mut g = self.inner.lock().expect("store lock");
        g.active_node_revisions
            .insert(node_id.to_string(), revisions);
    }

    /// V2-04.6: Two-phase epoch-fenced garbage collection (`ACTIVE -> DRAINING -> DELETED`)
    /// and intermediate `deployment_members` compaction.
    ///
    /// Protects:
    /// - Current `deployment_head`
    /// - Recent rollback window (`retain_recent_revisions`)
    /// - Any revision held in `active_node_revisions`
    pub fn mark_and_collect_garbage(
        &self,
        retain_recent_revisions: usize,
    ) -> (Vec<String>, Vec<String>, Vec<u64>) {
        let mut g = self.inner.lock().expect("store lock");
        let head = g.head_revision;
        let current_epoch = g.epoch;

        let mut protected_revisions: BTreeSet<u64> = BTreeSet::new();
        if head > 0 {
            protected_revisions.insert(head);
        }
        for rev in g.revisions.keys().rev().take(retain_recent_revisions) {
            protected_revisions.insert(*rev);
        }
        for held in g.active_node_revisions.values() {
            for r in held {
                protected_revisions.insert(*r);
            }
        }

        let mut protected_digests: HashSet<String> = HashSet::new();
        for rev_id in &protected_revisions {
            if let Some(rev) = g.revisions.get(rev_id) {
                for p in &rev.manifest.providers {
                    protected_digests.insert(p.digest.clone());
                }
                for m in &rev.manifest.members {
                    protected_digests.insert(m.definition_digest.clone());
                }
            }
        }

        let mut compacted_revisions = Vec::new();
        let rev_keys: Vec<u64> = g.revisions.keys().copied().collect();
        for rev_id in rev_keys {
            if !protected_revisions.contains(&rev_id)
                && let Some(rev) = g.revisions.get_mut(&rev_id)
                && !rev.members_compacted
            {
                rev.members_compacted = true;
                compacted_revisions.push(rev_id);
            }
        }

        let mut newly_draining = Vec::new();
        let mut newly_deleted = Vec::new();
        for (digest, art) in g.artifacts.iter_mut() {
            if protected_digests.contains(digest) {
                continue;
            }
            match art.state {
                ArtifactState::Active => {
                    art.state = ArtifactState::Draining;
                    art.drain_epoch = Some(current_epoch);
                    newly_draining.push(digest.clone());
                }
                ArtifactState::Draining => {
                    art.state = ArtifactState::Deleted;
                    art.bytes = None;
                    newly_deleted.push(digest.clone());
                }
                ArtifactState::Deleted => {}
            }
        }

        (newly_draining, newly_deleted, compacted_revisions)
    }

    /// V2-03.5: Convert a consistent legacy v1 snapshot into an explicit
    /// bootstrap `CanonicalManifest`, blocking if any connector maps to an
    /// unavailable tier or has ambiguous ownership.
    pub fn build_legacy_bootstrap_manifest(
        edition: EditionTier,
        legacy_connectors: &[(&str, &Value)],
        legacy_workflows: &[(&str, u64, &Value)],
        legacy_channels: &[(&str, u64, &str, u64)],
    ) -> Result<CanonicalManifest, DeploymentError> {
        let mut providers_map: BTreeMap<String, ProviderRef> = BTreeMap::new();
        let mut members: Vec<ManifestMember> = Vec::new();

        for (conn_id, cfg) in legacy_connectors {
            let conn_type = cfg
                .get("connector_type")
                .or_else(|| cfg.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("db");
            let resolved = map_legacy_connector(conn_id, conn_type, cfg).map_err(|f| {
                DeploymentError::new(f.code, format!("{}: {}", f.entity_name, f.message))
            })?;
            if edition == EditionTier::Community && resolved.tier == EditionTier::Enterprise {
                return Err(DeploymentError::new(
                    "BACKFILL_ENTERPRISE_PROVIDER_REQUIRED",
                    format!(
                        "connector '{conn_id}' requires Enterprise provider '{}'",
                        resolved.provider_id
                    ),
                ));
            }
            let p_digest = sha256_digest(resolved.provider_id.as_bytes());
            providers_map.insert(
                resolved.provider_id.to_string(),
                ProviderRef {
                    publisher: "org.orion".to_string(),
                    provider_id: resolved.provider_id.to_string(),
                    version: "2.0.0".to_string(),
                    digest: p_digest,
                },
            );
            members.push(ManifestMember {
                entity: EntityRef {
                    kind: EntityKind::Connection,
                    id: (*conn_id).to_string(),
                    revision: 1,
                },
                definition_digest: sha256_digest(&serde_json::to_vec(cfg).unwrap_or_default()),
                owning_package: None,
                requires: Vec::new(),
                rollouts: Vec::new(),
            });
        }

        for (wf_id, rev, wf_json) in legacy_workflows {
            members.push(ManifestMember {
                entity: EntityRef {
                    kind: EntityKind::Workflow,
                    id: (*wf_id).to_string(),
                    revision: *rev,
                },
                definition_digest: sha256_digest(&serde_json::to_vec(wf_json).unwrap_or_default()),
                owning_package: None,
                requires: Vec::new(),
                rollouts: Vec::new(),
            });
        }

        for (ch_id, ch_rev, wf_id, wf_rev) in legacy_channels {
            let wf_ref = EntityRef {
                kind: EntityKind::Workflow,
                id: (*wf_id).to_string(),
                revision: *wf_rev,
            };
            members.push(ManifestMember {
                entity: EntityRef {
                    kind: EntityKind::Channel,
                    id: (*ch_id).to_string(),
                    revision: *ch_rev,
                },
                definition_digest: sha256_digest(format!("{ch_id}:{ch_rev}:{wf_id}:{wf_rev}").as_bytes()),
                owning_package: None,
                requires: vec![wf_ref],
                rollouts: vec![RolloutInterval {
                    workflow_revision: *wf_rev,
                    start_bucket: 0,
                    end_bucket: 100,
                }],
            });
        }

        let manifest = CanonicalManifest {
            schema_version: 2,
            providers: providers_map.into_values().collect(),
            provider_lock_digest: None,
            members,
        };
        manifest.canonicalize_and_verify()?;
        Ok(manifest)
    }

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
            let cfg_str = serde_json::to_string(&conn.public_config).unwrap_or_else(|_| "{}".into());
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
            let manifest_str =
                serde_json::to_string(&rev.manifest).unwrap_or_else(|_| "{}".into());
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::CommunityAdmissionPolicy;
    use std::path::PathBuf;

    fn sample_archive_entries() -> Vec<ArchiveEntryInput> {
        let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../design-docs/v2/fixtures/package-identity");
        let bundle: Value =
            serde_json::from_str(&std::fs::read_to_string(fixture_dir.join("bundle.json")).unwrap())
                .unwrap();
        let pkg = &bundle["providers"][1]; // helper wasm package with no dependencies
        let mut entries = Vec::new();
        for (path, text) in pkg["payloads"].as_object().unwrap() {
            let bytes = text.as_str().unwrap().as_bytes().to_vec();
            entries.push(ArchiveEntryInput {
                path: path.clone(),
                compressed_size: bytes.len(),
                bytes,
                is_symlink: false,
            });
        }
        let inv_bytes = canonical_bytes(&pkg["inventory"]).unwrap();
        entries.push(ArchiveEntryInput {
            path: "inventory.json".to_string(),
            compressed_size: inv_bytes.len(),
            bytes: inv_bytes,
            is_symlink: false,
        });
        entries
    }

    #[test]
    fn v2_03_1_typed_refs_disjoint_rollouts_and_canonical_manifest_verification() {
        // Reject integer/digest confusion on ProviderRef
        let bad_provider = ProviderRef {
            publisher: "org.orion".to_string(),
            provider_id: "org.orion.http".to_string(),
            version: "42".to_string(),
            digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                .to_string(),
        };
        assert_eq!(
            bad_provider.validate().unwrap_err().code,
            "PROVIDER_REF_INVALID"
        );

        // Reject overlapping and gapped rollout intervals
        let gap_rollout = vec![
            RolloutInterval {
                workflow_revision: 1,
                start_bucket: 0,
                end_bucket: 50,
            },
            RolloutInterval {
                workflow_revision: 2,
                start_bucket: 60,
                end_bucket: 100,
            },
        ];
        assert_eq!(
            validate_rollout_partition(&gap_rollout).unwrap_err().code,
            "ROLLOUT_PARTITION_INVALID"
        );

        // Valid partition [0, 20) + [20, 100)
        let valid_rollout = vec![
            RolloutInterval {
                workflow_revision: 2,
                start_bucket: 0,
                end_bucket: 20,
            },
            RolloutInterval {
                workflow_revision: 1,
                start_bucket: 20,
                end_bucket: 100,
            },
        ];
        validate_rollout_partition(&valid_rollout).expect("disjoint full partition");
    }

    #[test]
    fn v2_04_1_2_3_4_5_6_end_to_end_archive_admit_plan_apply_rollback_and_gc() {
        let store = ImmutableDeploymentStore::new("node-test-1");
        let policy = CommunityAdmissionPolicy::new();

        // 1. Reject symlink in archive (V2-04.1)
        let mut bad_entries = sample_archive_entries();
        bad_entries[0].is_symlink = true;
        assert_eq!(
            store.ingest_verified_archive(&bad_entries).unwrap_err().code,
            "ARCHIVE_SYMLINK_DENIED"
        );

        // 2. Ingest verified archive and admit package (V2-04.1, V2-04.2)
        let entries = sample_archive_entries();
        let pkg = store.ingest_verified_archive(&entries).expect("ingested");
        let adm = store
            .admit_package(&pkg.package_digest, &policy, None)
            .expect("admitted");
        assert!(!adm.revoked);

        // Upload/install alone does not advance deployment_head
        assert_eq!(store.current_head(), (0, 0));

        // 3. Plan & commit revision 1 (V2-04.3, V2-04.4)
        let manifest_v1 = CanonicalManifest {
            schema_version: 2,
            providers: vec![ProviderRef {
                publisher: pkg.publisher.clone(),
                provider_id: pkg.provider_id.clone(),
                version: pkg.version.clone(),
                digest: pkg.package_digest.clone(),
            }],
            provider_lock_digest: None,
            members: vec![ManifestMember {
                entity: EntityRef {
                    kind: EntityKind::Workflow,
                    id: "orders-wf".to_string(),
                    revision: 1,
                },
                definition_digest: sha256_digest(b"wf-v1"),
                owning_package: None,
                requires: Vec::new(),
                rollouts: Vec::new(),
            }],
        };
        let plan_v1 = store.plan_candidate(0, manifest_v1).expect("planned v1");
        let receipt_v1 = store
            .apply_candidate("alice", "idem-1", &plan_v1, "initial deploy", &policy)
            .expect("applied v1");
        assert_eq!(receipt_v1.committed_revision, 1);
        assert_eq!(store.current_head(), (1, 1));

        // Lost-response retry with same principal + idempotency_key + request returns original receipt
        let retry_v1 = store
            .apply_candidate("alice", "idem-1", &plan_v1, "initial deploy", &policy)
            .expect("idempotent retry");
        assert_eq!(retry_v1, receipt_v1);
        assert_eq!(store.current_head(), (1, 1));

        // Reusing idempotency_key with different reason/request fails
        let conflict = store
            .apply_candidate("alice", "idem-1", &plan_v1, "changed reason", &policy)
            .unwrap_err();
        assert_eq!(conflict.code, "IDEMPOTENCY_KEY_CONFLICT");

        // 4. Plan & commit revision 2
        let manifest_v2 = CanonicalManifest {
            schema_version: 2,
            providers: Vec::new(),
            provider_lock_digest: None,
            members: vec![ManifestMember {
                entity: EntityRef {
                    kind: EntityKind::Workflow,
                    id: "orders-wf".to_string(),
                    revision: 2,
                },
                definition_digest: sha256_digest(b"wf-v2"),
                owning_package: None,
                requires: Vec::new(),
                rollouts: Vec::new(),
            }],
        };
        let plan_v2 = store.plan_candidate(1, manifest_v2).expect("planned v2");
        let receipt_v2 = store
            .apply_candidate("alice", "idem-2", &plan_v2, "upgrade wf", &policy)
            .expect("applied v2");
        assert_eq!(receipt_v2.committed_revision, 2);

        // 5. Rollback to revision 1 creates revision 3 (V2-04.5)
        let (rollback_receipt, warnings) = store
            .rollback_to_revision("alice", "idem-3", 2, 1, "rollback to v1", &policy)
            .expect("rolled back to v1");
        assert_eq!(rollback_receipt.committed_revision, 3);
        assert_eq!(rollback_receipt.manifest_digest, receipt_v1.manifest_digest);
        assert!(!warnings.is_empty());

        // 6. Two-phase GC marks unreferenced artifacts DRAINING then DELETED (V2-04.6)
        let mut empty_manifest = CanonicalManifest {
            schema_version: 2,
            providers: Vec::new(),
            provider_lock_digest: None,
            members: Vec::new(),
        };
        let plan_v4 = store.plan_candidate(3, empty_manifest.clone()).unwrap();
        store
            .apply_candidate("alice", "idem-4", &plan_v4, "clear estate", &policy)
            .unwrap();
        let (draining, deleted, compacted) = store.mark_and_collect_garbage(1);
        assert!(!draining.is_empty());
        assert!(deleted.is_empty());
        assert!(!compacted.is_empty());

        // Attempting to plan with a DRAINING artifact is rejected
        empty_manifest.providers.push(ProviderRef {
            publisher: pkg.publisher,
            provider_id: pkg.provider_id,
            version: pkg.version,
            digest: pkg.package_digest.clone(),
        });
        assert_eq!(
            store.plan_candidate(4, empty_manifest).unwrap_err().code,
            "ARTIFACT_NOT_ACTIVE"
        );

        // Second GC sweep transitions DRAINING -> DELETED while receipts remain intact
        let (_, deleted2, _) = store.mark_and_collect_garbage(1);
        assert!(deleted2.contains(&pkg.package_digest));
    }

    #[test]
    fn v2_03_5_legacy_backfill_converts_community_estate_and_blocks_enterprise_connectors() {
        let http_cfg = json!({ "type": "http", "base_url": "https://api.example.com" });
        let wf_json = json!({ "id": "wf1", "tasks": [] });
        let manifest = ImmutableDeploymentStore::build_legacy_bootstrap_manifest(
            EditionTier::Community,
            &[("ext-api", &http_cfg)],
            &[("wf1", 1, &wf_json)],
            &[("orders", 1, "wf1", 1)],
        )
        .expect("community estate converts");
        assert_eq!(manifest.providers.len(), 1);
        assert_eq!(manifest.providers[0].provider_id, "org.orion.http");

        // Enterprise connector (kafka) blocks Community backfill with actionable code
        let kafka_cfg = json!({ "type": "kafka", "brokers": ["localhost:9092"] });
        let err = ImmutableDeploymentStore::build_legacy_bootstrap_manifest(
            EditionTier::Community,
            &[("events-bus", &kafka_cfg)],
            &[],
            &[],
        )
        .unwrap_err();
        assert_eq!(err.code, "BACKFILL_ENTERPRISE_PROVIDER_REQUIRED");
    }

    #[tokio::test]
    async fn v2_03_2_to_4_sqlite_migration_and_persistence_roundtrip() {
        use std::str::FromStr;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::from_str("sqlite::memory:").unwrap(),
            )
            .await
            .unwrap();
        crate::storage::migrator_for(crate::storage::DbBackend::Sqlite)
            .run(&pool)
            .await
            .expect("sqlite migrations including 023_v2_immutable_deployments must apply");

        let store = ImmutableDeploymentStore::new("node-sql-1");
        let policy = CommunityAdmissionPolicy::new();
        let entries = sample_archive_entries();
        let pkg = store.ingest_verified_archive(&entries).unwrap();
        store
            .admit_package(&pkg.package_digest, &policy, None)
            .unwrap();
        store
            .store_connection_revision(
                "orders-db",
                1,
                &pkg.package_digest,
                json!({"url": "https://api.example.com"}),
                Some("vault://secrets/orders#v1".to_string()),
            )
            .unwrap();

        let manifest = CanonicalManifest {
            schema_version: 2,
            providers: vec![ProviderRef {
                publisher: pkg.publisher.clone(),
                provider_id: pkg.provider_id.clone(),
                version: pkg.version.clone(),
                digest: pkg.package_digest.clone(),
            }],
            provider_lock_digest: None,
            members: vec![ManifestMember {
                entity: EntityRef {
                    kind: EntityKind::Workflow,
                    id: "orders-wf".to_string(),
                    revision: 1,
                },
                definition_digest: sha256_digest(b"wf-sql-v1"),
                owning_package: None,
                requires: Vec::new(),
                rollouts: Vec::new(),
            }],
        };
        let plan = store.plan_candidate(0, manifest).unwrap();
        store
            .apply_candidate("alice", "sql-idem-1", &plan, "persist test", &policy)
            .unwrap();

        store
            .persist_to_sqlite(&pool)
            .await
            .expect("persist_to_sqlite succeeds");

        let (head_rev, epoch): (i64, i64) =
            sqlx::query_as("SELECT head_revision, epoch FROM deployment_head WHERE singleton_id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((head_rev, epoch), (1, 1));

        let (op_rev,): (i64,) = sqlx::query_as(
            "SELECT committed_revision FROM deployment_operations WHERE principal = 'alice' AND idempotency_key = 'sql-idem-1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(op_rev, 1);
    }
}
