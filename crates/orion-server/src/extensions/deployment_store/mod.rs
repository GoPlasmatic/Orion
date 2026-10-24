//! V2-03 & V2-04 Immutable storage, canonical manifests, bounded archive
//! ingestion, candidate planning, idempotent CAS apply, rollback by new
//! revision, two-phase `ACTIVE -> DRAINING -> DELETED` artifact GC, and legacy
//! snapshot backfill (`MOD-02`).

mod manifest;
mod sqlite;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub use manifest::{
    CanonicalManifest, EntityKind, EntityRef, ManifestMember, ProviderRef, RolloutInterval,
    validate_rollout_partition,
};

use super::admission::{AdmissionPolicy, PackageAdmissionRequest};
use super::contract::{ImplementationKind, ProviderReference};
use super::migration::{EditionTier, map_legacy_connector};
use super::package_identity::{
    VerifiedArtifactMetadata, canonical_bytes, check_dependency_lock, sha256_digest,
    verify_inventory,
};
use super::schema_profile::{check_manifest_bundle, validate_safe_package_path};

fn is_public_config_secret_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    const SECRET_PATTERNS: &[&str] = &[
        "secret",
        "password",
        "passwd",
        "pwd",
        "token",
        "api_key",
        "apikey",
        "api-key",
        "private_key",
        "private-key",
        "access_key",
        "access-key",
        "authorization",
        "credential",
        "bearer",
        "passphrase",
        "signing_key",
    ];
    SECRET_PATTERNS.iter().any(|pat| lower.contains(pat))
}

fn validate_no_secrets_in_public_config(value: &Value) -> Result<(), DeploymentError> {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if is_public_config_secret_key(k) {
                    return Err(DeploymentError::new(
                        "CONNECTION_PUBLIC_SECRET_FORBIDDEN",
                        format!("secret key '{k}' must use secret_locator, not public_config"),
                    ));
                }
                validate_no_secrets_in_public_config(v)?;
            }
            Ok(())
        }
        Value::Array(items) => {
            for item in items {
                validate_no_secrets_in_public_config(item)?;
            }
            Ok(())
        }
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.starts_with("Bearer ") || trimmed.starts_with("Basic ") {
                return Err(DeploymentError::new(
                    "CONNECTION_PUBLIC_SECRET_FORBIDDEN",
                    "authorization credentials must use secret_locator, not public_config",
                ));
            }
            if let Some(after_scheme) = trimmed.split("://").nth(1) {
                let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
                if authority.contains('@') {
                    return Err(DeploymentError::new(
                        "CONNECTION_PUBLIC_SECRET_FORBIDDEN",
                        "credential-bearing URL userinfo must use secret_locator, not public_config",
                    ));
                }
            }
            if let Ok(parsed) = url::Url::parse(trimmed) {
                if !parsed.username().is_empty() || parsed.password().is_some() {
                    return Err(DeploymentError::new(
                        "CONNECTION_PUBLIC_SECRET_FORBIDDEN",
                        "credential-bearing URL userinfo must use secret_locator, not public_config",
                    ));
                }
                for (qk, _) in parsed.query_pairs() {
                    if is_public_config_secret_key(&qk) {
                        return Err(DeploymentError::new(
                            "CONNECTION_PUBLIC_SECRET_FORBIDDEN",
                            format!(
                                "secret URL query parameter '{qk}' must use secret_locator, not public_config"
                            ),
                        ));
                    }
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn extract_inventory_file_digests(inventory_json: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(files) = inventory_json.get("files").and_then(Value::as_array) {
        for entry in files {
            if let Some(d) = entry
                .get("digest")
                .or_else(|| entry.get("sha256"))
                .and_then(Value::as_str)
            {
                if d.starts_with("sha256:") {
                    out.push(d.to_string());
                } else {
                    out.push(format!("sha256:{d}"));
                }
            }
        }
    }
    out
}

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
    pub fn compute_request_hash(
        &self,
        operation: &str,
        reason: &str,
    ) -> Result<String, DeploymentError> {
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

#[derive(Debug, Clone, Default)]
pub(super) struct DeploymentDirtySet {
    pub(super) head: bool,
    pub(super) expected_head_revision: Option<u64>,
    pub(super) artifacts: BTreeSet<String>,
    pub(super) packages: BTreeSet<String>,
    pub(super) admissions: BTreeSet<String>,
    pub(super) connections: BTreeSet<(String, u64)>,
    pub(super) revisions: BTreeSet<u64>,
    pub(super) receipts: BTreeSet<(String, String)>,
    pub(super) compacted_member_revisions: BTreeSet<u64>,
}

#[derive(Debug, Clone)]
pub(super) struct DeploymentState {
    pub(super) node_id: String,
    pub(super) head_revision: u64,
    pub(super) epoch: u64,
    pub(super) artifacts: BTreeMap<String, ArtifactObjectRow>,
    pub(super) packages: BTreeMap<String, ProviderPackageRow>,
    pub(super) publisher_bindings: BTreeMap<(String, String, String), String>,
    pub(super) admissions: BTreeMap<String, ProviderAdmissionRow>,
    pub(super) connections: BTreeMap<(String, u64), ConnectionRevisionRow>,
    pub(super) revisions: BTreeMap<u64, DeploymentRevisionRow>,
    pub(super) receipts: BTreeMap<(String, String), DeploymentReceipt>,
    pub(super) active_node_revisions: BTreeMap<String, BTreeSet<u64>>,
    pub(super) dirty: DeploymentDirtySet,
}

impl Default for DeploymentState {
    fn default() -> Self {
        Self {
            node_id: "node-local-1".to_string(),
            head_revision: 0,
            epoch: 0,
            artifacts: BTreeMap::new(),
            packages: BTreeMap::new(),
            publisher_bindings: BTreeMap::new(),
            admissions: BTreeMap::new(),
            connections: BTreeMap::new(),
            revisions: BTreeMap::new(),
            receipts: BTreeMap::new(),
            active_node_revisions: BTreeMap::new(),
            dirty: DeploymentDirtySet::default(),
        }
    }
}

/// Transactional immutable deployment and artifact store (`V2-03`, `V2-04`).
#[derive(Clone)]
pub struct ImmutableDeploymentStore {
    pub(super) inner: Arc<Mutex<DeploymentState>>,
    pub(super) write_lock: Arc<tokio::sync::Mutex<()>>,
}

impl Default for ImmutableDeploymentStore {
    fn default() -> Self {
        Self::new("node-local-1")
    }
}

/// A rollback request: re-deploy `target_revision`'s manifest as a new
/// revision on top of `expected_head_revision`.
#[derive(Debug, Clone, Copy)]
pub struct Rollback<'a> {
    pub principal: &'a str,
    pub idempotency_key: &'a str,
    pub expected_head_revision: u64,
    pub target_revision: u64,
    pub reason: &'a str,
}

impl ImmutableDeploymentStore {
    pub fn new(node_id: impl Into<String>) -> Self {
        let state = DeploymentState {
            node_id: node_id.into(),
            ..Default::default()
        };
        Self {
            inner: Arc::new(Mutex::new(state)),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub fn node_id(&self) -> String {
        let g = self.inner.lock().expect("store lock");
        g.node_id.clone()
    }

    pub fn set_node_id(&self, node_id: impl Into<String>) {
        let mut g = self.inner.lock().expect("store lock");
        g.node_id = node_id.into();
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

    pub fn get_artifact(&self, digest: &str) -> Option<ArtifactObjectRow> {
        let g = self.inner.lock().expect("store lock");
        g.artifacts.get(digest).cloned()
    }

    /// The package stored under `package_digest`.
    pub fn get_package(&self, package_digest: &str) -> Option<ProviderPackageRow> {
        let g = self.inner.lock().expect("store lock");
        g.packages.get(package_digest).cloned()
    }

    pub fn get_admission(&self, package_digest: &str) -> Option<ProviderAdmissionRow> {
        let g = self.inner.lock().expect("store lock");
        g.admissions.get(package_digest).cloned()
    }

    pub fn get_connection(
        &self,
        connection_id: &str,
        revision: u64,
    ) -> Option<ConnectionRevisionRow> {
        let g = self.inner.lock().expect("store lock");
        g.connections
            .get(&(connection_id.to_string(), revision))
            .cloned()
    }

    /// Resolve the active connection revision for `connection_id`: when a
    /// deployment revision is active (`head_revision > 0`) and declares a
    /// `Connection` member for `connection_id`, return that exact revision;
    /// otherwise return the highest stored revision for `connection_id`.
    pub fn get_active_connection(&self, connection_id: &str) -> Option<ConnectionRevisionRow> {
        let g = self.inner.lock().expect("store lock");
        if g.head_revision > 0
            && let Some(rev_row) = g.revisions.get(&g.head_revision)
        {
            for member in &rev_row.manifest.members {
                if member.entity.kind == EntityKind::Connection && member.entity.id == connection_id
                {
                    return g
                        .connections
                        .get(&(connection_id.to_string(), member.entity.revision))
                        .cloned();
                }
            }
        }
        g.connections
            .values()
            .filter(|c| c.connection_id == connection_id)
            .max_by_key(|c| c.revision)
            .cloned()
    }

    /// V2-04.1: Ingest a bounded archive, rejecting symlinks, path traversal,
    /// decompression ratio bombs (> 20x), duplicate entries, and inventory
    /// mismatches. Upload creates no serving routes or active generations.
    pub fn ingest_verified_archive(
        &self,
        entries: &[ArchiveEntryInput],
    ) -> Result<ProviderPackageRow, DeploymentError> {
        let (id_key, package_digest, row, raw_inv, hashed_payloads) =
            Self::prepare_archive_ingestion(entries)?;
        let mut g = self.inner.lock().expect("store lock");
        Self::finish_ingest_verified_archive(
            &mut g,
            id_key,
            package_digest,
            row,
            raw_inv,
            hashed_payloads,
        )
    }

    #[allow(clippy::type_complexity)]
    pub(super) fn prepare_archive_ingestion(
        entries: &[ArchiveEntryInput],
    ) -> Result<
        (
            (String, String, String),
            String,
            ProviderPackageRow,
            Vec<u8>,
            Vec<(String, Vec<u8>)>,
        ),
        DeploymentError,
    > {
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
        let provider_id = manifest_json["id"]
            .as_str()
            .ok_or_else(|| DeploymentError::new("MANIFEST_SHAPE_INVALID", "id"))?
            .to_string();
        let version = manifest_json["version"]
            .as_str()
            .ok_or_else(|| DeploymentError::new("MANIFEST_SHAPE_INVALID", "version"))?
            .to_string();
        let impl_kind = match manifest_json["implementation"]["kind"]
            .as_str()
            .unwrap_or("rust")
        {
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

        let hashed_payloads: Vec<(String, Vec<u8>)> = payloads
            .into_values()
            .map(|bytes| {
                let digest = sha256_digest(&bytes);
                (digest, bytes)
            })
            .collect();

        Ok((
            (publisher, provider_id, version),
            package_digest,
            row,
            raw_inv,
            hashed_payloads,
        ))
    }

    pub(super) fn finish_ingest_verified_archive(
        g: &mut DeploymentState,
        id_key: (String, String, String),
        package_digest: String,
        row: ProviderPackageRow,
        raw_inv: Vec<u8>,
        hashed_payloads: Vec<(String, Vec<u8>)>,
    ) -> Result<ProviderPackageRow, DeploymentError> {
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
        for (obj_digest, bytes) in hashed_payloads {
            let size_bytes = bytes.len();
            g.dirty.artifacts.insert(obj_digest.clone());
            g.artifacts
                .entry(obj_digest.clone())
                .or_insert_with(|| ArtifactObjectRow {
                    digest: obj_digest,
                    size_bytes,
                    state: ArtifactState::Active,
                    drain_epoch: None,
                    bytes: Some(bytes),
                });
        }
        g.dirty.artifacts.insert(package_digest.clone());
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
        g.dirty.packages.insert(package_digest.clone());
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
        Self::admit_package_into_state(&mut g, package_digest, policy, lock)
    }

    pub(super) fn admit_package_into_state(
        g: &mut DeploymentState,
        package_digest: &str,
        policy: &dyn AdmissionPolicy,
        lock: Option<&Value>,
    ) -> Result<ProviderAdmissionRow, DeploymentError> {
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
                    .expect("validated manifest operations object")
                    .keys()
                    .cloned()
                    .collect();
                ops.sort();
                let impl_str = p.manifest_json["implementation"]["kind"]
                    .as_str()
                    .expect("validated manifest implementation kind")
                    .to_string();
                let import_profile = if impl_str == "wasm" {
                    match p.manifest_json["implementation"]["world"]
                        .as_str()
                        .expect("validated wasm world string")
                    {
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

        // A package states its tier, or implies Enterprise by declaring policy
        // requirements; a tier it names that is not one is refused rather
        // than read as Community.
        let tier = match pkg.distribution_json.get("tier") {
            Some(Value::String(name)) => EditionTier::parse(name).ok_or_else(|| {
                DeploymentError::new(
                    "PACKAGE_INVALID_TIER",
                    format!("package declares tier '{name}'; expected 'community' or 'enterprise'"),
                )
            })?,
            Some(other) => {
                return Err(DeploymentError::new(
                    "PACKAGE_INVALID_TIER",
                    format!("package tier must be a string, got {other}"),
                ));
            }
            None if pkg
                .distribution_json
                .get("policy_requirements")
                .and_then(Value::as_array)
                .is_some_and(|reqs| !reqs.is_empty()) =>
            {
                EditionTier::Enterprise
            }
            None => EditionTier::Community,
        };

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
        let fp = sha256_digest(
            &canonical_bytes(&fp_doc).expect("canonical fingerprint bytes serialize"),
        );
        let admission = ProviderAdmissionRow {
            package_digest: pkg.package_digest,
            admission_fingerprint: fp,
            policy_id: verdict.policy_id,
            revoked: false,
        };
        g.dirty.admissions.insert(admission.package_digest.clone());
        g.admissions
            .insert(admission.package_digest.clone(), admission.clone());
        Ok(admission)
    }

    pub fn revoke_package(&self, package_digest: &str) -> Result<(), DeploymentError> {
        let mut g = self.inner.lock().expect("store lock");
        Self::revoke_package_into_state(&mut g, package_digest)
    }

    pub(super) fn revoke_package_into_state(
        g: &mut DeploymentState,
        package_digest: &str,
    ) -> Result<(), DeploymentError> {
        let Some(adm) = g.admissions.get_mut(package_digest) else {
            return Err(DeploymentError::new("ADMISSION_NOT_FOUND", package_digest));
        };
        adm.revoked = true;
        g.dirty.admissions.insert(package_digest.to_string());
        // A revocation moves the epoch, so a peer polling the head sees it
        // and re-reads the store, as it does for a new revision.
        g.epoch += 1;
        g.dirty.head = true;
        Ok(())
    }

    /// Store an immutable definition artifact blob (e.g. workflow or channel
    /// definition bytes) as an `ACTIVE` content-addressed artifact (`TD-19`).
    pub fn store_definition_artifact(
        &self,
        bytes: Vec<u8>,
    ) -> Result<ArtifactObjectRow, DeploymentError> {
        let digest = sha256_digest(&bytes);
        let size_bytes = bytes.len();
        let row = ArtifactObjectRow {
            digest: digest.clone(),
            size_bytes,
            state: ArtifactState::Active,
            drain_epoch: None,
            bytes: Some(bytes),
        };
        let mut g = self.inner.lock().expect("store lock");
        if let Some(existing) = g.artifacts.get(&digest)
            && existing.state != ArtifactState::Active
        {
            return Err(DeploymentError::new(
                "ARTIFACT_NOT_ACTIVE",
                format!("definition artifact '{digest}' is not ACTIVE"),
            ));
        }
        g.dirty.artifacts.insert(digest.clone());
        g.artifacts.insert(digest, row.clone());
        Ok(row)
    }

    /// V2-03.2: Store an immutable connection revision with versioned secret
    /// locator and redacted public configuration digest (`TD-20`).
    pub fn store_connection_revision(
        &self,
        connection_id: &str,
        revision: u64,
        provider_digest: &str,
        public_config: Value,
        secret_locator: Option<String>,
    ) -> Result<ConnectionRevisionRow, DeploymentError> {
        let (row, cfg_digest, cfg_bytes) = Self::prepare_connection_revision(
            connection_id,
            revision,
            provider_digest,
            public_config,
            secret_locator,
        )?;
        let mut g = self.inner.lock().expect("store lock");
        Self::finish_store_connection_revision(&mut g, row, cfg_digest, cfg_bytes)
    }

    pub(super) fn prepare_connection_revision(
        connection_id: &str,
        revision: u64,
        provider_digest: &str,
        public_config: Value,
        secret_locator: Option<String>,
    ) -> Result<(ConnectionRevisionRow, String, Vec<u8>), DeploymentError> {
        if revision == 0 {
            return Err(DeploymentError::new(
                "CONNECTION_REVISION_INVALID",
                "revision must be >= 1",
            ));
        }
        // Reject raw secret keys or credential-bearing URLs at any nesting depth
        validate_no_secrets_in_public_config(&public_config)?;

        let cfg_bytes = canonical_bytes(&public_config).map_err(|e| {
            DeploymentError::new(
                "CONNECTION_CONFIG_INVALID",
                format!("{connection_id}: {}", e.code),
            )
        })?;
        let cfg_digest = sha256_digest(&cfg_bytes);
        let row = ConnectionRevisionRow {
            connection_id: connection_id.to_string(),
            revision,
            provider_digest: provider_digest.to_string(),
            public_config,
            secret_locator,
            configuration_digest: cfg_digest.clone(),
        };
        Ok((row, cfg_digest, cfg_bytes))
    }

    pub(super) fn finish_store_connection_revision(
        g: &mut DeploymentState,
        row: ConnectionRevisionRow,
        cfg_digest: String,
        cfg_bytes: Vec<u8>,
    ) -> Result<ConnectionRevisionRow, DeploymentError> {
        let key = (row.connection_id.clone(), row.revision);
        if let Some(existing) = g.connections.get(&key) {
            if existing != &row {
                return Err(DeploymentError::new(
                    "IMMUTABLE_CONNECTION_CONFLICT",
                    "immutable connection revision cannot be modified",
                ));
            }
            return Ok(existing.clone());
        }
        g.dirty.artifacts.insert(cfg_digest.clone());
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
        g.dirty.connections.insert(key.clone());
        g.connections.insert(key, row.clone());
        Ok(row)
    }

    /// Validate that all provider metadata, connection revisions, and member
    /// definition artifacts referenced by `manifest` are consistent and active (`TD-19`).
    fn verify_manifest_store_closure(
        g: &DeploymentState,
        manifest: &CanonicalManifest,
        missing_artifact_code: &str,
    ) -> Result<(), DeploymentError> {
        for p in &manifest.providers {
            let Some(adm) = g.admissions.get(&p.digest) else {
                return Err(DeploymentError::new(
                    "PROVIDER_NOT_ADMITTED",
                    format!(
                        "provider '{}' ({}) is not admitted",
                        p.provider_id, p.digest
                    ),
                ));
            };
            if adm.revoked {
                return Err(DeploymentError::new(
                    "PROVIDER_REVOKED",
                    format!("provider '{}' ({}) is revoked", p.provider_id, p.digest),
                ));
            }
            if let Some(pkg) = g.packages.get(&p.digest)
                && (pkg.publisher != p.publisher
                    || pkg.provider_id != p.provider_id
                    || pkg.version != p.version)
            {
                return Err(DeploymentError::new(
                    "PROVIDER_METADATA_MISMATCH",
                    format!(
                        "provider ref ({}/{}/{}) does not match stored package metadata ({}/{}/{}) for '{}'",
                        p.publisher,
                        p.provider_id,
                        p.version,
                        pkg.publisher,
                        pkg.provider_id,
                        pkg.version,
                        p.digest
                    ),
                ));
            }
            if g.artifacts
                .get(&p.digest)
                .is_none_or(|a| a.state != ArtifactState::Active)
            {
                return Err(DeploymentError::new(
                    missing_artifact_code,
                    format!("provider artifact '{}' is not ACTIVE", p.digest),
                ));
            }
        }

        let declared_provider_digests: HashSet<&str> = manifest
            .providers
            .iter()
            .map(|p| p.digest.as_str())
            .collect();

        for m in &manifest.members {
            if let Some(art) = g.artifacts.get(&m.definition_digest)
                && art.state != ArtifactState::Active
            {
                return Err(DeploymentError::new(
                    missing_artifact_code,
                    format!("member artifact '{}' is not ACTIVE", m.definition_digest),
                ));
            }
            if let Some(ref owning_pkg) = m.owning_package {
                if !declared_provider_digests.contains(owning_pkg.as_str()) {
                    return Err(DeploymentError::new(
                        "OWNING_PACKAGE_NOT_DECLARED",
                        format!(
                            "member '{}' references owning_package '{}' not declared in manifest.providers",
                            m.entity.id, owning_pkg
                        ),
                    ));
                }
                if let Some(pkg) = g.packages.get(owning_pkg) {
                    let pkg_digests = extract_inventory_file_digests(&pkg.inventory_json);
                    if !pkg_digests.is_empty() && !pkg_digests.contains(&m.definition_digest) {
                        return Err(DeploymentError::new(
                            "MEMBER_ARTIFACT_NOT_IN_PACKAGE",
                            format!(
                                "member '{}' definition_digest '{}' is not in owning_package '{}' inventory",
                                m.entity.id, m.definition_digest, owning_pkg
                            ),
                        ));
                    }
                }
            }
            if m.entity.kind == EntityKind::Connection {
                let Some(conn_row) = g.connections.get(&(m.entity.id.clone(), m.entity.revision))
                else {
                    return Err(DeploymentError::new(
                        "CONNECTION_REVISION_NOT_FOUND",
                        format!(
                            "connection '{}' revision {} is not in store",
                            m.entity.id, m.entity.revision
                        ),
                    ));
                };
                if conn_row.configuration_digest != m.definition_digest {
                    return Err(DeploymentError::new(
                        "CONNECTION_DIGEST_MISMATCH",
                        format!(
                            "connection '{}' revision {} configuration_digest '{}' does not match manifest definition_digest '{}'",
                            m.entity.id,
                            m.entity.revision,
                            conn_row.configuration_digest,
                            m.definition_digest
                        ),
                    ));
                }
                if !declared_provider_digests.contains(conn_row.provider_digest.as_str()) {
                    return Err(DeploymentError::new(
                        "CONNECTION_PROVIDER_NOT_DECLARED",
                        format!(
                            "connection '{}' revision {} references provider '{}' not declared in manifest.providers",
                            m.entity.id, m.entity.revision, conn_row.provider_digest
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    /// V2-04.3: Build a `CandidatePlan` outside any write transaction, checking
    /// base revision, manifest closure, and computing added/removed diffs.
    pub fn plan_candidate(
        &self,
        base_revision: u64,
        candidate_manifest: CanonicalManifest,
    ) -> Result<CandidatePlan, DeploymentError> {
        let g = self.inner.lock().expect("store lock");
        Self::plan_candidate_on_state(&g, base_revision, candidate_manifest)
    }

    pub(super) fn plan_candidate_on_state(
        g: &DeploymentState,
        base_revision: u64,
        candidate_manifest: CanonicalManifest,
    ) -> Result<CandidatePlan, DeploymentError> {
        let (candidate_bytes, candidate_digest) = candidate_manifest.canonicalize_and_verify()?;

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

        Self::verify_manifest_store_closure(g, &candidate_manifest, "ARTIFACT_NOT_ACTIVE")?;

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
        let mut g = self.inner.lock().expect("store lock");
        Self::apply_candidate_into_state(&mut g, principal, idempotency_key, plan, reason, policy)
    }

    pub(super) fn apply_candidate_into_state(
        g: &mut DeploymentState,
        principal: &str,
        idempotency_key: &str,
        plan: &CandidatePlan,
        reason: &str,
        policy: &dyn AdmissionPolicy,
    ) -> Result<DeploymentReceipt, DeploymentError> {
        let request_hash = plan.compute_request_hash("apply", reason)?;

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
                .check_runtime_lease(
                    0,
                    &p.provider_id,
                    "",
                    super::contract::EffectStatus::NotStarted,
                )
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
        }
        Self::verify_manifest_store_closure(
            g,
            &plan.candidate_manifest,
            "ARTIFACT_DRAINING_OR_DELETED",
        )?;

        // Ensure all member definition digests are registered in `g.artifacts` so
        // two-phase GC tracks their lifecycle (`TD-19`).
        for m in &plan.candidate_manifest.members {
            if !g.artifacts.contains_key(&m.definition_digest) {
                g.dirty.artifacts.insert(m.definition_digest.clone());
                g.artifacts.insert(
                    m.definition_digest.clone(),
                    ArtifactObjectRow {
                        digest: m.definition_digest.clone(),
                        size_bytes: 0,
                        state: ArtifactState::Active,
                        drain_epoch: None,
                        bytes: None,
                    },
                );
            }
        }

        // 4. Advance head_revision and epoch atomically and persist receipt
        let prev_head = g.head_revision;
        let next_revision = g.head_revision + 1;
        let next_epoch = g.epoch + 1;
        if !g.dirty.head {
            g.dirty.expected_head_revision = Some(prev_head);
        }
        g.head_revision = next_revision;
        g.epoch = next_epoch;
        g.dirty.head = true;
        g.dirty.revisions.insert(next_revision);
        g.dirty.receipts.insert(op_key.clone());

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
            receiving_node_id: g.node_id.clone(),
        };
        g.receipts.insert(op_key, receipt.clone());
        Ok(receipt)
    }

    /// V2-04.5: Plan and apply a rollback to `target_revision` as a NEW
    /// monotonic revision on top of `expected_head_revision`, re-verifying
    /// current compatibility, admissions, and revocations.
    pub fn rollback_to_revision(
        &self,
        rollback: &Rollback<'_>,
        policy: &dyn AdmissionPolicy,
    ) -> Result<(DeploymentReceipt, Vec<String>), DeploymentError> {
        let mut g = self.inner.lock().expect("store lock");
        Self::rollback_to_revision_into_state(&mut g, rollback, policy)
    }

    pub(super) fn rollback_to_revision_into_state(
        g: &mut DeploymentState,
        rollback: &Rollback<'_>,
        policy: &dyn AdmissionPolicy,
    ) -> Result<(DeploymentReceipt, Vec<String>), DeploymentError> {
        let &Rollback {
            principal,
            idempotency_key,
            expected_head_revision,
            target_revision,
            reason,
        } = rollback;
        let target_manifest = {
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

        let mut plan = Self::plan_candidate_on_state(g, expected_head_revision, target_manifest)?;
        plan.external_effect_warnings.push(
            "Configuration rollback creates a new deployment revision; already-emitted external side effects are not reversed."
                .to_string(),
        );
        let warnings = plan.external_effect_warnings.clone();
        let receipt =
            Self::apply_candidate_into_state(g, principal, idempotency_key, &plan, reason, policy)?;
        Ok((receipt, warnings))
    }

    /// Record the revisions currently held (applied + retained in-flight) by a node.
    pub fn update_node_retained_revisions(&self, node_id: &str, revisions: BTreeSet<u64>) {
        let mut g = self.inner.lock().expect("store lock");
        g.active_node_revisions
            .insert(node_id.to_string(), revisions);
    }

    /// V2-04.6: Two-phase epoch-fenced garbage collection (`ACTIVE -> DRAINING -> DELETED`)
    /// and intermediate `deployment_members` compaction (`TD-22`, `TD-23`).
    ///
    /// Protects:
    /// - Current `deployment_head`
    /// - Recent rollback window (`retain_recent_revisions`)
    /// - Any revision held in `active_node_revisions`
    /// - Newly staged (never yet deployed) non-revoked packages, connections, and payload files
    /// - All constituent payload file digests of protected packages
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

        // Track all digests ever referenced by any committed revision so newly
        // staged packages/connections (never yet deployed) are not prematurely collected.
        let mut ever_deployed_digests: HashSet<String> = HashSet::new();
        for rev in g.revisions.values() {
            if let Some(ref lock_digest) = rev.manifest.provider_lock_digest {
                ever_deployed_digests.insert(lock_digest.clone());
            }
            for p in &rev.manifest.providers {
                ever_deployed_digests.insert(p.digest.clone());
                if let Some(pkg) = g.packages.get(&p.digest) {
                    for fd in extract_inventory_file_digests(&pkg.inventory_json) {
                        ever_deployed_digests.insert(fd);
                    }
                }
            }
            for m in &rev.manifest.members {
                ever_deployed_digests.insert(m.definition_digest.clone());
            }
        }

        let mut protected_digests: HashSet<String> = HashSet::new();
        for rev_id in &protected_revisions {
            if let Some(rev) = g.revisions.get(rev_id) {
                if let Some(ref lock_digest) = rev.manifest.provider_lock_digest {
                    protected_digests.insert(lock_digest.clone());
                }
                for p in &rev.manifest.providers {
                    protected_digests.insert(p.digest.clone());
                    if let Some(pkg) = g.packages.get(&p.digest) {
                        for fd in extract_inventory_file_digests(&pkg.inventory_json) {
                            protected_digests.insert(fd);
                        }
                    }
                }
                for m in &rev.manifest.members {
                    protected_digests.insert(m.definition_digest.clone());
                    if let Some(ref owning_pkg) = m.owning_package {
                        protected_digests.insert(owning_pkg.clone());
                        if let Some(pkg) = g.packages.get(owning_pkg) {
                            for fd in extract_inventory_file_digests(&pkg.inventory_json) {
                                protected_digests.insert(fd);
                            }
                        }
                    }
                }
            }
        }

        // Protect newly staged packages (uploaded/admitted and never yet deployed nor revoked)
        // along with their payload file digests, and newly staged connection revisions.
        for (pkg_digest, pkg) in &g.packages {
            let is_revoked = g.admissions.get(pkg_digest).is_some_and(|a| a.revoked);
            if !ever_deployed_digests.contains(pkg_digest) && !is_revoked {
                protected_digests.insert(pkg_digest.clone());
                for fd in extract_inventory_file_digests(&pkg.inventory_json) {
                    protected_digests.insert(fd);
                }
            }
        }
        for conn in g.connections.values() {
            if !ever_deployed_digests.contains(&conn.configuration_digest) {
                protected_digests.insert(conn.configuration_digest.clone());
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
        for rev_id in &compacted_revisions {
            g.dirty.revisions.insert(*rev_id);
            g.dirty.compacted_member_revisions.insert(*rev_id);
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
                    if art.drain_epoch.is_none_or(|de| current_epoch > de) {
                        art.state = ArtifactState::Deleted;
                        art.bytes = None;
                        newly_deleted.push(digest.clone());
                    }
                }
                ArtifactState::Deleted => {}
            }
        }
        for d in newly_draining.iter().chain(newly_deleted.iter()) {
            g.dirty.artifacts.insert(d.clone());
        }
        if !newly_draining.is_empty()
            || !newly_deleted.is_empty()
            || !compacted_revisions.is_empty()
        {
            g.epoch += 1;
            g.dirty.head = true;
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
            let cfg_bytes = canonical_bytes(cfg).map_err(|e| {
                DeploymentError::new(
                    "CONNECTION_CONFIG_INVALID",
                    format!("{conn_id}: {}", e.code),
                )
            })?;
            members.push(ManifestMember {
                entity: EntityRef {
                    kind: EntityKind::Connection,
                    id: (*conn_id).to_string(),
                    revision: 1,
                },
                definition_digest: sha256_digest(&cfg_bytes),
                owning_package: None,
                requires: Vec::new(),
                rollouts: Vec::new(),
            });
        }

        for (wf_id, rev, wf_json) in legacy_workflows {
            let wf_bytes = canonical_bytes(wf_json).map_err(|e| {
                DeploymentError::new("WORKFLOW_CONFIG_INVALID", format!("{wf_id}: {}", e.code))
            })?;
            members.push(ManifestMember {
                entity: EntityRef {
                    kind: EntityKind::Workflow,
                    id: (*wf_id).to_string(),
                    revision: *rev,
                },
                definition_digest: sha256_digest(&wf_bytes),
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
                definition_digest: sha256_digest(
                    format!("{ch_id}:{ch_rev}:{wf_id}:{wf_rev}").as_bytes(),
                ),
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
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests;
