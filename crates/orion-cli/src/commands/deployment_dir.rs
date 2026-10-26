//! Deploying a compiled deployment directory — what
//! `orion-cli compile --format deployment` writes — to a target.
//!
//! The directory carries no revisions, because only the target knows them.
//! This module reads the target's history and assigns each member one:
//!
//! - a member the head deploys with the same definition keeps its revision;
//! - a member some earlier revision deployed with the same definition takes
//!   that revision back (revisions are immutable, so it names the same thing);
//! - anything else takes one past the highest revision the entity ever had,
//!   or 1 for an entity the target has never seen.
//!
//! A connection reuses a stored revision with the same provider, public
//! settings and secret locators, and otherwise creates the next one.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use orion_api::v2::{
    COMPILED_DEPLOYMENT_SCHEMA_VERSION, CompiledConnection, CompiledDeployment,
    ConnectionRevisionResponse, CreateConnectionRevisionRequest, Definition, DeploymentManifest,
    EntityKind, EntityRef, ManifestMember,
};
use orion_client::V2Admin;

use crate::client::OrionClient;

/// A deployment directory, read and checked against itself.
#[derive(Debug)]
pub struct DeploymentDir {
    pub deployment: CompiledDeployment,
    /// Digest → the definition file's path, for every digest a member names.
    pub definitions: BTreeMap<String, PathBuf>,
    /// Connection id → its file.
    pub connections: BTreeMap<String, CompiledConnection>,
}

impl DeploymentDir {
    pub fn read(root: &Path) -> Result<Self> {
        let manifest_path = root.join("deployment.json");
        let text = std::fs::read_to_string(&manifest_path).with_context(|| {
            format!(
                "reading '{}' — is this a directory `orion-cli compile --format deployment` \
                 wrote?",
                manifest_path.display()
            )
        })?;
        let deployment: CompiledDeployment = serde_json::from_str(&text)
            .with_context(|| format!("'{}' is not a deployment", manifest_path.display()))?;
        if deployment.schema_version != COMPILED_DEPLOYMENT_SCHEMA_VERSION {
            bail!(
                "'{}' has schema_version {}; this orion-cli reads {}",
                manifest_path.display(),
                deployment.schema_version,
                COMPILED_DEPLOYMENT_SCHEMA_VERSION
            );
        }

        let mut seen = BTreeSet::new();
        let mut definitions = BTreeMap::new();
        let mut connections = BTreeMap::new();
        for member in &deployment.members {
            if !seen.insert((member.kind, member.id.as_str())) {
                bail!(
                    "deployment.json lists {} '{}' twice",
                    member.kind,
                    member.id
                );
            }
            if member.kind == EntityKind::Connection {
                let name = member.connection.as_deref().unwrap_or(&member.id);
                let path = root.join("connections").join(format!("{name}.json"));
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading connection '{}'", path.display()))?;
                let connection: CompiledConnection = serde_json::from_str(&text)
                    .with_context(|| format!("'{}' is not a connection", path.display()))?;
                if connection.connection_id != member.id {
                    bail!(
                        "'{}' is connection '{}', and deployment.json names it '{}'",
                        path.display(),
                        connection.connection_id,
                        member.id
                    );
                }
                connections.insert(member.id.clone(), connection);
            } else if matches!(
                member.kind,
                EntityKind::Workflow | EntityKind::Channel | EntityKind::Plugin
            ) {
                let Some(digest) = member.definition.as_deref() else {
                    bail!("{} '{}' names no definition", member.kind, member.id);
                };
                let Ok(parsed) = orion_api::Digest::parse(digest) else {
                    bail!(
                        "{} '{}' names definition '{digest}', which is not a sha256: digest",
                        member.kind,
                        member.id
                    );
                };
                // `hex()` is exactly 64 lowercase hex characters, so the
                // digest cannot name a path outside `definitions/`.
                let path = root
                    .join("definitions")
                    .join(format!("{}.json", parsed.hex()));
                if !path.is_file() {
                    bail!(
                        "{} '{}' names definition {digest}, and '{}' is not there",
                        member.kind,
                        member.id,
                        path.display()
                    );
                }
                definitions.insert(digest.to_string(), path);
            } else {
                bail!(
                    "deployment.json lists '{}' of kind '{}', which a deployment cannot carry",
                    member.id,
                    member.kind
                );
            }
        }
        for member in &deployment.members {
            for req in &member.requires {
                if !seen.contains(&(req.kind, req.id.as_str())) {
                    bail!(
                        "{} '{}' requires {} '{}', which deployment.json does not list",
                        member.kind,
                        member.id,
                        req.kind,
                        req.id
                    );
                }
            }
        }
        Ok(Self {
            deployment,
            definitions,
            connections,
        })
    }
}

/// `(kind, id, revision, definition digest)` of one deployed member.
type Deployed = (EntityKind, String, u64, String);

/// What the target has deployed: its head, and every revision before it.
#[derive(Debug, Default)]
pub struct History {
    pub head_revision: u64,
    pub head_manifest_digest: Option<String>,
    /// What the head deploys.
    head: Vec<Deployed>,
    /// The same for every revision the target lists, head included.
    all: Vec<Deployed>,
}

impl History {
    /// The target's history: every committed revision, page by page, and
    /// the manifest of each one not compacted. A compacted revision has no
    /// members left to read.
    pub async fn read(client: &OrionClient) -> Result<Self> {
        let first = client.list_deployments(None, None).await?;
        let mut history = Self {
            head_revision: first.head_revision,
            ..Self::default()
        };
        let mut page = first;
        loop {
            for summary in page.data.iter().filter(|r| !r.members_compacted) {
                let revision = client.get_deployment(summary.revision).await?;
                history.add(
                    summary.revision,
                    &summary.manifest_digest,
                    &revision.manifest,
                );
            }
            let Some(cursor) = page.next_cursor.take() else {
                break;
            };
            page = client.list_deployments(None, Some(&cursor)).await?;
        }
        Ok(history)
    }

    /// Record one revision's members.
    fn add(&mut self, revision: u64, manifest_digest: &str, manifest: &DeploymentManifest) {
        let members: Vec<Deployed> = manifest
            .members
            .iter()
            .map(|m| {
                (
                    m.entity.kind,
                    m.entity.id.clone(),
                    m.entity.revision,
                    m.definition_digest.clone(),
                )
            })
            .collect();
        if revision == self.head_revision {
            self.head = members.clone();
            self.head_manifest_digest = Some(manifest_digest.to_string());
        }
        self.all.extend(members);
    }

    /// The revision `(kind, id)` takes for `digest`: kept, taken back, or
    /// the next one.
    pub fn revision_for(&self, kind: EntityKind, id: &str, digest: &str) -> u64 {
        let same = |list: &[Deployed]| {
            list.iter()
                .filter(|(k, i, _, d)| *k == kind && i == id && d == digest)
                .map(|(_, _, r, _)| *r)
                .max()
        };
        if let Some(revision) = same(&self.head).or_else(|| same(&self.all)) {
            return revision;
        }
        self.all
            .iter()
            .filter(|(k, i, _, _)| *k == kind && i == id)
            .map(|(_, _, r, _)| *r)
            .max()
            .unwrap_or(0)
            + 1
    }

    /// The revision of connection `id` the head deploys, if it deploys one.
    fn head_connection(&self, id: &str) -> Option<u64> {
        self.head
            .iter()
            .find(|(k, i, _, _)| *k == EntityKind::Connection && i == id)
            .map(|(_, _, r, _)| *r)
    }
}

/// The stored revision of a connection that matches `wanted`, if one does:
/// the head's when it matches, else the highest.
pub fn matching_connection_revision(
    wanted: &CompiledConnection,
    stored: &[ConnectionRevisionResponse],
    head_revision: Option<u64>,
) -> Option<(u64, String)> {
    let matches: Vec<&ConnectionRevisionResponse> = stored
        .iter()
        .filter(|row| {
            row.connection_id == wanted.connection_id
                && row.provider_digest == wanted.provider_digest
                && row.public_config == wanted.public_config
                && row.secret_locators == wanted.secret_locators
        })
        .collect();
    matches
        .iter()
        .find(|row| Some(row.revision) == head_revision)
        .or_else(|| matches.iter().max_by_key(|row| row.revision))
        .map(|row| (row.revision, row.configuration_digest.clone()))
}

/// A member with the revision it was assigned, for the summary.
#[derive(Debug, Clone)]
pub struct Assigned {
    pub kind: EntityKind,
    pub id: String,
    pub revision: u64,
    /// Whether the head deploys this revision already.
    pub unchanged: bool,
}

/// What deploying a directory sends: the manifest, and against what.
#[derive(Debug)]
pub struct Prepared {
    pub base_revision: u64,
    pub head_manifest_digest: Option<String>,
    pub manifest: DeploymentManifest,
    pub assigned: Vec<Assigned>,
}

/// Upload the directory's definitions, create the connection revisions it
/// needs, and build the manifest that deploys it on top of the target's
/// head. Writes nothing that serves: definitions and connection revisions
/// are inert until a manifest names them. With `expected_head`, refuses
/// before writing anything unless the head is that revision.
pub async fn prepare(
    client: &OrionClient,
    dir: &DeploymentDir,
    expected_head: Option<u64>,
) -> Result<Prepared> {
    let history = History::read(client).await?;
    if let Some(expected) = expected_head
        && expected != history.head_revision
    {
        bail!(
            "the target's head is revision {}, not {expected}",
            history.head_revision
        );
    }

    for (digest, path) in &dir.definitions {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading '{}'", path.display()))?;
        let definition: Definition = serde_json::from_str(&text)
            .with_context(|| format!("'{}' is not a definition", path.display()))?;
        let stored = client
            .upload_definition(&definition)
            .await
            .with_context(|| format!("uploading '{}'", path.display()))?;
        if &stored.digest != digest {
            bail!(
                "the target stored '{}' as {}, not {digest}: it canonicalises definitions \
                 differently from the orion-cli that compiled this directory — compile with \
                 the target's version",
                path.display(),
                stored.digest
            );
        }
        for advisory in &stored.advisories {
            eprintln!(
                "warning: '{}' {}: {} [{}]",
                path.display(),
                advisory.path,
                advisory.message,
                advisory.check
            );
        }
    }

    let mut connection_revisions: BTreeMap<String, (u64, String)> = BTreeMap::new();
    if !dir.connections.is_empty() {
        let capabilities = client.runtime_capabilities().await?;
        let builtin: BTreeMap<String, String> = capabilities
            .provider_backends
            .into_iter()
            .filter_map(|backend| Some((backend.driver_id, backend.package_digest?)))
            .collect();
        let stored = client.list_connections().await?;
        for (id, connection) in &dir.connections {
            match builtin.get(&connection.driver) {
                Some(digest) if digest == &connection.provider_digest => {}
                Some(digest) => bail!(
                    "connection '{id}' names {} as driver {}'s digest, and the target's is \
                     {digest}",
                    connection.provider_digest,
                    connection.driver
                ),
                None => bail!(
                    "connection '{id}' runs on driver '{}', which the target does not have",
                    connection.driver
                ),
            }
            let found = matching_connection_revision(
                connection,
                &stored.connections,
                history.head_connection(id),
            );
            let assigned = match found {
                Some(existing) => existing,
                None => {
                    let revision = stored
                        .connections
                        .iter()
                        .filter(|row| &row.connection_id == id)
                        .map(|row| row.revision)
                        .max()
                        .unwrap_or(0)
                        + 1;
                    let created = client
                        .create_connection_revision(
                            id,
                            &CreateConnectionRevisionRequest {
                                revision,
                                provider_digest: connection.provider_digest.clone(),
                                public_config: connection.public_config.clone(),
                                secret_locators: connection.secret_locators.clone(),
                                bindings: Default::default(),
                            },
                        )
                        .await
                        .with_context(|| {
                            format!("creating connection '{id}' revision {revision}")
                        })?;
                    (revision, created.connection.configuration_digest)
                }
            };
            connection_revisions.insert(id.clone(), assigned);
        }
    }

    // Every member's revision, then every requirement resolved against them.
    let mut revisions: BTreeMap<(EntityKind, String), (u64, String)> = BTreeMap::new();
    for member in &dir.deployment.members {
        let assigned = if member.kind == EntityKind::Connection {
            connection_revisions
                .get(&member.id)
                .cloned()
                .with_context(|| format!("connection '{}' was not resolved", member.id))?
        } else {
            let digest = member.definition.clone().unwrap_or_default();
            (
                history.revision_for(member.kind, &member.id, &digest),
                digest,
            )
        };
        revisions.insert((member.kind, member.id.clone()), assigned);
    }

    let mut members = Vec::new();
    let mut assigned = Vec::new();
    for member in &dir.deployment.members {
        let revision_of = |kind: EntityKind, id: &str| {
            revisions
                .get(&(kind, id.to_string()))
                .cloned()
                .with_context(|| format!("{kind} '{id}' was assigned no revision"))
        };
        let (revision, digest) = revision_of(member.kind, &member.id)?;
        let mut requires = Vec::new();
        for req in &member.requires {
            let (rev, _) = revision_of(req.kind, &req.id)?;
            requires.push(EntityRef {
                kind: req.kind,
                id: req.id.clone(),
                revision: rev,
            });
        }
        members.push(ManifestMember {
            entity: EntityRef {
                kind: member.kind,
                id: member.id.clone(),
                revision,
            },
            definition_digest: digest,
            owning_package: None,
            requires,
            rollouts: Vec::new(),
        });
        assigned.push(Assigned {
            kind: member.kind,
            id: member.id.clone(),
            revision,
            unchanged: history
                .head
                .iter()
                .any(|(k, i, r, _)| *k == member.kind && i == &member.id && *r == revision),
        });
    }
    Ok(Prepared {
        base_revision: history.head_revision,
        head_manifest_digest: history.head_manifest_digest,
        manifest: DeploymentManifest::of_members(members),
        assigned,
    })
}

/// The default idempotency key for applying `candidate_digest` on top of
/// `base_revision`: a retry of the same apply replays it, and the same
/// manifest applied again later, over another head, is a new operation.
pub fn default_idempotency_key(base_revision: u64, candidate_digest: &str) -> String {
    format!(
        "deploy-{base_revision}-{}",
        candidate_digest.trim_start_matches(orion_api::Digest::PREFIX)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn member(kind: EntityKind, id: &str, rev: u64, digest: &str) -> ManifestMember {
        ManifestMember {
            entity: EntityRef {
                kind,
                id: id.to_string(),
                revision: rev,
            },
            definition_digest: digest.to_string(),
            owning_package: None,
            requires: Vec::new(),
            rollouts: Vec::new(),
        }
    }

    #[test]
    fn a_member_keeps_takes_back_or_advances_its_revision() {
        use EntityKind::{Channel, Workflow};
        let mut history = History {
            head_revision: 3,
            ..History::default()
        };
        let manifest = |m: ManifestMember| DeploymentManifest::of_members(vec![m]);
        history.add(
            1,
            "sha256:1",
            &manifest(member(Workflow, "w", 1, "sha256:a")),
        );
        history.add(
            2,
            "sha256:2",
            &manifest(member(Workflow, "w", 2, "sha256:b")),
        );
        // A rollback to revision 1's manifest.
        history.add(
            3,
            "sha256:3",
            &manifest(member(Workflow, "w", 1, "sha256:a")),
        );
        assert_eq!(history.revision_for(Workflow, "w", "sha256:a"), 1);
        assert_eq!(history.revision_for(Workflow, "w", "sha256:b"), 2);
        // New content goes past every revision the entity ever had, not the
        // head's: revision 2 already names something else.
        assert_eq!(history.revision_for(Workflow, "w", "sha256:c"), 3);
        assert_eq!(history.revision_for(Channel, "c", "sha256:z"), 1);
        assert_eq!(history.head_manifest_digest.as_deref(), Some("sha256:3"));
    }

    #[test]
    fn a_connection_reuses_a_revision_with_the_same_settings() {
        let wanted = CompiledConnection {
            connection_id: "db".into(),
            driver: "org.orion.sqlite".into(),
            provider_digest: "sha256:p".into(),
            public_config: json!({"connection_string": "sqlite::memory:"}),
            secret_locators: BTreeMap::new(),
        };
        let row = |rev: u64, config: Value| ConnectionRevisionResponse {
            connection_id: "db".into(),
            revision: rev,
            provider_digest: "sha256:p".into(),
            public_config: config,
            configuration_digest: format!("sha256:c{rev}"),
            ..Default::default()
        };
        let stored = vec![
            row(1, json!({"connection_string": "sqlite::memory:"})),
            row(2, json!({"connection_string": "sqlite:/tmp/x.db"})),
            row(3, json!({"connection_string": "sqlite::memory:"})),
        ];
        assert_eq!(
            matching_connection_revision(&wanted, &stored, Some(1)),
            Some((1, "sha256:c1".to_string()))
        );
        assert_eq!(
            matching_connection_revision(&wanted, &stored, Some(2)),
            Some((3, "sha256:c3".to_string()))
        );
        let other = CompiledConnection {
            public_config: json!({"connection_string": "sqlite:/tmp/y.db"}),
            ..wanted
        };
        assert_eq!(matching_connection_revision(&other, &stored, None), None);
    }
}
