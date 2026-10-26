//! Typed entity references, disjoint rollout partitions, and canonical deployment manifests (`V2-03.1`, `MOD-02`).

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{DeploymentError, DeploymentErrorKind};
pub use crate::extensions::ProviderRef;
use crate::extensions::package_identity::canonical_bytes;
use crate::extensions::{Digest, Revision};

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
    pub revision: Revision,
}

impl EntityRef {
    pub fn validate(&self) -> Result<(), DeploymentError> {
        if self.id.is_empty() || self.id.starts_with(Digest::PREFIX) {
            return Err(DeploymentError::new(
                DeploymentErrorKind::EntityRefInvalid,
                "entity id must be non-empty and must not be a digest",
            ));
        }
        Ok(())
    }
}

/// A manifest's provider entry: a publisher and an id, and a semantic
/// version (not an integer revision or a digest). The digest's shape is the
/// type's.
pub fn validate_provider_ref(provider: &ProviderRef) -> Result<(), DeploymentError> {
    if provider.publisher.is_empty() || provider.provider_id.is_empty() {
        return Err(DeploymentError::new(
            DeploymentErrorKind::ProviderRefInvalid,
            "publisher and provider_id are required",
        ));
    }
    if !provider.version.contains('.') || provider.version.starts_with(Digest::PREFIX) {
        return Err(DeploymentError::new(
            DeploymentErrorKind::ProviderRefInvalid,
            "provider version must be a semantic version string, not an integer revision or digest",
        ));
    }
    Ok(())
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
            DeploymentErrorKind::RolloutPartitionInvalid,
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
                DeploymentErrorKind::RolloutPartitionInvalid,
                "duplicate or zero workflow revision in rollout",
            ));
        }
        if interval.start_bucket >= interval.end_bucket || interval.end_bucket > 100 {
            return Err(DeploymentError::new(
                DeploymentErrorKind::RolloutPartitionInvalid,
                "invalid bucket bounds",
            ));
        }
        if interval.start_bucket != cursor {
            return Err(DeploymentError::new(
                DeploymentErrorKind::RolloutPartitionInvalid,
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
            DeploymentErrorKind::RolloutPartitionInvalid,
            format!("rollout intervals end at {cursor}, expected 100"),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestMember {
    pub entity: EntityRef,
    pub definition_digest: String,
    /// Absent, none, empty: a member that needs nothing beyond its entity
    /// and its definition writes only those. The canonical form spells them
    /// out, so the digest is the same either way.
    #[serde(default)]
    pub owning_package: Option<String>,
    #[serde(default)]
    pub requires: Vec<EntityRef>,
    #[serde(default)]
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
    /// The manifest `value` spells, read once at the API boundary. A digest
    /// and a revision are checked types, so a malformed one is refused here
    /// rather than by [`Self::canonicalize_and_verify`]; it is still named
    /// as the reference it is (`PROVIDER_REF_INVALID`, `ENTITY_REF_INVALID`),
    /// and anything else unreadable is `INVALID_MANIFEST_JSON`.
    pub fn from_json(value: &Value) -> Result<Self, DeploymentError> {
        Self::deserialize(value).map_err(|e| {
            malformed_ref(value).unwrap_or_else(|| {
                DeploymentError::new(DeploymentErrorKind::InvalidManifestJson, e.to_string())
            })
        })
    }

    /// Validate ownership, singleton constraints, rollout partitions, and
    /// `requires` closure, returning canonical JSON bytes and `sha256:` digest.
    pub fn canonicalize_and_verify(&self) -> Result<(Vec<u8>, String), DeploymentError> {
        if self.schema_version != 2 {
            return Err(DeploymentError::new(
                DeploymentErrorKind::ManifestVersionUnsupported,
                "deployment manifest schema_version must be 2",
            ));
        }

        let mut provider_keys = HashSet::new();
        let mut provider_digests = HashSet::new();
        for p in &self.providers {
            validate_provider_ref(p)?;
            if !provider_keys.insert((
                p.publisher.clone(),
                p.provider_id.clone(),
                p.version.clone(),
            )) {
                return Err(DeploymentError::new(
                    DeploymentErrorKind::ManifestDuplicateProvider,
                    format!(
                        "duplicate provider {}/{}@{}",
                        p.publisher, p.provider_id, p.version
                    ),
                ));
            }
            provider_digests.insert(p.digest.clone());
        }

        // Singleton entity check (except workflows under rollout, which are referenced via Channel rollouts)
        let mut member_set: HashMap<(EntityKind, String), Revision> = HashMap::new();
        let mut all_entities: HashSet<EntityRef> = HashSet::new();
        let mut owner_by_id: HashMap<(EntityKind, String), Option<String>> = HashMap::new();

        for m in &self.members {
            m.entity.validate()?;
            if let Some(pkg) = &m.owning_package
                && !provider_digests.contains(pkg.as_str())
            {
                return Err(DeploymentError::new(
                    DeploymentErrorKind::ManifestOwnerMissing,
                    format!("owning_package '{pkg}' is not in manifest providers"),
                ));
            }
            if let Some(prev_owner) = owner_by_id.get(&(m.entity.kind, m.entity.id.clone()))
                && prev_owner != &m.owning_package
            {
                return Err(DeploymentError::new(
                    DeploymentErrorKind::ManifestOwnershipConflict,
                    format!(
                        "conflicting ownership for {}:{}",
                        m.entity.kind.as_str(),
                        m.entity.id
                    ),
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
                        DeploymentErrorKind::ManifestDuplicateSingleton,
                        format!(
                            "multiple revisions of singleton {}:{}",
                            m.entity.kind.as_str(),
                            m.entity.id
                        ),
                    ));
                }
            } else if !all_entities.insert(m.entity.clone()) {
                return Err(DeploymentError::new(
                    DeploymentErrorKind::ManifestDuplicateMember,
                    format!(
                        "duplicate workflow revision {}:{}",
                        m.entity.id, m.entity.revision
                    ),
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
                        DeploymentErrorKind::ManifestMissingRequirement,
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
            DeploymentError::new(DeploymentErrorKind::ManifestCanonicalError, e.code)
        })?;
        let digest = crate::crypto::sha256_digest(&bytes).into_string();
        Ok((bytes, digest))
    }
}

/// The reference `value` holds that its types refuse: a provider digest
/// that is not one, or an entity revision of `0`. `None` when the problem is
/// elsewhere.
fn malformed_ref(value: &Value) -> Option<DeploymentError> {
    let providers = value.get("providers").and_then(Value::as_array);
    for provider in providers.into_iter().flatten() {
        if let Some(digest) = provider.get("digest").and_then(Value::as_str)
            && Digest::parse(digest).is_err()
        {
            return Some(DeploymentError::new(
                DeploymentErrorKind::ProviderRefInvalid,
                format!("provider digest must be sha256:<64 lowercase hex>, not '{digest}'"),
            ));
        }
    }
    let members = value.get("members").and_then(Value::as_array);
    let refs = members.into_iter().flatten().flat_map(|member| {
        let requires = member.get("requires").and_then(Value::as_array);
        member
            .get("entity")
            .into_iter()
            .chain(requires.into_iter().flatten())
    });
    for entity in refs {
        if entity.get("revision").and_then(Value::as_u64) == Some(0) {
            return Some(DeploymentError::new(
                DeploymentErrorKind::EntityRefInvalid,
                "entity integer revision must be >= 1",
            ));
        }
    }
    None
}
