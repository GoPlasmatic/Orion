//! A definition set as a revision-free v2 deployment directory — what
//! `orion-cli compile --format deployment` writes.
//!
//! ```text
//! deployment.json            members by kind and id, no revisions
//! definitions/<hex>.json     one Definition each, in canonical bytes, named by digest
//! connections/<id>.json      one per connector, its secrets as references
//! ```
//!
//! Revisions are left out because only the target knows them: a member
//! whose definition the target already deploys keeps its revision, and a
//! changed one takes the next. `orion-cli deployments apply -d` assigns them.
//!
//! Every conversion is strict. A key a definition does not define is an
//! error naming the entity and the key, never dropped: a deployment that
//! silently lost a setting would serve something nobody wrote.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use orion_api::v2::{
    COMPILED_DEPLOYMENT_SCHEMA_VERSION, ChannelDefinition, CompiledConnection, CompiledDeployment,
    CompiledMember, CompiledRequirement, CompiledRollout, Definition, EntityKind, PluginDefinition,
    WorkflowDefinition,
};
use serde_json::{Map, Value};

use super::{DefinitionSet, Entity};
use crate::extensions::foundation;

/// A compiled deployment: every file it consists of, by path relative to
/// the output directory, and what was left out on the way.
#[derive(Debug, Default)]
pub struct DeploymentFiles {
    /// Relative path → bytes, sorted by path.
    pub files: BTreeMap<String, Vec<u8>>,
    /// What the conversion did not carry that a reader might expect it to,
    /// such as a connector's tags.
    pub notes: Vec<String>,
    pub counts: Counts,
}

/// How many members of each kind a deployment has.
#[derive(Debug, Default, Clone, Copy)]
pub struct Counts {
    pub workflows: usize,
    pub channels: usize,
    pub plugins: usize,
    pub connections: usize,
}

/// The keys a workflow in a set may carry: the definition's own, the v1
/// failure flag it replaces, and the identity the deployment supplies.
const WORKFLOW_KEYS: &[&str] = &[
    "name",
    "description",
    "priority",
    "condition",
    "tasks",
    "tags",
    "loop",
    "on_failure",
];
const WORKFLOW_IDENTITY: &[&str] = &["workflow_id", "version", "status"];

const CHANNEL_KEYS: &[&str] = &[
    "name",
    "description",
    "channel_type",
    "protocol",
    "methods",
    "route_pattern",
    "transport_config",
    "config",
    "priority",
    "tags",
];
const CHANNEL_IDENTITY: &[&str] = &["channel_id", "version", "status"];

/// Build the deployment `set` compiles to. Every problem is reported, not
/// just the first.
pub fn build(set: &DefinitionSet) -> Result<DeploymentFiles, Vec<String>> {
    let mut errors = Vec::new();
    let mut out = DeploymentFiles::default();
    let mut members: Vec<CompiledMember> = Vec::new();

    // Which functions take a connector, plugins of the set included. A set
    // whose plugins collide was refused by the gate already; the built-in
    // registry still answers for the built-ins.
    let registry = set.function_registry().ok();
    let functions = registry
        .as_ref()
        .unwrap_or_else(|| crate::engine::FunctionRegistry::builtin());

    // Connections first: a workflow's requirements are checked against them.
    let mut connection_ids: BTreeSet<String> = BTreeSet::new();
    let mut bound: Vec<(String, String, BTreeMap<String, String>)> = Vec::new();
    for def in set.iter(Entity::Connector) {
        match connection(&def.doc, &def.origin) {
            Ok((connection, notes)) => {
                out.notes.extend(notes);
                let id = connection.connection_id.clone();
                if !connection_ids.insert(id.clone()) {
                    errors.push(format!(
                        "connector '{id}' ({}): two connectors share the name",
                        def.origin
                    ));
                    continue;
                }
                match pretty(&connection) {
                    Ok(bytes) => {
                        out.files.insert(format!("connections/{id}.json"), bytes);
                    }
                    Err(e) => errors.push(e),
                }
                // A bound connection is deployed with its binder: the member
                // requires each target, which the node pins the call to.
                let requires = connection
                    .bindings
                    .values()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .map(|target| CompiledRequirement {
                        kind: EntityKind::Connection,
                        id: target.clone(),
                    })
                    .collect();
                bound.push((id.clone(), def.origin.clone(), connection.bindings.clone()));
                members.push(CompiledMember {
                    kind: EntityKind::Connection,
                    id: id.clone(),
                    definition: None,
                    connection: Some(id),
                    requires,
                    rollouts: Vec::new(),
                });
                out.counts.connections += 1;
            }
            Err(e) => errors.push(e),
        }
    }
    for (id, origin, bindings) in bound {
        for (alias, target) in bindings {
            if !connection_ids.contains(&target) {
                errors.push(format!(
                    "connector '{id}' ({origin}): `bindings.{alias}` names connection \
                     '{target}', and the set has no connector of that name"
                ));
            }
        }
    }

    // Workflow id → version → definition digest. Two documents may share an
    // id only when each names a distinct explicit `version`: they are two
    // revisions of one workflow, selected by a channel's `rollouts`.
    let mut variants: BTreeMap<String, BTreeMap<u64, String>> = BTreeMap::new();
    let mut unversioned_workflows: BTreeSet<String> = BTreeSet::new();
    for def in set.iter(Entity::Workflow) {
        let (id, version, workflow) = match workflow_variant(&def.doc, &def.origin) {
            Ok(converted) => converted,
            Err(e) => {
                errors.push(e);
                continue;
            }
        };
        let mut requires = Vec::new();
        for (connection, function) in used_connections(&workflow, functions) {
            if connection_ids.contains(&connection) {
                requires.push(CompiledRequirement {
                    kind: EntityKind::Connection,
                    id: connection,
                });
            } else {
                errors.push(format!(
                    "workflow '{id}' ({}): {function} names connection '{connection}', and \
                     the set has no connector of that name — a deployment carries every \
                     connection it uses",
                    def.origin
                ));
            }
        }
        match definition_file(&Definition::Workflow(workflow)) {
            Ok((digest, path, bytes)) => {
                match version {
                    Some(version) => {
                        if unversioned_workflows.contains(&id) {
                            errors.push(format!(
                                "workflow '{id}' ({}): one definition of it names no `version` \
                                 and another names version {version}; a set with two \
                                 revisions must give every one of them a distinct `version`",
                                def.origin
                            ));
                            continue;
                        }
                        let by_version = variants.entry(id.clone()).or_default();
                        if by_version.contains_key(&version) {
                            errors.push(format!(
                                "workflow '{id}' ({}): a second definition names version \
                                 {version}; each variant needs a distinct `version`",
                                def.origin
                            ));
                            continue;
                        }
                        if let Some((other, _)) =
                            by_version.iter().find(|(_, known)| **known == digest)
                        {
                            errors.push(format!(
                                "workflow '{id}' ({}): versions {other} and {version} have \
                                 identical definitions, so the target would assign them the \
                                 same revision; a split needs two different definitions",
                                def.origin
                            ));
                            continue;
                        }
                        by_version.insert(version, digest.clone());
                    }
                    None => {
                        if variants.contains_key(&id) || !unversioned_workflows.insert(id.clone()) {
                            errors.push(format!(
                                "workflow '{id}' ({}): another definition of it is already in \
                                 the set; two revisions of one workflow need distinct explicit \
                                 `version`s",
                                def.origin
                            ));
                            continue;
                        }
                    }
                }
                out.files.insert(path, bytes);
                members.push(member(EntityKind::Workflow, id, digest, requires));
                out.counts.workflows += 1;
            }
            Err(e) => errors.push(format!("workflow '{id}' ({}): {e}", def.origin)),
        }
    }

    let workflow_ids: BTreeSet<&str> = members
        .iter()
        .filter(|m| m.kind == EntityKind::Workflow)
        .map(|m| m.id.as_str())
        .collect();
    let mut channel_members = Vec::new();
    for def in set.iter(Entity::Channel) {
        let (id, workflow_id, connections, rollouts, channel) =
            match channel_variants(&def.doc, &def.origin) {
                Ok(converted) => converted,
                Err(e) => {
                    errors.push(e);
                    continue;
                }
            };
        if !workflow_ids.contains(workflow_id.as_str()) {
            errors.push(format!(
                "channel '{id}' ({}): its workflow '{workflow_id}' is not in the set",
                def.origin
            ));
            continue;
        }
        let versions = variants.get(&workflow_id);
        let compiled_rollouts = if rollouts.is_empty() {
            if versions.is_some_and(|versions| versions.len() > 1) {
                errors.push(format!(
                    "channel '{id}' ({}): workflow '{workflow_id}' has {} versions in the set, \
                     so the channel must name which one each bucket serves with `rollouts`",
                    def.origin,
                    versions.map_or(0, BTreeMap::len)
                ));
                continue;
            }
            Vec::new()
        } else {
            let mut compiled = Vec::with_capacity(rollouts.len());
            let mut missing = false;
            for rollout in rollouts {
                let Some(definition) = versions.and_then(|versions| versions.get(&rollout.version))
                else {
                    errors.push(format!(
                        "channel '{id}' ({}): `rollouts` names version {} of workflow \
                         '{workflow_id}', and the set carries no such version",
                        def.origin, rollout.version
                    ));
                    missing = true;
                    continue;
                };
                compiled.push(CompiledRollout {
                    workflow: workflow_id.clone(),
                    definition: definition.clone(),
                    start_bucket: rollout.start_bucket,
                    end_bucket: rollout.end_bucket,
                });
            }
            if missing {
                continue;
            }
            compiled
        };
        let mut requires = vec![CompiledRequirement {
            kind: EntityKind::Workflow,
            id: workflow_id,
        }];
        let mut missing = false;
        for connection in connections {
            if connection_ids.contains(&connection) {
                requires.push(CompiledRequirement {
                    kind: EntityKind::Connection,
                    id: connection,
                });
            } else {
                errors.push(format!(
                    "channel '{id}' ({}): `connections` names '{connection}', and the set has \
                     no connector of that name — a deployment carries every connection it uses",
                    def.origin
                ));
                missing = true;
            }
        }
        if missing {
            continue;
        }
        match definition_file(&Definition::Channel(channel)) {
            Ok((digest, path, bytes)) => {
                out.files.insert(path, bytes);
                channel_members.push(member_with_rollouts(
                    EntityKind::Channel,
                    id,
                    digest,
                    requires,
                    compiled_rollouts,
                ));
                out.counts.channels += 1;
            }
            Err(e) => errors.push(format!("channel '{id}' ({}): {e}", def.origin)),
        }
    }
    members.extend(channel_members);

    for plugin in &set.plugins {
        let id = plugin.manifest.name.clone();
        let Some(path) = &plugin.component_path else {
            errors.push(format!(
                "plugin '{id}' ({}): no component beside the manifest, and a deployment must \
                 carry the bytes — build it, or name it with `component = …`",
                plugin.origin
            ));
            continue;
        };
        let converted = std::fs::read(path)
            .map_err(|e| format!("reading '{}': {e}", path.display()))
            .and_then(|bytes| {
                let manifest = serde_json::to_value(&plugin.manifest).map_err(|e| e.to_string())?;
                let mut definition = plugin_definition(manifest, &bytes);
                definition.signature = beside_signature(&id, path, &bytes)?;
                definition_file(&Definition::Plugin(definition))
            });
        match converted {
            Ok((digest, path, bytes)) => {
                out.files.insert(path, bytes);
                members.push(member(EntityKind::Plugin, id, digest, Vec::new()));
                out.counts.plugins += 1;
            }
            Err(e) => errors.push(format!("plugin '{id}' ({}): {e}", plugin.origin)),
        }
    }

    let mut seen: BTreeSet<(EntityKind, String)> = BTreeSet::new();
    for m in &members {
        // Workflows are the one kind two members may share an id under: they
        // are two revisions of one workflow, and the channel's rollouts
        // select between them.
        if m.kind == EntityKind::Workflow {
            continue;
        }
        if !seen.insert((m.kind, m.id.clone())) {
            errors.push(format!(
                "two {}s in the set share the id '{}'",
                m.kind, m.id
            ));
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    // By the kind's name, then the id and digest: the order `deployment.json`
    // has always listed members in, made deterministic when two workflow
    // variants share an id.
    members.sort_by(|a, b| {
        (
            a.kind.as_str(),
            &a.id,
            a.definition.as_deref().unwrap_or_default(),
        )
            .cmp(&(
                b.kind.as_str(),
                &b.id,
                b.definition.as_deref().unwrap_or_default(),
            ))
    });
    for m in &mut members {
        m.requires
            .sort_by(|a, b| (a.kind.as_str(), &a.id).cmp(&(b.kind.as_str(), &b.id)));
        m.requires.dedup();
        m.rollouts.sort_by(|a, b| {
            (a.start_bucket, a.end_bucket, &a.workflow, &a.definition).cmp(&(
                b.start_bucket,
                b.end_bucket,
                &b.workflow,
                &b.definition,
            ))
        });
    }
    let deployment = CompiledDeployment {
        schema_version: COMPILED_DEPLOYMENT_SCHEMA_VERSION,
        members,
    };
    out.files.insert(
        "deployment.json".to_string(),
        pretty(&deployment).map_err(|e| vec![e])?,
    );
    Ok(out)
}

fn member(
    kind: EntityKind,
    id: String,
    digest: String,
    requires: Vec<CompiledRequirement>,
) -> CompiledMember {
    member_with_rollouts(kind, id, digest, requires, Vec::new())
}

fn member_with_rollouts(
    kind: EntityKind,
    id: String,
    digest: String,
    requires: Vec<CompiledRequirement>,
    rollouts: Vec<CompiledRollout>,
) -> CompiledMember {
    CompiledMember {
        kind,
        id,
        definition: Some(digest),
        connection: None,
        requires,
        rollouts,
    }
}

/// Pretty JSON with a trailing newline. `serde_json`'s map is sorted, so the
/// output is the same for the same value.
fn pretty<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// A definition's digest, its file and its canonical bytes — the bytes the
/// server stores for it, so the digest is the one an upload answers.
fn definition_file(definition: &Definition) -> Result<(String, String, Vec<u8>), String> {
    let bytes =
        crate::runtime::definitions::canonical_bytes(definition).map_err(|e| e.to_string())?;
    let digest = crate::crypto::sha256_digest(&bytes);
    let file = format!("definitions/{}.json", digest.hex());
    Ok((digest.into_string(), file, bytes))
}

/// The object `doc` is, or an error naming `what` and `origin`.
fn object<'a>(doc: &'a Value, what: &str, origin: &str) -> Result<&'a Map<String, Value>, String> {
    doc.as_object()
        .ok_or_else(|| format!("{what} ({origin}): not a JSON object"))
}

/// The entity's id under `key`: required, because a member is addressed by
/// it, and the shape a deployment file can be named after.
fn entity_id(
    obj: &Map<String, Value>,
    key: &str,
    what: &str,
    origin: &str,
) -> Result<String, String> {
    let id = obj
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{what} ({origin}): needs an explicit `{key}`"))?;
    if !is_member_id(id) {
        return Err(format!(
            "{what} ({origin}): `{key}` '{id}' must be letters, digits, '.', '_' and '-'"
        ));
    }
    Ok(id.to_string())
}

fn is_member_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with(crate::crypto::Digest::PREFIX)
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Refuse every key of `obj` that is in neither `keys` nor `allowed`.
fn refuse_unknown_keys(
    obj: &Map<String, Value>,
    keys: &[&str],
    allowed: &[&str],
    what: &str,
) -> Result<(), String> {
    let unknown: Vec<&str> = obj
        .keys()
        .map(String::as_str)
        .filter(|k| !keys.contains(k) && !allowed.contains(k))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{what}: {} not part of a deployed definition",
        unknown
            .iter()
            .map(|k| format!("`{k}`"))
            .collect::<Vec<_>>()
            .join(", ")
            + if unknown.len() == 1 { " is" } else { " are" }
    ))
}

/// `activate: true` says what a deployment does anyway; `false` asks for
/// what it cannot do.
fn check_activate(obj: &Map<String, Value>, what: &str) -> Result<(), String> {
    match obj.get("activate") {
        None | Some(Value::Bool(true)) => Ok(()),
        Some(_) => Err(format!(
            "{what}: `activate` must be true or absent — every member of a deployment serves; \
             leave the entity out of the set instead"
        )),
    }
}

/// A set's workflow as a [`WorkflowDefinition`], with its id. Identity
/// leaves the document, and v1's `continue_on_error` becomes `on_failure`.
pub fn workflow_definition(
    doc: &Value,
    origin: &str,
) -> Result<(String, WorkflowDefinition), String> {
    let (id, _, definition) = workflow_variant(doc, origin)?;
    Ok((id, definition))
}

/// A set's workflow as a [`WorkflowDefinition`], with its id and the
/// authoring `version` that names it among the variants of one id.
///
/// `version` is a selector, not a deployment revision: the deployer assigns
/// revisions from the target's history. It is stripped from the definition,
/// like the rest of the identity.
pub fn workflow_variant(
    doc: &Value,
    origin: &str,
) -> Result<(String, Option<u64>, WorkflowDefinition), String> {
    let obj = object(doc, "workflow", origin)?;
    let id = entity_id(obj, "workflow_id", "workflow", origin)?;
    let what = format!("workflow '{id}' ({origin})");
    refuse_unknown_keys(
        obj,
        WORKFLOW_KEYS,
        &[WORKFLOW_IDENTITY, &["continue_on_error", "activate"]].concat(),
        &what,
    )?;
    check_activate(obj, &what)?;
    let version = identity_version(obj, &what)?;

    let mut definition: Map<String, Value> = obj
        .iter()
        .filter(|(k, v)| WORKFLOW_KEYS.contains(&k.as_str()) && !v.is_null())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    match obj.get("continue_on_error") {
        None | Some(Value::Null) => {}
        Some(Value::Bool(continues)) => {
            let policy = if *continues { "continue" } else { "fail" };
            if let Some(stated) = definition.get("on_failure")
                && stated != policy
            {
                return Err(format!(
                    "{what}: `continue_on_error: {continues}` and `on_failure: {stated}` disagree"
                ));
            }
            definition.insert("on_failure".to_string(), Value::String(policy.to_string()));
        }
        Some(other) => {
            return Err(format!(
                "{what}: `continue_on_error` must be a boolean, not {other}"
            ));
        }
    }
    let workflow: WorkflowDefinition =
        serde_json::from_value(Value::Object(definition)).map_err(|e| format!("{what}: {e}"))?;
    Ok((id, version, workflow))
}

/// The identity `version` a definition carries, when it carries one. A
/// version of `0` is refused: it reads as "no revision", which is not a
/// variant an author can mean.
pub(crate) fn identity_version(
    obj: &Map<String, Value>,
    what: &str,
) -> Result<Option<u64>, String> {
    match obj.get("version") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_u64() {
            Some(version) if version >= 1 => Ok(Some(version)),
            _ => Err(format!(
                "{what}: `version` must be an integer of at least 1, not {value}"
            )),
        },
    }
}

/// One bucket interval of a channel's traffic split, selecting a workflow
/// `version` of the channel's own workflow id.
///
/// The `version` is the authoring selector a workflow document carries. The
/// target still assigns deployment revisions; this never names one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChannelRollout {
    pub version: u64,
    pub start_bucket: u8,
    pub end_bucket: u8,
}

/// A channel's compiled pieces: id, workflow id, driver connections,
/// optional split, and the definition itself.
pub type ChannelVariant = (
    String,
    String,
    Vec<String>,
    Vec<ChannelRollout>,
    ChannelDefinition,
);

/// Validate that `rollouts` partition `[0, 100)` with no gaps, overlaps or
/// repeated versions — the rule the server applies to a new revision's
/// intervals, applied offline so a set fails before it is deployed.
pub(crate) fn validate_channel_rollouts(rollouts: &[ChannelRollout]) -> Result<(), String> {
    if rollouts.is_empty() {
        return Err("at least one rollout interval is required".to_string());
    }
    let mut sorted = rollouts.to_vec();
    sorted.sort_by_key(|rollout| (rollout.start_bucket, rollout.end_bucket));
    let mut seen = BTreeSet::new();
    let mut cursor = 0u8;
    for rollout in &sorted {
        if rollout.version == 0 || !seen.insert(rollout.version) {
            return Err("duplicate or zero workflow version in rollout".to_string());
        }
        if rollout.start_bucket >= rollout.end_bucket || rollout.end_bucket > 100 {
            return Err(format!(
                "rollout for version {} has invalid bucket bounds [{}, {})",
                rollout.version, rollout.start_bucket, rollout.end_bucket
            ));
        }
        if rollout.start_bucket != cursor {
            return Err(format!(
                "rollout intervals have a gap or overlap at bucket {cursor} vs {}",
                rollout.start_bucket
            ));
        }
        cursor = rollout.end_bucket;
    }
    if cursor != 100 {
        return Err(format!("rollout intervals end at {cursor}, expected 100"));
    }
    Ok(())
}

/// The `rollouts` list a channel document carries, validated. Absent or
/// `null` is no split.
pub(crate) fn parse_channel_rollouts(
    doc: &Value,
    what: &str,
) -> Result<Vec<ChannelRollout>, String> {
    let Some(obj) = doc.as_object() else {
        return Ok(Vec::new());
    };
    rollouts_from_object(obj, what)
}

fn rollouts_from_object(
    obj: &Map<String, Value>,
    what: &str,
) -> Result<Vec<ChannelRollout>, String> {
    let Some(value) = obj.get("rollouts") else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let Some(items) = value.as_array() else {
        return Err(format!(
            "{what}: `rollouts` is a list of bucket intervals, not {value}"
        ));
    };
    let mut rollouts = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let at = format!("`rollouts[{index}]`");
        let Some(map) = item.as_object() else {
            return Err(format!("{what}: {at} is not an object"));
        };
        if let Some(key) = map
            .keys()
            .find(|key| !matches!(key.as_str(), "version" | "start_bucket" | "end_bucket"))
        {
            return Err(format!(
                "{what}: {at} has `{key}`, which is not part of a bucket interval \
                 (`version`, `start_bucket`, `end_bucket`)"
            ));
        }
        let version = map
            .get("version")
            .and_then(Value::as_u64)
            .filter(|version| *version >= 1)
            .ok_or_else(|| format!("{what}: {at} needs a `version` of at least 1"))?;
        let bucket = |key: &str| -> Result<u8, String> {
            map.get(key)
                .and_then(Value::as_u64)
                .and_then(|bucket| u8::try_from(bucket).ok())
                .filter(|bucket| *bucket <= 100)
                .ok_or_else(|| format!("{what}: {at} needs `{key}` as a bucket in 0..=100"))
        };
        rollouts.push(ChannelRollout {
            version,
            start_bucket: bucket("start_bucket")?,
            end_bucket: bucket("end_bucket")?,
        });
    }
    validate_channel_rollouts(&rollouts).map_err(|error| format!("{what}: {error}"))?;
    Ok(rollouts)
}

/// A set's channel as a [`ChannelDefinition`], with its id, the id of the
/// workflow it serves and the connections its driver connects with (a Kafka
/// channel's cluster). Both become its member's requirements; neither is part
/// of the definition, so neither moves its digest.
pub fn channel_definition(
    doc: &Value,
    origin: &str,
) -> Result<(String, String, Vec<String>, ChannelDefinition), String> {
    let (id, workflow_id, connections, _, channel) = channel_variants(doc, origin)?;
    Ok((id, workflow_id, connections, channel))
}

/// A set's channel with its optional traffic split, alongside the id of the
/// workflow it serves and the connections its driver connects with.
pub fn channel_variants(doc: &Value, origin: &str) -> Result<ChannelVariant, String> {
    let obj = object(doc, "channel", origin)?;
    let id = entity_id(obj, "channel_id", "channel", origin)?;
    let what = format!("channel '{id}' ({origin})");
    // `topic` and `consumer_group` are v1 Kafka fields a definition has no
    // place for; absent or null, they say nothing.
    let present = |key: &str| obj.get(key).is_some_and(|v| !v.is_null());
    let without_nulls: Map<String, Value> = obj
        .iter()
        .filter(|(k, v)| !(matches!(k.as_str(), "topic" | "consumer_group") && v.is_null()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    refuse_unknown_keys(
        &without_nulls,
        CHANNEL_KEYS,
        &[
            CHANNEL_IDENTITY,
            &["workflow_id", "activate", "connections", "rollouts"],
        ]
        .concat(),
        &what,
    )?;
    check_activate(obj, &what)?;
    if !present("workflow_id") {
        return Err(format!(
            "{what}: names no `workflow_id`, and a deployed channel serves one workflow"
        ));
    }
    let workflow_id = entity_id(obj, "workflow_id", "channel", origin)?;
    let rollouts = rollouts_from_object(obj, &what)?;
    let connections = match obj.get("connections") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => {
            let mut ids = Vec::with_capacity(items.len());
            for item in items {
                match item.as_str() {
                    Some(id) if is_member_id(id) => {
                        if !ids.iter().any(|seen| seen == id) {
                            ids.push(id.to_string());
                        }
                    }
                    _ => {
                        return Err(format!(
                            "{what}: `connections` names connector ids, and {item} is not one"
                        ));
                    }
                }
            }
            ids
        }
        Some(other) => {
            return Err(format!(
                "{what}: `connections` is a list of connector ids, not {other}"
            ));
        }
    };

    let definition: Map<String, Value> = obj
        .iter()
        .filter(|(k, v)| CHANNEL_KEYS.contains(&k.as_str()) && !v.is_null())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let channel: ChannelDefinition =
        serde_json::from_value(Value::Object(definition)).map_err(|e| format!("{what}: {e}"))?;
    Ok((id, workflow_id, connections, rollouts, channel))
}

/// Every connection `workflow` uses, each with a function that names it:
/// the `connector` of every task whose function takes one — `http_call`,
/// `db_read`/`db_write`, `data_query`/`data_write`, the `cache_*` family,
/// whatever `functions` says carries a [`ConnectorRule`] — and the
/// `connection` of every `connector_call`. Task groups and loop setup
/// included; a computed name is not a literal and is not counted.
///
/// A workflow member requires all of them, because the deployment must
/// carry each. Requiring one grants nothing by itself: connector authority
/// (`runtime::authority`) admits only the operations a workflow's own
/// `connector_call` tasks name on a connection it requires.
///
/// [`ConnectorRule`]: crate::engine::ConnectorRule
pub(crate) fn used_connections(
    workflow: &WorkflowDefinition,
    functions: &crate::engine::FunctionRegistry,
) -> BTreeMap<String, String> {
    let loop_config = workflow.loop_config.as_ref();
    let mut used = BTreeMap::new();
    for reference in crate::engine::connector_refs(&workflow.tasks, loop_config, functions) {
        used.entry(reference.connector.to_string())
            .or_insert_with(|| reference.function.to_string());
    }
    for task in crate::engine::leaf_tasks(&workflow.tasks, loop_config) {
        if task["function"]["name"] != crate::engine::functions::connector_call::NAME {
            continue;
        }
        if let Some(connection) = task["function"]["input"]["connection"].as_str() {
            used.entry(connection.to_string())
                .or_insert_with(|| crate::engine::functions::connector_call::NAME.to_string());
        }
    }
    used
}

/// The detached signature beside a plugin's component, when there is one:
/// `<id>.sig` or `<component file>.sig`, as `orion-cli plugin sign`
/// writes it. A node with `[plugins.trust]` refuses an unsigned plugin, so a
/// signed component must reach the deployment with its signature.
fn beside_signature(id: &str, component: &Path, bytes: &[u8]) -> Result<Option<String>, String> {
    use crate::signatures::{Kind, SignatureDir, Subject};
    let dir = SignatureDir::open(component.parent().unwrap_or_else(|| Path::new(".")))?;
    let subject = Subject {
        kind: Kind::Plugin,
        id: id.to_string(),
        file_names: component
            .file_name()
            .map(|n| vec![n.to_string_lossy().into_owned()])
            .unwrap_or_default(),
        digest: crate::plugin::WasmRuntime::digest(bytes),
        carried: None,
    };
    dir.lookup(&subject)
        .map(crate::signatures::read_sig_file)
        .transpose()
}

/// A plugin manifest and its component as a [`PluginDefinition`].
pub fn plugin_definition(manifest: Value, component: &[u8]) -> PluginDefinition {
    use base64::Engine as _;
    PluginDefinition {
        manifest,
        component: base64::engine::general_purpose::STANDARD.encode(component),
        signature: None,
        tags: Vec::new(),
    }
}

/// A set's connector as a connection over the built-in driver its type
/// names. Every secret reference in its config moves to `secret_locators`
/// at its dotted path; the rest is `public_config`, checked as a revision
/// create checks it, so a literal credential is refused here.
pub fn connection(doc: &Value, origin: &str) -> Result<(CompiledConnection, Vec<String>), String> {
    let obj = object(doc, "connector", origin)?;
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("connector ({origin}): needs a `name`"))?;
    if !is_member_id(name) {
        return Err(format!(
            "connector '{name}' ({origin}): a connection id must be letters, digits, '.', '_' \
             and '-'"
        ));
    }
    let what = format!("connector '{name}' ({origin})");
    refuse_unknown_keys(
        obj,
        &["name", "connector_type", "provider", "config", "bindings"],
        &["id", "tags", "enabled"],
        &what,
    )?;
    match obj.get("enabled") {
        None | Some(Value::Null) | Some(Value::Bool(true)) => {}
        Some(_) => {
            return Err(format!(
                "{what}: `enabled` must be true or absent — every connection of a deployment \
                 loads; leave the connector out of the set instead"
            ));
        }
    }
    let mut notes = Vec::new();
    if obj
        .get("tags")
        .and_then(Value::as_array)
        .is_some_and(|tags| !tags.is_empty())
    {
        notes.push(format!(
            "note: {what}: a connection has no tags; its tags are not carried"
        ));
    }

    let mut config = obj
        .get("config")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let Some(settings) = config.as_object_mut() else {
        return Err(format!("{what}: `config` is not an object"));
    };
    settings.remove("type");

    let connector_type = obj
        .get("connector_type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // `provider` names a Direct Rust driver compiled into the target node by
    // its id — Community's (`org.orion.http`) or an edition's
    // (`com.orion.enterprise.kafka`). Its digest is derived from the id, the
    // way the node derives it, so no package needs to exist offline. A
    // `connector_type` maps to one of Community's drivers instead.
    let driver = match (obj.get("provider"), obj.contains_key("connector_type")) {
        (Some(_), true) => {
            return Err(format!(
                "{what}: names both `provider` and `connector_type`; a connection runs on one \
                 provider"
            ));
        }
        (Some(Value::String(id)), false) if is_member_id(id) => id.clone(),
        (Some(other), false) => {
            return Err(format!(
                "{what}: `provider` is a driver id such as \"org.orion.http\", not {other}"
            ));
        }
        (None, _) => {
            let driver = driver_for(connector_type, &config).map_err(|e| format!("{what}: {e}"))?;
            if connector_type == "db" {
                // It became the provider; the provider's settings have no such key.
                if let Some(settings) = config.as_object_mut() {
                    settings.remove("driver");
                }
            }
            driver.to_string()
        }
    };
    let provider_digest = foundation::synthetic_provider_digest(&driver).into_string();

    // `bindings` name the set's connections a provider's declared
    // dependencies call. Only a `provider` declares dependencies: none of
    // the `connector_type` drivers does.
    let bindings: BTreeMap<String, String> = match obj.get("bindings") {
        None | Some(Value::Null) => BTreeMap::new(),
        Some(_) if !obj.contains_key("provider") => {
            return Err(format!(
                "{what}: `bindings` binds a provider's declared dependencies; name the \
                 `provider` that declares them"
            ));
        }
        Some(Value::Object(map)) => map
            .iter()
            .map(|(alias, target)| match target.as_str() {
                Some(target) if is_member_id(target) => Ok((alias.clone(), target.to_string())),
                _ => Err(format!(
                    "{what}: `bindings.{alias}` must name a connection of the set, not {target}"
                )),
            })
            .collect::<Result<_, _>>()?,
        Some(other) => {
            return Err(format!(
                "{what}: `bindings` maps each dependency alias to a connection, not {other}"
            ));
        }
    };
    if let Some((alias, _)) = bindings.iter().find(|(_, target)| target.as_str() == name) {
        return Err(format!(
            "{what}: `bindings.{alias}` names the connection itself; a connection cannot be \
             its own dependency"
        ));
    }

    let mut secret_locators = BTreeMap::new();
    let mut misplaced = Vec::new();
    extract_references(&mut config, "", &mut secret_locators, &mut misplaced);
    if let Some(path) = misplaced.first() {
        return Err(format!(
            "{what}: the secret reference at `{path}` sits where no locator can name it (a \
             locator path is dotted keys of letters, digits and '_', with no arrays)"
        ));
    }
    crate::extensions::deployment_store::check_connection_config(&config, &secret_locators)
        .map_err(|e| {
            format!(
                "{what}: {} — a deployment carries references only; write the credential as \
                 a secret reference such as env://VAR",
                e.message
            )
        })?;
    // The judgement `POST /api/v2/admin/connections` makes, so a set that
    // compiles deploys.
    if let Some(problems) =
        crate::validation::builtin_connection_problems(&provider_digest, &config, &secret_locators)
        && !problems.is_empty()
    {
        let said: Vec<String> = problems
            .iter()
            .map(|problem| match problem.path.as_str() {
                "" => problem.message.clone(),
                path => format!("`config.{path}`: {}", problem.message),
            })
            .collect();
        return Err(format!(
            "{what}: its configuration would not load — {}",
            said.join("; ")
        ));
    }

    Ok((
        CompiledConnection {
            connection_id: name.to_string(),
            driver: driver.to_string(),
            provider_digest,
            public_config: config,
            secret_locators,
            bindings,
        },
        notes,
    ))
}

/// The built-in driver a v1 connector of `connector_type` with `config`
/// runs on: `http`'s, a `db` connector's by its `driver` field, a memory
/// cache's.
fn driver_for(connector_type: &str, config: &Value) -> Result<&'static str, String> {
    match connector_type {
        "http" => Ok(foundation::DRIVER_HTTP),
        // The driver is the field's to name: a connection string that is a
        // secret reference or a `${VAR}` placeholder shows no scheme until
        // it is resolved, which is why a set must state it
        // (`connector.db_driver`), and the admin validator has already held
        // it to any scheme that is visible.
        "db" => match config.get("driver") {
            None => Err(format!(
                "a db connector names its `driver` ({}) for a deployment to name its provider",
                crate::connector::DbDriver::NAMES.join(", ")
            )),
            Some(raw) => match raw.as_str().and_then(crate::connector::DbDriver::from_name) {
                Some(crate::connector::DbDriver::Postgresql) => Ok(foundation::DRIVER_POSTGRESQL),
                Some(crate::connector::DbDriver::Mysql) => Ok(foundation::DRIVER_MYSQL),
                Some(crate::connector::DbDriver::Sqlite) => Ok(foundation::DRIVER_SQLITE),
                None => Err(format!(
                    "db driver {raw} is not one of {}",
                    crate::connector::DbDriver::NAMES.join(", ")
                )),
            },
        },
        "cache" => match config.get("backend").and_then(Value::as_str) {
            Some("memory") | None => Ok(foundation::DRIVER_MEMORY_CACHE),
            Some(other) => Err(format!(
                "cache backend '{other}' has no built-in driver; only `memory` does"
            )),
        },
        other => Err(format!(
            "connector type '{other}' has no built-in driver a deployment can name (http, db \
             and cache do)"
        )),
    }
}

/// Move every string under `value` that is a secret reference into
/// `locators` at its dotted path. A reference with no dotted path — inside
/// an array, or under a key a locator path cannot spell — goes to
/// `misplaced`.
fn extract_references(
    value: &mut Value,
    path: &str,
    locators: &mut BTreeMap<String, String>,
    misplaced: &mut Vec<String>,
) {
    match value {
        Value::Object(map) => {
            let mut moved = Vec::new();
            for (key, child) in map.iter_mut() {
                let child_path = crate::definitions::expand::join(path, key);
                let spellable =
                    !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
                if let Value::String(s) = child
                    && crate::connector::secrets::is_secret_reference(s)
                {
                    if spellable && !path.contains('[') {
                        locators.insert(child_path, s.clone());
                        moved.push(key.clone());
                    } else {
                        misplaced.push(child_path);
                    }
                    continue;
                }
                let child_path = if spellable {
                    child_path
                } else {
                    // Not a locator path; anything below it is misplaced.
                    format!("{path}[{key}]")
                };
                extract_references(child, &child_path, locators, misplaced);
            }
            for key in moved {
                map.remove(&key);
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter_mut().enumerate() {
                let item_path = format!("{path}[{i}]");
                if let Value::String(s) = item
                    && crate::connector::secrets::is_secret_reference(s)
                {
                    misplaced.push(item_path);
                    continue;
                }
                extract_references(item, &item_path, locators, misplaced);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orion_api::v2::WorkflowOnFailure;
    use serde_json::json;

    #[test]
    fn a_workflow_leaves_its_identity_and_maps_continue_on_error() {
        let (id, wf) = workflow_definition(
            &json!({"workflow_id": "w", "version": 3, "status": "active", "name": "W",
                    "tasks": [], "continue_on_error": true}),
            "wf.json",
        )
        .expect("converts");
        assert_eq!(id, "w");
        assert_eq!(wf.on_failure, WorkflowOnFailure::Continue);
    }

    /// Two workflow documents may share an id when each names a distinct
    /// `version`; a channel's `rollouts` then selects between them, and the
    /// compiled member carries each variant's digest.
    #[test]
    fn two_workflow_versions_compile_to_a_channel_rollout() {
        let def = |entity, doc, origin: &str| super::super::set::Definition {
            entity,
            origin: origin.to_string(),
            doc,
            spans: None,
            provenance: Default::default(),
        };
        let set = DefinitionSet {
            definitions: vec![
                def(
                    Entity::Workflow,
                    json!({"workflow_id": "checkout", "version": 1, "name": "V1",
                           "tasks": []}),
                    "v1.json",
                ),
                def(
                    Entity::Workflow,
                    json!({"workflow_id": "checkout", "version": 2, "name": "V2",
                           "tasks": []}),
                    "v2.json",
                ),
                def(
                    Entity::Channel,
                    json!({"channel_id": "orders", "name": "Orders", "protocol": "rest",
                    "workflow_id": "checkout",
                    "rollouts": [
                        {"version": 1, "start_bucket": 0, "end_bucket": 50},
                        {"version": 2, "start_bucket": 50, "end_bucket": 100}
                    ]}),
                    "channel.json",
                ),
            ],
            plugins: Vec::new(),
        };
        let out = build(&set).expect("compiles");
        let deployment: CompiledDeployment =
            serde_json::from_slice(&out.files["deployment.json"]).expect("a manifest");
        assert_eq!(deployment.schema_version, 2);
        let workflows: Vec<&CompiledMember> = deployment
            .members
            .iter()
            .filter(|member| member.kind == EntityKind::Workflow)
            .collect();
        assert_eq!(workflows.len(), 2, "one member per variant");
        assert!(workflows.iter().all(|member| member.id == "checkout"));
        assert_ne!(workflows[0].definition, workflows[1].definition);

        let channel = deployment
            .members
            .iter()
            .find(|member| member.kind == EntityKind::Channel)
            .expect("the channel");
        assert_eq!(channel.rollouts.len(), 2);
        assert_eq!(channel.rollouts[0].workflow, "checkout");
        assert_eq!(channel.rollouts[0].start_bucket, 0);
        assert_eq!(channel.rollouts[0].end_bucket, 50);
        assert_eq!(channel.rollouts[1].start_bucket, 50);
        assert_eq!(channel.rollouts[1].end_bucket, 100);
        assert_eq!(
            channel.rollouts[0].definition,
            workflows[0].definition.as_deref().unwrap_or_default()
        );
        assert_eq!(
            channel.rollouts[1].definition,
            workflows[1].definition.as_deref().unwrap_or_default()
        );
        assert!(
            out.files.contains_key(
                channel.rollouts[0]
                    .definition
                    .strip_prefix("sha256:")
                    .map(|hex| format!("definitions/{hex}.json"))
                    .expect("a digest")
                    .as_str()
            ),
            "every rollout names a definition file"
        );
    }

    /// A set with one revision per workflow still compiles to the old shape:
    /// no `rollouts` key, only the version-2 schema number.
    #[test]
    fn a_single_revision_channel_has_no_rollouts() {
        let def = |entity, doc, origin: &str| super::super::set::Definition {
            entity,
            origin: origin.to_string(),
            doc,
            spans: None,
            provenance: Default::default(),
        };
        let set = DefinitionSet {
            definitions: vec![
                def(
                    Entity::Workflow,
                    json!({"workflow_id": "checkout", "name": "Checkout", "tasks": []}),
                    "workflow.json",
                ),
                def(
                    Entity::Channel,
                    json!({"channel_id": "orders", "name": "Orders", "protocol": "rest",
                           "workflow_id": "checkout"}),
                    "channel.json",
                ),
            ],
            plugins: Vec::new(),
        };
        let out = build(&set).expect("compiles");
        let deployment: CompiledDeployment =
            serde_json::from_slice(&out.files["deployment.json"]).expect("a manifest");
        assert_eq!(deployment.schema_version, 2);
        assert!(
            deployment
                .members
                .iter()
                .all(|member| member.rollouts.is_empty()),
            "an unsplit deployment carries no rollout metadata"
        );
        let json: Value = serde_json::from_slice(&out.files["deployment.json"]).expect("json");
        assert!(
            !json.to_string().contains("\"rollouts\""),
            "the key is omitted, not empty: {json}"
        );
    }

    /// A canary's split has to be unambiguous and total. Each case is
    /// refused by name rather than compiled into something the server will
    /// reject later.
    #[test]
    fn an_invalid_or_ambiguous_split_is_refused() {
        let def = |entity, doc, origin: &str| super::super::set::Definition {
            entity,
            origin: origin.to_string(),
            doc,
            spans: None,
            provenance: Default::default(),
        };
        let workflow = |version: u64, name: &str| {
            def(
                Entity::Workflow,
                json!({"workflow_id": "checkout", "version": version, "name": name,
                       "tasks": []}),
                &format!("v{version}.json"),
            )
        };
        let channel = |rollouts: Value| {
            def(
                Entity::Channel,
                json!({"channel_id": "orders", "name": "Orders", "protocol": "rest",
                       "workflow_id": "checkout", "rollouts": rollouts}),
                "channel.json",
            )
        };
        let build_set = |definitions| {
            build(&DefinitionSet {
                definitions,
                plugins: Vec::new(),
            })
        };

        // A gap and an overlap.
        for rollouts in [
            json!([
                {"version": 1, "start_bucket": 0, "end_bucket": 40},
                {"version": 2, "start_bucket": 50, "end_bucket": 100}
            ]),
            json!([
                {"version": 1, "start_bucket": 0, "end_bucket": 60},
                {"version": 2, "start_bucket": 50, "end_bucket": 100}
            ]),
        ] {
            let errors = build_set(vec![
                workflow(1, "V1"),
                workflow(2, "V2"),
                channel(rollouts),
            ])
            .expect_err("refused");
            assert!(
                errors.iter().any(|error| error.contains("gap or overlap")
                    || error.contains("invalid bucket bounds")),
                "{errors:?}"
            );
        }

        // A version the set does not carry.
        let errors = build_set(vec![
            workflow(1, "V1"),
            workflow(2, "V2"),
            channel(json!([
                {"version": 1, "start_bucket": 0, "end_bucket": 50},
                {"version": 3, "start_bucket": 50, "end_bucket": 100}
            ])),
        ])
        .expect_err("refused");
        assert!(
            errors.iter().any(|error| error.contains("no such version")),
            "{errors:?}"
        );

        // A multi-version workflow with no split at all.
        let errors = build_set(vec![
            workflow(1, "V1"),
            workflow(2, "V2"),
            def(
                Entity::Channel,
                json!({"channel_id": "orders", "name": "Orders", "protocol": "rest",
                   "workflow_id": "checkout"}),
                "channel.json",
            ),
        ])
        .expect_err("refused");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("must name which one each bucket serves")),
            "{errors:?}"
        );

        // Two versions with the same definition would collapse to one
        // revision at the target.
        let errors = build_set(vec![
            workflow(1, "Same"),
            workflow(2, "Same"),
            channel(json!([
                {"version": 1, "start_bucket": 0, "end_bucket": 50},
                {"version": 2, "start_bucket": 50, "end_bucket": 100}
            ])),
        ])
        .expect_err("refused");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("identical definitions")),
            "{errors:?}"
        );

        // A repeated version.
        let errors = build_set(vec![workflow(1, "V1"), workflow(1, "V2")]).expect_err("refused");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("distinct `version`")),
            "{errors:?}"
        );
    }

    /// A connector over a `provider` may bind the provider's dependencies to
    /// other connections of the set: the compiled connection carries them,
    /// and its member requires each target, so they deploy together.
    #[test]
    fn a_connection_binds_over_a_provider_and_requires_its_targets() {
        let def = |doc: Value, origin: &str| super::super::set::Definition {
            entity: Entity::Connector,
            origin: origin.to_string(),
            doc,
            spans: None,
            provenance: Default::default(),
        };
        let set = DefinitionSet {
            definitions: vec![
                def(
                    json!({"name": "store", "provider": "com.orion.storage", "config": {}}),
                    "store.json",
                ),
                def(
                    json!({"name": "models", "provider": "com.orion.models", "config": {},
                           "bindings": {"artifact_store": "store"}}),
                    "models.json",
                ),
            ],
            plugins: Vec::new(),
        };
        let out = build(&set).expect("compiles");
        let compiled: CompiledConnection =
            serde_json::from_slice(&out.files["connections/models.json"]).expect("a connection");
        assert_eq!(compiled.bindings["artifact_store"], "store");
        let unbound: Value =
            serde_json::from_slice(&out.files["connections/store.json"]).expect("a connection");
        assert!(unbound.get("bindings").is_none(), "{unbound}");
        let manifest: Value =
            serde_json::from_slice(&out.files["deployment.json"]).expect("a manifest");
        let models = manifest["members"]
            .as_array()
            .and_then(|m| m.iter().find(|m| m["id"] == "models"))
            .expect("the member");
        assert_eq!(
            models["requires"],
            json!([{"kind": "connection", "id": "store"}]),
            "{manifest}"
        );

        // A target the set does not carry, bindings without a provider, and
        // a connection bound to itself are each refused by name.
        let missing = DefinitionSet {
            definitions: vec![def(
                json!({"name": "models", "provider": "com.orion.models", "config": {},
                       "bindings": {"artifact_store": "nowhere"}}),
                "models.json",
            )],
            plugins: Vec::new(),
        };
        let errors = build(&missing).expect_err("refused");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("`bindings.artifact_store`") && e.contains("nowhere")),
            "{errors:?}"
        );
        let typed = connection(
            &json!({"name": "c", "connector_type": "http",
                    "config": {"base_url": "https://x.example"}, "bindings": {"a": "b"}}),
            "c.json",
        )
        .expect_err("refused");
        assert!(typed.contains("name the `provider`"), "{typed}");
        let itself = connection(
            &json!({"name": "c", "provider": "com.orion.models", "config": {},
                    "bindings": {"a": "c"}}),
            "c.json",
        )
        .expect_err("refused");
        assert!(itself.contains("its own dependency"), "{itself}");
    }

    #[test]
    fn an_unknown_workflow_key_is_named() {
        let err = workflow_definition(
            &json!({"workflow_id": "w", "name": "W", "tasks": [], "rollout_percentage": 50}),
            "wf.json",
        )
        .expect_err("refused");
        assert!(
            err.contains("workflow 'w' (wf.json)") && err.contains("`rollout_percentage`"),
            "{err}"
        );
    }

    #[test]
    fn a_channel_requires_its_workflow_and_refuses_a_topic() {
        let (id, wf, connections, ch) = channel_definition(
            &json!({"channel_id": "c", "name": "c", "protocol": "rest", "workflow_id": "w",
                    "topic": null}),
            "ch.json",
        )
        .expect("converts");
        assert_eq!(
            (id.as_str(), wf.as_str(), ch.name.as_str()),
            ("c", "w", "c")
        );
        assert!(connections.is_empty());
        let err = channel_definition(
            &json!({"channel_id": "c", "name": "c", "protocol": "rest", "workflow_id": "w",
                    "topic": "orders"}),
            "ch.json",
        )
        .expect_err("refused");
        assert!(err.contains("`topic`"), "{err}");
    }

    #[test]
    fn a_channel_names_the_connections_its_driver_connects_with() {
        let (_, _, connections, ch) = channel_definition(
            &json!({"channel_id": "c", "name": "c", "protocol": "kafka", "workflow_id": "w",
                    "connections": ["events", "events"]}),
            "ch.json",
        )
        .expect("converts");
        assert_eq!(connections, ["events"], "named once, in order");
        assert!(
            !serde_json::to_value(&ch)
                .expect("serializes")
                .to_string()
                .contains("events"),
            "the requirement is the member's, not the definition's"
        );
        let err = channel_definition(
            &json!({"channel_id": "c", "name": "c", "protocol": "kafka", "workflow_id": "w",
                    "connections": "events"}),
            "ch.json",
        )
        .expect_err("refused");
        assert!(err.contains("`connections`"), "{err}");
    }

    #[test]
    fn a_connector_names_a_compiled_in_driver_by_id() {
        let (conn, _) = connection(
            &json!({"name": "events", "provider": "com.orion.enterprise.kafka",
                    "config": {"brokers": ["kafka-1:9092"],
                               "sasl_password": "env://KAFKA_PASSWORD"}}),
            "events.json",
        )
        .expect("converts");
        assert_eq!(
            conn.provider_digest,
            foundation::synthetic_provider_digest("com.orion.enterprise.kafka").into_string(),
            "the digest the node derives from the same id"
        );
        assert!(conn.secret_locators.contains_key("sasl_password"));

        // A built-in driver named by id is the digest its connector type names.
        let (http, _) = connection(
            &json!({"name": "api", "provider": "org.orion.http",
                    "config": {"url": "https://api.example.com"}}),
            "api.json",
        )
        .expect("converts");
        assert_eq!(
            Some(http.provider_digest.as_str()),
            foundation::builtin_provider_digest(foundation::DRIVER_HTTP)
                .as_ref()
                .map(|d| d.as_str())
        );

        let both = connection(
            &json!({"name": "x", "provider": "org.orion.http", "connector_type": "http",
                    "config": {}}),
            "x.json",
        )
        .expect_err("refused");
        assert!(
            both.contains("both `provider` and `connector_type`"),
            "{both}"
        );
        let bad = connection(&json!({"name": "x", "provider": 7, "config": {}}), "x.json")
            .expect_err("refused");
        assert!(bad.contains("`provider`"), "{bad}");
    }

    #[test]
    fn references_move_to_locators_and_the_rest_stays_public() {
        let (conn, _) = connection(
            &json!({"name": "api", "connector_type": "http", "config": {
                "type": "http", "url": "https://api.example.com",
                "auth": {"type": "bearer", "token": "env://API_TOKEN"}}}),
            "conn.json",
        )
        .expect("converts");
        assert_eq!(conn.driver, foundation::DRIVER_HTTP);
        assert_eq!(conn.secret_locators["auth.token"], "env://API_TOKEN");
        assert_eq!(
            conn.public_config,
            json!({"url": "https://api.example.com", "auth": {"type": "bearer"}})
        );
    }

    #[test]
    fn a_workflow_uses_every_connection_its_tasks_name() {
        let (_, wf) = workflow_definition(
            &json!({"workflow_id": "w", "name": "W",
                "loop": {"setup": [
                    {"id": "warm", "name": "Warm", "function": {"name": "cache_read",
                        "input": {"connector": "cache", "key": "k", "output": "data.k"}}}]},
                "tasks": [
                    {"id": "api", "name": "Api", "function": {"name": "http_call",
                        "input": {"connector": "partner", "method": "GET", "path": "/x"}}},
                    {"id": "g", "tasks": [
                        {"id": "read", "name": "Read", "function": {"name": "db_read",
                            "input": {"connector": "orders-db", "query": "SELECT 1"}}}]},
                    {"id": "call", "name": "Call", "function": {"name": "connector_call",
                        "input": {"connection": "billing", "target_id": "org.orion.http",
                                  "operation": "http.get", "input": {}}}},
                    {"id": "computed", "name": "Computed", "function": {"name": "db_read",
                        "input": {"connector": {"var": "c"}, "query": "SELECT 1"}}}]}),
            "wf.json",
        )
        .expect("converts");
        let used = used_connections(&wf, crate::engine::FunctionRegistry::builtin());
        assert_eq!(
            used.into_iter().collect::<Vec<_>>(),
            [
                ("billing".to_string(), "connector_call".to_string()),
                ("cache".to_string(), "cache_read".to_string()),
                ("orders-db".to_string(), "db_read".to_string()),
                ("partner".to_string(), "http_call".to_string()),
            ]
        );
    }

    #[test]
    fn a_reference_in_an_array_is_refused() {
        let err = connection(
            &json!({"name": "api", "connector_type": "http", "config": {
                "url": "https://api.example.com", "extra": ["env://X"]}}),
            "conn.json",
        )
        .expect_err("refused");
        assert!(err.contains("extra[0]"), "{err}");
    }

    #[test]
    fn the_driver_follows_the_field_and_leaves_the_settings() {
        for (field, url, driver) in [
            (
                "postgresql",
                "postgres://db/app",
                foundation::DRIVER_POSTGRESQL,
            ),
            ("mysql", "mariadb://db/app", foundation::DRIVER_MYSQL),
            ("sqlite", "sqlite::memory:", foundation::DRIVER_SQLITE),
            // A reference shows no scheme; the field still names the driver.
            (
                "postgresql",
                "env://ORDERS_DB_URL",
                foundation::DRIVER_POSTGRESQL,
            ),
            (
                "sqlite",
                "${LEADERBOARD_URL:-sqlite::memory:}",
                foundation::DRIVER_SQLITE,
            ),
        ] {
            let (conn, _) = connection(
                &json!({"name": "db", "connector_type": "db",
                        "config": {"connection_string": url, "driver": field}}),
                "db.json",
            )
            .expect("converts");
            assert_eq!(conn.driver, driver, "{url}");
            assert!(conn.public_config.get("driver").is_none(), "{url}");
        }
        let err = connection(
            &json!({"name": "db", "connector_type": "db",
                    "config": {"connection_string": "env://DB"}}),
            "db.json",
        )
        .expect_err("no driver, no provider");
        assert!(err.contains("`driver`"), "{err}");
    }
}
