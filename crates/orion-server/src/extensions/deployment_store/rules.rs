//! What the store decides, on values: an archive's admissibility, a
//! connection revision's shape, a package's admission, a manifest's closure
//! over what is stored, a plan's diff, and one collection pass. The
//! transactions in [`super::writes`] read the facts these need from the
//! database and write what they decide; nothing here touches a database.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde_json::{Value, json};

use super::error::DeploymentErrorKind as Kind;
use super::{
    ArchiveEntryInput, ArtifactState, CandidatePlan, CanonicalManifest, ConnectionRevisionRow,
    DeploymentError, EntityKind, EntityRef, GcOutcome, ImplementationKind, ProviderAdmissionRow,
    ProviderPackageRow, extract_inventory_file_digests,
};
use crate::extensions::admission::{Admission, EditionTier, PackageAdmissionRequest};
use crate::extensions::identity::{ConnectionId, Digest, ProviderId, Revision};
use crate::extensions::package_identity::{
    VerifiedArtifactMetadata, canonical_bytes, check_dependency_lock, sha256_digest,
    verify_inventory,
};
use crate::extensions::schema_profile::{
    CompiledSchemaBundle, check_manifest_bundle, validate_safe_package_path,
};

// ============================================================
// Archive ingestion
// ============================================================

/// A verified archive, ready to store: its package row, its inventory bytes
/// (stored under the package digest), every other file by digest, and the
/// schema bundle its manifest compiles to.
pub(super) struct PreparedPackage {
    pub row: ProviderPackageRow,
    pub inventory_bytes: Vec<u8>,
    pub payloads: Vec<(Digest, Vec<u8>)>,
    pub bundle: CompiledSchemaBundle,
}

/// V2-04.1: refuse symlinks, path traversal, decompression ratio bombs
/// (> 20x), duplicate entries and inventory mismatches; read the package's
/// identity from its manifest and distribution.
pub(super) fn prepare_archive(
    entries: &[ArchiveEntryInput],
) -> Result<PreparedPackage, DeploymentError> {
    if entries.is_empty() || entries.len() > 1_024 {
        return Err(DeploymentError::new(
            Kind::ArchiveEntryCountExceeded,
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
                Kind::ArchiveSymlinkDenied,
                format!("symlink entry '{}' is forbidden", entry.path),
            ));
        }
        validate_safe_package_path(&entry.path, "ARCHIVE_PATH_INVALID")
            .map_err(|f| DeploymentError::from_code(f.code, f.location))?;
        if !seen_paths.insert(entry.path.to_ascii_lowercase()) {
            return Err(DeploymentError::new(
                Kind::ArchiveDuplicateEntry,
                format!("duplicate or case-colliding archive path '{}'", entry.path),
            ));
        }
        if entry.compressed_size > 0 && entry.bytes.len() / entry.compressed_size.max(1) > 20 {
            return Err(DeploymentError::new(
                Kind::ArchiveCompressionRatioExceeded,
                format!("decompression ratio exceeded on '{}'", entry.path),
            ));
        }
        total_uncompressed += entry.bytes.len();
        if entry.bytes.len() > 16 * 1024 * 1024 || total_uncompressed > 64 * 1024 * 1024 {
            return Err(DeploymentError::new(
                Kind::ArchiveSizeLimitExceeded,
                "archive size limit exceeded",
            ));
        }
        if entry.path == "inventory.json" {
            raw_inventory = Some(entry.bytes.clone());
        } else if !entry.path.starts_with("signatures/") {
            payloads.insert(entry.path.clone(), entry.bytes.clone());
        }
    }

    let inventory_bytes = raw_inventory.ok_or_else(|| {
        DeploymentError::new(Kind::ArchiveInventoryMissing, "missing root inventory.json")
    })?;
    let inventory_json: Value = serde_json::from_slice(&inventory_bytes).map_err(|_| {
        DeploymentError::new(Kind::ArchiveInventoryInvalid, "invalid inventory.json")
    })?;
    let package_digest = verify_inventory(&inventory_json, &payloads, Some(&inventory_bytes))
        .map_err(|f| DeploymentError::from_code(f.code, "inventory verification failed"))?;

    let manifest_json: Value = payloads
        .get("connector.json")
        .and_then(|bytes| serde_json::from_slice(bytes).ok())
        .ok_or_else(|| DeploymentError::new(Kind::ManifestInvalidJson, "connector.json"))?;
    let distribution_json: Value = payloads
        .get("distribution.json")
        .and_then(|bytes| serde_json::from_slice(bytes).ok())
        .ok_or_else(|| DeploymentError::new(Kind::DistributionInvalidJson, "distribution.json"))?;

    let file_list: Vec<String> = payloads.keys().cloned().collect();
    let mut schemas = BTreeMap::new();
    for (path, bytes) in &payloads {
        if is_schema_file(path) {
            let val: Value = serde_json::from_slice(bytes)
                .map_err(|_| DeploymentError::new(Kind::SchemaInvalidJson, path.clone()))?;
            schemas.insert(path.clone(), val);
        }
    }
    let bundle = check_manifest_bundle(&manifest_json, &file_list, &schemas)
        .map_err(|f| DeploymentError::from_code(f.code, f.location))?;

    let publisher = distribution_json["publisher"]
        .as_str()
        .ok_or_else(|| DeploymentError::new(Kind::DistributionShapeInvalid, "publisher"))?
        .to_string();
    let provider_id = manifest_json["id"]
        .as_str()
        .ok_or_else(|| DeploymentError::new(Kind::ManifestShapeInvalid, "id"))?
        .to_string();
    let version = manifest_json["version"]
        .as_str()
        .ok_or_else(|| DeploymentError::new(Kind::ManifestShapeInvalid, "version"))?
        .to_string();
    let implementation = match manifest_json["implementation"]["kind"]
        .as_str()
        .unwrap_or("rust")
    {
        "workflow" => ImplementationKind::Workflow,
        "wasm" => ImplementationKind::Wasm,
        _ => ImplementationKind::Rust,
    };

    Ok(PreparedPackage {
        row: ProviderPackageRow {
            package_digest,
            publisher,
            provider_id: ProviderId::from(provider_id),
            version,
            implementation,
            manifest_json,
            distribution_json,
            inventory_json,
        },
        inventory_bytes,
        payloads: payloads
            .into_values()
            .map(|bytes| (sha256_digest(&bytes), bytes))
            .collect(),
        bundle,
    })
}

/// Whether a package file is one of its JSON schemas.
pub(super) fn is_schema_file(path: &str) -> bool {
    path.starts_with("schemas/") && path.ends_with(".json")
}

/// Each file a package's inventory lists: `(path, sha256:<hex>)`.
pub(super) fn inventory_files(inventory: &Value) -> Vec<(String, String)> {
    inventory["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let path = entry["path"].as_str()?;
            let sha = entry["sha256"].as_str()?;
            Some((path.to_string(), format!("sha256:{sha}")))
        })
        .collect()
}

/// The schema bundle a stored package's manifest compiles to, from the
/// schema files `files` holds by digest.
pub(super) fn compile_bundle(
    package: &ProviderPackageRow,
    files: &HashMap<String, Vec<u8>>,
) -> Result<CompiledSchemaBundle, DeploymentError> {
    let listed = inventory_files(&package.inventory_json);
    let paths: Vec<String> = listed.iter().map(|(p, _)| p.clone()).collect();
    let mut schemas = BTreeMap::new();
    for (path, digest) in &listed {
        if !is_schema_file(path) {
            continue;
        }
        let bytes = files.get(digest).ok_or_else(|| {
            DeploymentError::corrupt(format!(
                "package {} lists '{path}', which is not stored",
                package.package_digest
            ))
        })?;
        let schema: Value = serde_json::from_slice(bytes)
            .map_err(|_| DeploymentError::new(Kind::SchemaInvalidJson, path.clone()))?;
        schemas.insert(path.clone(), schema);
    }
    check_manifest_bundle(&package.manifest_json, &paths, &schemas)
        .map_err(|f| DeploymentError::from_code(f.code, f.location))
}

// ============================================================
// Connection revisions
// ============================================================

/// A connection revision, checked, with its configuration digest and the
/// bytes that digest is taken over.
pub(super) struct PreparedConnection {
    pub row: ConnectionRevisionRow,
    pub config_bytes: Vec<u8>,
}

pub(super) fn prepare_connection(
    connection_id: &str,
    revision: u64,
    provider_digest: &str,
    public_config: Value,
    secret_locators: BTreeMap<String, String>,
    bindings: BTreeMap<String, String>,
) -> Result<PreparedConnection, DeploymentError> {
    let Some(revision) = Revision::new(revision) else {
        return Err(DeploymentError::new(
            Kind::ConnectionRevisionInvalid,
            "revision must be >= 1",
        ));
    };
    let provider_digest = Digest::parse(provider_digest).map_err(|e| {
        DeploymentError::new(
            Kind::ConnectionRevisionInvalid,
            format!("provider_digest: {e}"),
        )
    })?;
    super::check_connection_config(&public_config, &secret_locators)?;
    let config_bytes = super::connection_config_bytes(&public_config, &secret_locators, &bindings);
    let configuration_digest = sha256_digest(&config_bytes);
    Ok(PreparedConnection {
        row: ConnectionRevisionRow {
            connection_id: ConnectionId::from(connection_id),
            revision,
            provider_digest,
            public_config,
            secret_locators,
            bindings,
            configuration_digest,
        },
        config_bytes,
    })
}

// ============================================================
// Admission
// ============================================================

/// What a dependency lock is checked against: every stored package's
/// identity, contract and operations.
pub(super) fn verified_metadata(
    packages: &[ProviderPackageRow],
) -> BTreeMap<(String, String), VerifiedArtifactMetadata> {
    let mut verified = BTreeMap::new();
    for p in packages {
        let mut operations: Vec<String> = p.manifest_json["operations"]
            .as_object()
            .map(|ops| ops.keys().cloned().collect())
            .unwrap_or_default();
        operations.sort();
        let implementation = p.manifest_json["implementation"]["kind"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let import_profile = if implementation == "wasm" {
            match p.manifest_json["implementation"]["world"]
                .as_str()
                .unwrap_or_default()
            {
                "orion:connector/connector@1.0.0" => Some("core-v1".to_string()),
                "orion:connector/connector-http@1.0.0" => Some("http-v1".to_string()),
                "orion:connector/connector-composition@1.0.0" => Some("composition-v1".to_string()),
                "orion:connector/connector-sessions@1.0.0" => Some("sessions-v1".to_string()),
                _ => None,
            }
        } else {
            None
        };
        verified.insert(
            ("provider".to_string(), p.package_digest.to_string()),
            VerifiedArtifactMetadata {
                publisher: p.publisher.clone(),
                id: p.provider_id.to_string(),
                version: p.version.clone(),
                contract: "orion:connector@1.0.0".to_string(),
                schema_profile: Some("orion-json-schema/1".to_string()),
                implementation: Some(implementation),
                import_profile,
                operations,
                functions: Vec::new(),
                dependencies: Vec::new(),
            },
        );
    }
    verified
}

/// V2-04.2: admit `pkg` under `policy`, after checking `lock` (when given)
/// against `all_packages`: the admission row, with its fingerprint over the
/// runtime, schema profile, package and policy.
pub(super) fn admit(
    pkg: &ProviderPackageRow,
    policy: &Admission,
    lock: Option<(&Value, &[ProviderPackageRow])>,
) -> Result<ProviderAdmissionRow, DeploymentError> {
    if let Some((lock_doc, all_packages)) = lock {
        check_dependency_lock(lock_doc, &verified_metadata(all_packages))
            .map_err(|f| DeploymentError::from_code(f.code, "dependency lock check failed"))?;
    }

    // A package states its tier, or implies Enterprise by declaring policy
    // requirements; a tier it names that is not one is refused rather than
    // read as Community.
    let tier = match pkg.distribution_json.get("tier") {
        Some(Value::String(name)) => EditionTier::parse(name).ok_or_else(|| {
            DeploymentError::new(
                Kind::PackageInvalidTier,
                format!("package declares tier '{name}'; expected 'community' or 'enterprise'"),
            )
        })?,
        Some(other) => {
            return Err(DeploymentError::new(
                Kind::PackageInvalidTier,
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

    let verdict = policy
        .verify_package(&PackageAdmissionRequest {
            provider: pkg.provider_ref(),
            implementation: pkg.implementation,
            target_id: pkg.provider_id.to_string(),
            tier,
            signature: None,
        })
        .map_err(|e| DeploymentError::from_code(e.code, e.message))?;

    let fingerprint = sha256_digest(
        &canonical_bytes(&json!({
            "package_digest": pkg.package_digest,
            "policy_id": verdict.policy_id,
            "runtime_abi": "orion-v2",
            "schema_profile": "orion-json-schema/1",
        }))
        .expect("canonical fingerprint bytes serialize"),
    )
    .into_string();
    Ok(ProviderAdmissionRow {
        package_digest: pkg.package_digest.clone(),
        admission_fingerprint: fingerprint,
        policy_id: verdict.policy_id,
        revoked: false,
    })
}

// ============================================================
// Manifest closure and plans
// ============================================================

/// What the store holds that one manifest names, read in the transaction
/// that judges it.
#[derive(Default)]
pub(super) struct ClosureFacts {
    pub admissions: HashMap<Digest, ProviderAdmissionRow>,
    pub packages: HashMap<Digest, ProviderPackageRow>,
    /// Each named artifact's state. A package and a connection's
    /// configuration count as stored and active even without an
    /// `artifact_objects` row: their rows hold what the artifact would.
    pub artifacts: HashMap<String, ArtifactState>,
    pub connections: HashMap<(String, u64), ConnectionRevisionRow>,
}

impl ClosureFacts {
    /// Every digest a manifest's closure check reads: its providers, its
    /// members' definitions and their owning packages.
    pub fn digests_named_by(manifest: &CanonicalManifest) -> Vec<String> {
        let mut out: BTreeSet<String> = BTreeSet::new();
        out.extend(manifest.providers.iter().map(|p| p.digest.to_string()));
        for m in &manifest.members {
            out.insert(m.definition_digest.clone());
            if let Some(pkg) = &m.owning_package {
                out.insert(pkg.clone());
            }
        }
        out.into_iter().collect()
    }

    fn artifact(&self, digest: &str) -> Option<ArtifactState> {
        self.artifacts.get(digest).copied().or_else(|| {
            let held_by_a_row = self.packages.contains_key(digest)
                || self
                    .connections
                    .values()
                    .any(|c| c.configuration_digest == digest);
            held_by_a_row.then_some(ArtifactState::Active)
        })
    }
}

/// V2-03 / TD-19: every provider a manifest declares is admitted, not
/// revoked, stored with the identity the manifest gives it, and active; every
/// member names a stored, active definition (in its owning package's
/// inventory, when it has one); every connection member names a stored
/// revision whose configuration digest it carries, over a provider the
/// manifest declares or a built-in driver.
pub(super) fn verify_closure(
    facts: &ClosureFacts,
    manifest: &CanonicalManifest,
    inactive: Kind,
) -> Result<(), DeploymentError> {
    for p in &manifest.providers {
        let Some(adm) = facts.admissions.get(&p.digest) else {
            return Err(DeploymentError::new(
                Kind::ProviderNotAdmitted,
                format!(
                    "provider '{}' ({}) is not admitted",
                    p.provider_id, p.digest
                ),
            ));
        };
        if adm.revoked {
            return Err(DeploymentError::new(
                Kind::ProviderRevoked,
                format!("provider '{}' ({}) is revoked", p.provider_id, p.digest),
            ));
        }
        if let Some(pkg) = facts.packages.get(&p.digest)
            && pkg.provider_ref() != *p
        {
            return Err(DeploymentError::new(
                Kind::ProviderMetadataMismatch,
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
        if facts.artifact(&p.digest) != Some(ArtifactState::Active) {
            return Err(DeploymentError::new(
                inactive,
                format!("provider artifact '{}' is not ACTIVE", p.digest),
            ));
        }
    }

    let declared: HashSet<&str> = manifest
        .providers
        .iter()
        .map(|p| p.digest.as_str())
        .collect();

    for m in &manifest.members {
        // A member names a definition the store holds: what it deploys is
        // read from there, never from anywhere else.
        match facts.artifact(&m.definition_digest) {
            None => {
                return Err(DeploymentError::new(
                    Kind::MemberDefinitionNotFound,
                    format!(
                        "{} '{}' revision {} names definition '{}', which is not stored; \
                         upload it with POST /api/v2/admin/definitions first",
                        m.entity.kind.as_str(),
                        m.entity.id,
                        m.entity.revision,
                        m.definition_digest
                    ),
                ));
            }
            Some(ArtifactState::Active) => {}
            Some(_) => {
                return Err(DeploymentError::new(
                    inactive,
                    format!("member artifact '{}' is not ACTIVE", m.definition_digest),
                ));
            }
        }
        if let Some(owning) = &m.owning_package {
            if !declared.contains(owning.as_str()) {
                return Err(DeploymentError::new(
                    Kind::OwningPackageNotDeclared,
                    format!(
                        "member '{}' references owning_package '{}' not declared in manifest.providers",
                        m.entity.id, owning
                    ),
                ));
            }
            if let Some(pkg) = facts.packages.get(owning.as_str()) {
                let files = extract_inventory_file_digests(&pkg.inventory_json);
                if !files.is_empty() && !files.contains(&m.definition_digest) {
                    return Err(DeploymentError::new(
                        Kind::MemberArtifactNotInPackage,
                        format!(
                            "member '{}' definition_digest '{}' is not in owning_package '{}' inventory",
                            m.entity.id, m.definition_digest, owning
                        ),
                    ));
                }
            }
        }
        if m.entity.kind == EntityKind::Connection {
            let Some(conn) = facts
                .connections
                .get(&(m.entity.id.clone(), m.entity.revision.get()))
            else {
                return Err(DeploymentError::new(
                    Kind::ConnectionRevisionNotFound,
                    format!(
                        "connection '{}' revision {} is not in store",
                        m.entity.id, m.entity.revision
                    ),
                ));
            };
            if conn.configuration_digest != m.definition_digest {
                return Err(DeploymentError::new(
                    Kind::ConnectionDigestMismatch,
                    format!(
                        "connection '{}' revision {} configuration_digest '{}' does not match manifest definition_digest '{}'",
                        m.entity.id,
                        m.entity.revision,
                        conn.configuration_digest,
                        m.definition_digest
                    ),
                ));
            }
            // A built-in driver ships with the binary: it is always
            // available and has no package to declare.
            if !declared.contains(conn.provider_digest.as_str())
                && crate::extensions::foundation::builtin_driver(&conn.provider_digest).is_none()
            {
                return Err(DeploymentError::new(
                    Kind::ConnectionProviderNotDeclared,
                    format!(
                        "connection '{}' revision {} references provider '{}' not declared in manifest.providers",
                        m.entity.id, m.entity.revision, conn.provider_digest
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// V2-04.3: the plan of `candidate` on `base` (`None` for the empty base
/// before revision 1): its canonical bytes and digest, its closure over
/// `facts`, and the members it adds and removes.
///
/// `canonical` is the candidate's canonical bytes and digest, computed first
/// so a manifest that does not verify is refused before anything is read.
pub(super) fn plan(
    facts: &ClosureFacts,
    base_revision: u64,
    base: Option<&CanonicalManifest>,
    candidate: CanonicalManifest,
    (candidate_bytes, candidate_digest): (Vec<u8>, String),
) -> Result<CandidatePlan, DeploymentError> {
    let base_entities: BTreeSet<EntityRef> = base
        .map(|m| m.members.iter().map(|m| m.entity.clone()).collect())
        .unwrap_or_default();
    verify_closure(facts, &candidate, Kind::ArtifactNotActive)?;
    let next: BTreeSet<EntityRef> = candidate.members.iter().map(|m| m.entity.clone()).collect();
    Ok(CandidatePlan {
        base_revision,
        added_entities: next.difference(&base_entities).cloned().collect(),
        removed_entities: base_entities.difference(&next).cloned().collect(),
        candidate_manifest: candidate,
        candidate_digest,
        candidate_bytes,
        external_effect_warnings: Vec::new(),
    })
}

/// The apply-time recheck of a plan's providers: still allowed by the
/// policy's runtime lease, admitted, not revoked, and under this policy.
pub(super) fn recheck_admissions(
    facts: &ClosureFacts,
    manifest: &CanonicalManifest,
    policy: &Admission,
) -> Result<(), DeploymentError> {
    for p in &manifest.providers {
        if policy
            .check_runtime_lease(
                0,
                &p.provider_id,
                &ConnectionId::new(String::new()),
                crate::extensions::contract::EffectStatus::NotStarted,
            )
            .is_err()
        {
            return Err(DeploymentError::new(
                Kind::ProviderRevoked,
                format!("package '{}' is revoked by admission policy", p.digest),
            ));
        }
        let Some(adm) = facts.admissions.get(&p.digest) else {
            return Err(DeploymentError::new(
                Kind::ProviderNotAdmitted,
                p.digest.clone(),
            ));
        };
        if adm.revoked || adm.policy_id != policy.policy_id() {
            return Err(DeploymentError::new(
                Kind::AdmissionInvalidated,
                format!("admission for '{}' is revoked or policy changed", p.digest),
            ));
        }
    }
    Ok(())
}

// ============================================================
// Collection
// ============================================================

/// What one collection pass judges, read in its transaction.
pub(super) struct GcFacts {
    pub epoch: u64,
    /// The manifests of the retained revisions.
    pub retained: Vec<CanonicalManifest>,
    /// Revisions outside the retained set whose members are not yet
    /// compacted.
    pub compactable: Vec<u64>,
    /// Every package's file digests, by package digest.
    pub packages: BTreeMap<String, Vec<String>>,
    pub revoked: HashSet<String>,
    /// Every connection revision's configuration digest.
    pub configurations: HashSet<String>,
    /// Every artifact not yet deleted: digest, state, drain epoch, size.
    pub artifacts: Vec<(String, ArtifactState, Option<u64>, u64)>,
    /// Written or revived within the grace window.
    pub recent: HashSet<String>,
}

/// The revisions a pass keeps whole: the head, the `retain` newest, and
/// every one a node reports holding.
pub(super) fn retained_revisions(
    head: u64,
    revisions: &[(u64, bool)],
    retain: usize,
    held: impl IntoIterator<Item = u64>,
) -> BTreeSet<u64> {
    let mut retained = BTreeSet::from([head]);
    retained.extend(revisions.iter().rev().take(retain.max(1)).map(|(r, _)| *r));
    retained.extend(held);
    retained
}

/// V2-04.6: one pass of two-phase, epoch-fenced collection (`ACTIVE ->
/// DRAINING -> DELETED`) and member compaction (`TD-22`, `TD-23`).
///
/// **Never collected** — an artifact that is any of:
/// - referenced by a *retained* revision: its provider lock, each provider
///   package and its files, each member's definition, and each owning
///   package and its files;
/// - a connection revision's configuration (connection revisions are never
///   deleted, so neither is what they name);
/// - a package that is not revoked — admitted, or uploaded and awaiting
///   admission — and its files;
/// - recent: written or revived within the grace window, judged on the
///   database clock — a definition uploaded for an apply that has not
///   happened yet — and, for a recent package, its files.
///
/// Everything else moves one phase per pass: `ACTIVE` to `DRAINING` (bytes
/// kept), then, on a later pass once the epoch has moved past its drain,
/// `DRAINING` to `DELETED` (bytes dropped; the row stays as a tombstone a
/// re-upload revives). A protected artifact found `DRAINING` — one a peer
/// re-referenced after it was marked — is put back to `ACTIVE`. Revisions
/// outside the retained set are compacted, which is what makes rollback to
/// them refuse rather than half-work.
pub(super) fn plan_collection(facts: &GcFacts) -> GcOutcome {
    let mut out = GcOutcome::default();
    let mut protected: HashSet<String> = facts.recent.clone();
    let protect_package = |digest: &str, protected: &mut HashSet<String>| {
        protected.insert(digest.to_string());
        if let Some(files) = facts.packages.get(digest) {
            protected.extend(files.iter().cloned());
        }
    };
    for manifest in &facts.retained {
        if let Some(lock) = &manifest.provider_lock_digest {
            protected.insert(lock.clone());
        }
        for p in &manifest.providers {
            protect_package(&p.digest, &mut protected);
        }
        for m in &manifest.members {
            protected.insert(m.definition_digest.clone());
            if let Some(owning) = &m.owning_package {
                protect_package(owning, &mut protected);
            }
        }
    }
    protected.extend(facts.configurations.iter().cloned());
    for digest in facts.packages.keys() {
        if !facts.revoked.contains(digest) || facts.recent.contains(digest) {
            protect_package(digest, &mut protected);
        }
    }

    out.compacted = facts.compactable.clone();
    for (digest, state, drain_epoch, size) in &facts.artifacts {
        if protected.contains(digest) {
            if *state == ArtifactState::Draining {
                out.revived.push(digest.clone());
            }
            continue;
        }
        match state {
            ArtifactState::Active => out.draining.push(digest.clone()),
            ArtifactState::Draining if drain_epoch.is_none_or(|de| facts.epoch > de) => {
                out.deleted_bytes += size;
                out.deleted.push(digest.clone());
            }
            ArtifactState::Draining | ArtifactState::Deleted => {}
        }
    }
    out
}
