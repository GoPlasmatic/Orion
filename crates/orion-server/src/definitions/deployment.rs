//! A definition set as a revision-free v2 deployment directory — what
//! `orion-server compile --format deployment` writes.
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
    CompiledMember, CompiledRequirement, Definition, PluginDefinition, WorkflowDefinition,
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
                members.push(CompiledMember {
                    kind: "connection".to_string(),
                    id: id.clone(),
                    definition: None,
                    connection: Some(id),
                    requires: Vec::new(),
                });
                out.counts.connections += 1;
            }
            Err(e) => errors.push(e),
        }
    }

    for def in set.iter(Entity::Workflow) {
        let (id, workflow) = match workflow_definition(&def.doc, &def.origin) {
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
                    kind: "connection".to_string(),
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
                out.files.insert(path, bytes);
                members.push(member("workflow", id, digest, requires));
                out.counts.workflows += 1;
            }
            Err(e) => errors.push(format!("workflow '{id}' ({}): {e}", def.origin)),
        }
    }

    let workflow_ids: BTreeSet<&str> = members
        .iter()
        .filter(|m| m.kind == "workflow")
        .map(|m| m.id.as_str())
        .collect();
    let mut channel_members = Vec::new();
    for def in set.iter(Entity::Channel) {
        let (id, workflow_id, channel) = match channel_definition(&def.doc, &def.origin) {
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
        let requires = vec![CompiledRequirement {
            kind: "workflow".to_string(),
            id: workflow_id,
        }];
        match definition_file(&Definition::Channel(channel)) {
            Ok((digest, path, bytes)) => {
                out.files.insert(path, bytes);
                channel_members.push(member("channel", id, digest, requires));
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
                members.push(member("plugin", id, digest, Vec::new()));
                out.counts.plugins += 1;
            }
            Err(e) => errors.push(format!("plugin '{id}' ({}): {e}", plugin.origin)),
        }
    }

    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    for m in &members {
        if !seen.insert((m.kind.clone(), m.id.clone())) {
            errors.push(format!(
                "two {}s in the set share the id '{}'",
                m.kind, m.id
            ));
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    members.sort_by(|a, b| (&a.kind, &a.id).cmp(&(&b.kind, &b.id)));
    for m in &mut members {
        m.requires.sort();
        m.requires.dedup();
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
    kind: &str,
    id: String,
    digest: String,
    requires: Vec<CompiledRequirement>,
) -> CompiledMember {
    CompiledMember {
        kind: kind.to_string(),
        id,
        definition: Some(digest),
        connection: None,
        requires,
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
    Ok((id, workflow))
}

/// A set's channel as a [`ChannelDefinition`], with its id and the id of
/// the workflow it serves, which becomes its member's requirement.
pub fn channel_definition(
    doc: &Value,
    origin: &str,
) -> Result<(String, String, ChannelDefinition), String> {
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
        &[CHANNEL_IDENTITY, &["workflow_id", "activate"]].concat(),
        &what,
    )?;
    check_activate(obj, &what)?;
    if !present("workflow_id") {
        return Err(format!(
            "{what}: names no `workflow_id`, and a deployed channel serves one workflow"
        ));
    }
    let workflow_id = entity_id(obj, "workflow_id", "channel", origin)?;

    let definition: Map<String, Value> = obj
        .iter()
        .filter(|(k, v)| CHANNEL_KEYS.contains(&k.as_str()) && !v.is_null())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let channel: ChannelDefinition =
        serde_json::from_value(Value::Object(definition)).map_err(|e| format!("{what}: {e}"))?;
    Ok((id, workflow_id, channel))
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
pub fn used_connections(
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
/// `<id>.sig` or `<component file>.sig`, as `orion-server plugin sign`
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
        &["name", "connector_type", "config"],
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
    let driver = driver_for(connector_type, &config).map_err(|e| format!("{what}: {e}"))?;
    if connector_type == "db" {
        // It became the provider; the provider's settings have no such key.
        if let Some(settings) = config.as_object_mut() {
            settings.remove("driver");
        }
    }
    let provider_digest = foundation::builtin_provider_digest(driver)
        .ok_or_else(|| format!("{what}: '{driver}' is not a built-in driver"))?
        .into_string();

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
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
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
        let (id, wf, ch) = channel_definition(
            &json!({"channel_id": "c", "name": "c", "protocol": "rest", "workflow_id": "w",
                    "topic": null}),
            "ch.json",
        )
        .expect("converts");
        assert_eq!(
            (id.as_str(), wf.as_str(), ch.name.as_str()),
            ("c", "w", "c")
        );
        let err = channel_definition(
            &json!({"channel_id": "c", "name": "c", "protocol": "rest", "workflow_id": "w",
                    "topic": "orders"}),
            "ch.json",
        )
        .expect_err("refused");
        assert!(err.contains("`topic`"), "{err}");
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
