//! Deployment manifests: what a deployment deploys, as the wire spells it,
//! and the revision-free form `orion-cli compile --format deployment`
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
// Compiled deployments: what `orion-cli compile --format deployment`
// writes and `orion-cli deployments apply -d` reads
// ============================================================

/// The `schema_version` a compiled deployment directory's
/// `deployment.json` carries.
///
/// Version 2 added [`CompiledMember::rollouts`]. A version-1 directory has no
/// canary split and still reads: every member's `rollouts` defaults to empty.
pub const COMPILED_DEPLOYMENT_SCHEMA_VERSION: u64 = 2;

/// `deployment.json` of a compiled deployment directory: the members a
/// deployment deploys, with no revisions. Only the target knows which
/// revision a changed definition must get, so the deployer assigns them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompiledDeployment {
    pub schema_version: u64,
    pub members: Vec<CompiledMember>,
}

impl CompiledDeployment {
    /// `true` for every schema this build writes or still reads.
    ///
    /// Version 1 predates [`CompiledMember::rollouts`]; version 2 is what
    /// [`COMPILED_DEPLOYMENT_SCHEMA_VERSION`] names. A reader accepts both,
    /// and refuses anything else rather than guessing.
    pub fn supports_schema_version(version: u64) -> bool {
        (1..=COMPILED_DEPLOYMENT_SCHEMA_VERSION).contains(&version)
    }
}

/// One workflow revision a channel's rollout routes to, and the bucket
/// interval it serves.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompiledRollout {
    /// The workflow id whose revision this interval routes to.
    pub workflow: String,
    /// The digest of that revision's definition (the file
    /// `definitions/<hex>.json`).
    pub definition: String,
    pub start_bucket: u8,
    pub end_bucket: u8,
}

/// One member of a compiled deployment. A workflow, channel or plugin names
/// its `definition` by digest (the file `definitions/<hex>.json`); a
/// connection names its `connection` file (`connections/<id>.json`).
///
/// Two workflow members may share an id: they are two revisions of one
/// workflow, and a channel's `rollouts` selects between them. Every other
/// kind is unique by id.
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
    /// A channel's workflow split: disjoint bucket intervals covering
    /// `[0, 100)`, each naming the workflow id and definition digest it
    /// serves. Empty for a channel that serves one revision, and for every
    /// other kind.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rollouts: Vec<CompiledRollout>,
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
    /// Over a provider that declares dependencies: which connection of the
    /// set each dependency alias calls (`alias -> connection_id`). Omitted
    /// when empty, so a connection without any compiles as it always did.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bindings: BTreeMap<String, String>,
}

#[cfg(test)]
#[expect(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A version-1 directory predates `rollouts`: every member reads with an
    /// empty split, so an old compiled tree still deploys.
    #[test]
    fn a_version_one_directory_reads_with_no_rollouts() {
        let text = r#"{
            "schema_version": 1,
            "members": [{
                "kind": "channel",
                "id": "orders",
                "definition": "sha256:abc",
                "requires": [{"kind": "workflow", "id": "checkout"}]
            }]
        }"#;
        let deployment: CompiledDeployment = serde_json::from_str(text).unwrap();
        assert_eq!(deployment.schema_version, 1);
        assert!(deployment.members[0].rollouts.is_empty());
        assert!(CompiledDeployment::supports_schema_version(1));
        assert!(CompiledDeployment::supports_schema_version(2));
        assert!(!CompiledDeployment::supports_schema_version(0));
        assert!(!CompiledDeployment::supports_schema_version(3));
    }

    /// Version 2 carries a channel's split, and an unsplit member omits the
    /// key entirely so its bytes are the version-1 shape plus the version.
    #[test]
    fn a_canary_channel_round_trips_and_an_unsplit_one_omits_rollouts() {
        let split = CompiledMember {
            kind: EntityKind::Channel,
            id: "orders".into(),
            definition: Some("sha256:channel".into()),
            connection: None,
            requires: vec![CompiledRequirement {
                kind: EntityKind::Workflow,
                id: "checkout".into(),
            }],
            rollouts: vec![
                CompiledRollout {
                    workflow: "checkout".into(),
                    definition: "sha256:v1".into(),
                    start_bucket: 0,
                    end_bucket: 50,
                },
                CompiledRollout {
                    workflow: "checkout".into(),
                    definition: "sha256:v2".into(),
                    start_bucket: 50,
                    end_bucket: 100,
                },
            ],
        };
        let json = serde_json::to_value(&split).unwrap();
        assert_eq!(json["rollouts"][1]["definition"], "sha256:v2");
        let back: CompiledMember = serde_json::from_value(json).unwrap();
        assert_eq!(back, split);

        let unsplit = CompiledMember {
            rollouts: Vec::new(),
            ..split
        };
        let json = serde_json::to_value(&unsplit).unwrap();
        assert!(json.get("rollouts").is_none(), "{json}");
    }

    /// The member type stays strict: a typo in a rollout is refused, not
    /// silently ignored.
    #[test]
    fn an_unknown_rollout_key_is_refused() {
        let text = json!({
            "kind": "channel",
            "id": "orders",
            "definition": "sha256:channel",
            "requires": [],
            "rollouts": [{
                "workflow": "checkout",
                "definition": "sha256:v1",
                "start_bucket": 0,
                "end_bucket": 100,
                "weight": 100
            }]
        });
        assert!(serde_json::from_value::<CompiledMember>(text).is_err());
    }
}
