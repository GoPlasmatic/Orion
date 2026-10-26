//! Deployment manifests: what a deployment deploys, as the wire spells it,
//! and the revision-free form `orion-server compile --format deployment`
//! writes.
//!
//! These are the *shapes*. The server reads a [`DeploymentManifest`] into
//! its checked form (digests parsed, revisions non-zero, ownership, rollout
//! partitions and the `requires` closure verified) and refuses what fails
//! there by name — `PROVIDER_REF_INVALID`, `ENTITY_REF_INVALID`,
//! `MANIFEST_MISSING_REQUIREMENT`, … — which is why a digest is a `String`
//! and a revision a `u64` here rather than their checked types.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::vocab::EntityKind;

/// The `schema_version` a [`DeploymentManifest`] carries.
pub const DEPLOYMENT_MANIFEST_SCHEMA_VERSION: u64 = 2;

/// A deployment manifest: the providers a deployment pins and the members
/// it deploys (`POST /deployment-plans`, `POST /deployments`, and read back
/// by `GET /deployments/{revision}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct DeploymentManifest {
    /// Always [`DEPLOYMENT_MANIFEST_SCHEMA_VERSION`]; anything else is
    /// `MANIFEST_VERSION_UNSUPPORTED`.
    pub schema_version: u64,
    /// The uploaded provider packages the members run on. A connection over
    /// a built-in driver needs no entry.
    pub providers: Vec<ManifestProvider>,
    /// The digest of the provider lock the providers were resolved under,
    /// when they were.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_lock_digest: Option<String>,
    pub members: Vec<ManifestMember>,
}

impl DeploymentManifest {
    /// A manifest of `members` over no uploaded providers.
    pub fn of_members(members: Vec<ManifestMember>) -> Self {
        Self {
            schema_version: DEPLOYMENT_MANIFEST_SCHEMA_VERSION,
            providers: Vec::new(),
            provider_lock_digest: None,
            members,
        }
    }
}

/// One provider a manifest pins: a publisher, an id, a semantic version
/// and the package digest.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ManifestProvider {
    pub publisher: String,
    pub provider_id: String,
    pub version: String,
    /// `sha256:<64 lowercase hex>`.
    pub digest: String,
}

/// One member of a manifest: an entity at a revision, the definition (or,
/// for a connection, the configuration) it is, and what it needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ManifestMember {
    pub entity: EntityRef,
    /// A workflow, channel or plugin's stored definition; a connection
    /// revision's `configuration_digest`.
    pub definition_digest: String,
    /// The provider package (by digest, one of the manifest's `providers`)
    /// that owns this member. Absent for a member no package owns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owning_package: Option<String>,
    /// The members this one needs, each at the revision it uses. Absent
    /// means none.
    #[serde(default)]
    pub requires: Vec<EntityRef>,
    /// A channel's workflow rollout: disjoint bucket intervals covering
    /// `[0, 100)`. Absent means the channel serves one workflow revision.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rollouts: Vec<RolloutInterval>,
}

/// An entity at a revision.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct EntityRef {
    pub kind: EntityKind,
    pub id: String,
    /// At least 1.
    pub revision: u64,
}

/// One half-open bucket interval `[start_bucket, end_bucket)` of a
/// channel's rollout, served by `workflow_revision`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct RolloutInterval {
    pub workflow_revision: u64,
    pub start_bucket: u8,
    pub end_bucket: u8,
}

// ============================================================
// Compiled deployments: what `orion-server compile --format deployment`
// writes and `orion-cli deployments apply -d` reads
// ============================================================

/// The `schema_version` a compiled deployment directory's
/// `deployment.json` carries.
pub const COMPILED_DEPLOYMENT_SCHEMA_VERSION: u64 = 1;

/// `deployment.json` of a compiled deployment directory: the members a
/// deployment deploys, with no revisions. Only the target knows which
/// revision a changed definition must get, so the deployer assigns them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompiledDeployment {
    pub schema_version: u64,
    pub members: Vec<CompiledMember>,
}

/// One member of a compiled deployment. A workflow, channel or plugin names
/// its `definition` by digest (the file `definitions/<hex>.json`); a
/// connection names its `connection` file (`connections/<id>.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompiledMember {
    pub kind: EntityKind,
    pub id: String,
    /// A workflow, channel or plugin's definition digest; absent for a
    /// connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
    /// A connection's file name under `connections/`; absent for anything
    /// else, and for a connection whose file is named by its id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    pub requires: Vec<CompiledRequirement>,
}

/// A member another member depends on, by kind and id. The deployer
/// resolves it to the revision it assigns that member.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompiledRequirement {
    pub kind: EntityKind,
    pub id: String,
}

/// `connections/<id>.json` of a compiled deployment directory: a connection
/// over a built-in driver, without a revision. Its fields are those of a
/// [`CreateConnectionRevisionRequest`](super::CreateConnectionRevisionRequest)
/// for the connection it names, plus the driver the provider digest names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompiledConnection {
    pub connection_id: String,
    /// The built-in driver, such as `org.orion.postgresql`.
    pub driver: String,
    /// The driver's provider digest, as `GET /runtime-capabilities` reports
    /// it under `provider_backends[].package_digest`.
    pub provider_digest: String,
    pub public_config: Value,
    pub secret_locators: BTreeMap<String, String>,
}
