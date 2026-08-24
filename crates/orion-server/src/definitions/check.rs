//! The cross-reference pass over a [`DefinitionSet`].
//!
//! Lifted out of `package_cli::run_lint`, which was the only consumer and so
//! owned the walk. The checks are unchanged in substance — they are the ones
//! that had been guarding promotion artifacts — with four added that a
//! directory needs and an artifact happened not to have.
//!
//! Every check reports through [`Finding`] rather than a formatted string, so
//! the caller decides what fails the command. That distinction is the reason
//! this is not a `Vec<String>`: a `channel_call` resolved by `channel_logic`
//! cannot be verified statically and must not fail a gate, while a connector
//! that exists nowhere must.

use std::collections::BTreeMap;

use serde_json::Value;

use super::finding::Finding;
use super::set::{Boundary, DefinitionSet, Entity};
use crate::storage::repositories::channels::CreateChannelRequest;
use crate::storage::repositories::connectors::CreateConnectorRequest;
use crate::storage::repositories::workflows::CreateWorkflowRequest;

/// Run every check over `set`, treating names in `boundary` as satisfied
/// outside it.
///
/// `require_explicit_ids` is the one behavioural difference between the two
/// containers. A promotion artifact must carry explicit `workflow_id` and
/// `channel_id` — a generated id could not be referenced by a channel in the
/// same package, nor matched against the target on re-apply. A directory being
/// linted has no such contract: it may hold a workflow the author has not
/// assigned an id to yet, and refusing that would make the gate unusable
/// during authoring, which is when it is most wanted.
pub fn check(set: &DefinitionSet, boundary: &Boundary, require_explicit_ids: bool) -> Vec<Finding> {
    let mut findings = Vec::new();

    let connectors = check_connectors(set, &mut findings);
    let workflows = check_workflows(set, require_explicit_ids, &mut findings);
    let channels = check_channels(set, &workflows.ids, require_explicit_ids, &mut findings);

    check_closure(&workflows, &connectors, &channels, boundary, &mut findings);
    check_env_refs(set, &mut findings);

    findings
}

/// What the connector pass learned, for the closure checks.
struct Connectors {
    /// name → declared `connector_type`.
    by_name: BTreeMap<String, String>,
}

/// What the workflow pass learned.
struct Workflows {
    ids: Vec<String>,
    /// (id-or-origin, tasks) in set order.
    tasks: Vec<(String, Value)>,
}

/// What the channel pass learned.
struct Channels {
    names: Vec<String>,
}

fn check_connectors(set: &DefinitionSet, findings: &mut Vec<Finding>) -> Connectors {
    let mut by_name = BTreeMap::new();
    let mut seen: Vec<String> = Vec::new();
    for def in set.iter(Entity::Connector) {
        let req: CreateConnectorRequest = match serde_json::from_value(def.doc.clone()) {
            Ok(req) => req,
            Err(e) => {
                findings.push(Finding::error(
                    "parse.connector",
                    &def.origin,
                    format!("not a connector import item: {e}"),
                ));
                continue;
            }
        };
        if let Err(e) = crate::validation::validate_create_connector(&req) {
            findings.push(Finding::error(
                "schema.connector",
                format!("connector '{}'", req.name),
                e.to_string(),
            ));
        }
        if seen.contains(&req.name) {
            findings.push(Finding::error(
                "duplicate.connector_name",
                format!("connector '{}'", req.name),
                "two connectors in the set share this name",
            ));
        }
        seen.push(req.name.clone());
        by_name.insert(
            req.name.clone(),
            serde_json::to_value(req.connector_type)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default(),
        );
    }
    Connectors { by_name }
}

fn check_workflows(
    set: &DefinitionSet,
    require_explicit_ids: bool,
    findings: &mut Vec<Finding>,
) -> Workflows {
    let loop_cap = crate::config::EngineConfig::default().max_loop_iterations;
    let mut ids = Vec::new();
    let mut tasks = Vec::new();
    for def in set.iter(Entity::Workflow) {
        let req: CreateWorkflowRequest = match serde_json::from_value(def.doc.clone()) {
            Ok(req) => req,
            Err(e) => {
                findings.push(Finding::error(
                    "parse.workflow",
                    &def.origin,
                    format!("not a workflow import item: {e}"),
                ));
                continue;
            }
        };
        if let Err(e) = crate::validation::validate_create_workflow(&req, loop_cap) {
            findings.push(Finding::error(
                "schema.workflow",
                format!("workflow '{}'", req.name),
                e.to_string(),
            ));
        }
        // The advisory the single-file lint already emits, carried into set
        // mode so a directory gate is not weaker than the per-file one.
        for (path, message) in crate::validation::unresolvable_logic_warnings(&req.tasks) {
            findings.push(Finding::warning(
                "logic.unresolvable",
                format!("workflow '{}' {path}", req.name),
                message,
            ));
        }
        match &req.workflow_id {
            Some(id) => {
                if ids.contains(id) {
                    findings.push(Finding::error(
                        "duplicate.workflow_id",
                        format!("workflow '{}'", req.name),
                        format!("two workflows in the set share workflow_id '{id}'"),
                    ));
                }
                ids.push(id.clone());
                tasks.push((id.clone(), req.tasks.clone()));
            }
            None if require_explicit_ids => findings.push(
                Finding::error(
                    "missing.workflow_id",
                    format!("workflow '{}'", req.name),
                    "a package workflow must carry an explicit workflow_id — a generated \
                     id cannot be referenced by channels in the same package",
                )
                .with_remedy("add a workflow_id to the workflow definition"),
            ),
            // Authoring-time directory lint: an id-less workflow is still
            // worth checking, it just cannot be a `channel.workflow_id` target.
            None => tasks.push((def.origin.clone(), req.tasks.clone())),
        }
    }
    Workflows { ids, tasks }
}

fn check_channels(
    set: &DefinitionSet,
    workflow_ids: &[String],
    require_explicit_ids: bool,
    findings: &mut Vec<Finding>,
) -> Channels {
    let mut names: Vec<String> = Vec::new();
    let mut channel_ids: Vec<String> = Vec::new();
    // (method, pattern) → the channel that claimed it first.
    let mut routes: BTreeMap<(String, String), String> = BTreeMap::new();

    for def in set.iter(Entity::Channel) {
        let req: CreateChannelRequest = match serde_json::from_value(def.doc.clone()) {
            Ok(req) => req,
            Err(e) => {
                findings.push(Finding::error(
                    "parse.channel",
                    &def.origin,
                    format!("not a channel import item: {e}"),
                ));
                continue;
            }
        };
        if let Err(e) = crate::validation::validate_create_channel(&req) {
            findings.push(Finding::error(
                "schema.channel",
                format!("channel '{}'", req.name),
                e.to_string(),
            ));
        }
        match &req.channel_id {
            Some(id) => {
                if channel_ids.contains(id) {
                    findings.push(Finding::error(
                        "duplicate.channel_id",
                        format!("channel '{}'", req.name),
                        format!("two channels in the set share channel_id '{id}'"),
                    ));
                }
                channel_ids.push(id.clone());
            }
            None if require_explicit_ids => findings.push(Finding::error(
                "missing.channel_id",
                format!("channel '{}'", req.name),
                "a package channel must carry an explicit channel_id",
            )),
            None => {}
        }
        // K7: channel names are unique across channel_ids.
        if names.contains(&req.name) {
            findings.push(Finding::error(
                "duplicate.channel_name",
                format!("channel '{}'", req.name),
                "two channels in the set share this name — channel names are unique (K7)",
            ));
        }
        names.push(req.name.clone());

        // A route claimed twice is served by whichever channel the registry
        // happens to load second, which is not a property an author chose.
        if let Some(pattern) = route_pattern(&def.doc) {
            for method in methods(&def.doc) {
                let key = (method.clone(), pattern.clone());
                match routes.get(&key) {
                    Some(first) => findings.push(Finding::error(
                        "duplicate.route_pattern",
                        format!("channel '{}'", req.name),
                        format!("{method} {pattern} is already served by channel '{first}'"),
                    )),
                    None => {
                        routes.insert(key, req.name.clone());
                    }
                }
            }
        }

        match &req.workflow_id {
            Some(wf) if !wf.is_empty() => {
                if !workflow_ids.iter().any(|id| id == wf) {
                    findings.push(Finding::error(
                        "closure.workflow",
                        format!("channel '{}'", req.name),
                        format!("workflow '{wf}' is not in the set"),
                    ));
                }
            }
            _ => findings.push(Finding::error(
                "missing.workflow_id_ref",
                format!("channel '{}'", req.name),
                "no workflow_id — the channel can never activate",
            )),
        }
    }
    Channels { names }
}

/// Task references that must resolve in the set or be declared on the
/// boundary.
fn check_closure(
    workflows: &Workflows,
    connectors: &Connectors,
    channels: &Channels,
    boundary: &Boundary,
    findings: &mut Vec<Finding>,
) {
    for (workflow, tasks) in &workflows.tasks {
        for r in crate::engine::connector_refs(tasks) {
            let entity = format!("workflow '{workflow}'");
            match connectors.by_name.get(r.connector) {
                Some(declared) => {
                    // The type gate the artifact lint never applied: a
                    // `http_call` pointed at a `db` connector parses, imports,
                    // activates, and fails at the first request.
                    if let Some(allowed) = crate::engine::required_connector_types(r.function) {
                        let allowed: Vec<String> = allowed
                            .iter()
                            .filter_map(|t| serde_json::to_value(t).ok())
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect();
                        if !declared.is_empty() && !allowed.iter().any(|a| a == declared) {
                            findings.push(Finding::error(
                                "type.connector",
                                &entity,
                                format!(
                                    "'{}' needs a {} connector, but '{}' is type '{declared}'",
                                    r.function,
                                    allowed.join(" or "),
                                    r.connector
                                ),
                            ));
                        }
                    }
                }
                None if boundary.allows_connector(r.connector) => {}
                None => findings.push(Finding::error(
                    "closure.connector",
                    &entity,
                    format!(
                        "connector '{}' is neither in the set nor declared on the boundary",
                        r.connector
                    ),
                )),
            }
        }

        let (targets, dynamic) = crate::engine::channel_call_targets(tasks);
        for target in targets {
            if !channels.names.iter().any(|n| n == target) && !boundary.allows_channel(target) {
                findings.push(Finding::error(
                    "closure.channel_call",
                    format!("workflow '{workflow}'"),
                    format!(
                        "channel_call target '{target}' is neither in the set nor declared \
                         on the boundary"
                    ),
                ));
            }
        }
        if dynamic {
            findings.push(Finding::warning(
                "closure.channel_call_dynamic",
                format!("workflow '{workflow}'"),
                "resolves channel_call targets dynamically — closure checking cannot cover \
                 those calls",
            ));
        }
    }
}

/// Every `env://` reference in the set, reported once, so an operator can see
/// what the set needs in its environment before deploying it.
///
/// A warning, not an error: this process is not the one that will serve the
/// set, so its environment says nothing about whether the variable will be
/// present where it matters.
fn check_env_refs(set: &DefinitionSet, findings: &mut Vec<Finding>) {
    let mut refs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for def in &set.definitions {
        collect_env(&def.doc, def, &mut refs);
    }
    for (var, mut where_used) in refs {
        where_used.sort();
        where_used.dedup();
        findings.push(Finding::warning(
            "env.reference",
            where_used.join(", "),
            format!("requires environment variable '{var}'"),
        ));
    }
}

fn collect_env(
    value: &Value,
    def: &super::set::Definition,
    out: &mut BTreeMap<String, Vec<String>>,
) {
    match value {
        Value::String(s) => {
            if let Some(var) = s.strip_prefix("env://") {
                out.entry(var.to_string()).or_default().push(format!(
                    "{} '{}'",
                    def.entity.as_str(),
                    def.origin
                ));
            }
        }
        Value::Array(items) => items.iter().for_each(|v| collect_env(v, def, out)),
        Value::Object(map) => map.values().for_each(|v| collect_env(v, def, out)),
        _ => {}
    }
}

fn route_pattern(doc: &Value) -> Option<String> {
    doc.get("route_pattern")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}

/// The methods a channel serves, defaulting to a single unnamed slot so a
/// channel with a pattern but no explicit methods still collides with another
/// on the same pattern.
fn methods(doc: &Value) -> Vec<String> {
    match doc.get("methods").and_then(Value::as_array) {
        Some(list) if !list.is_empty() => list
            .iter()
            .filter_map(Value::as_str)
            .map(|m| m.to_uppercase())
            .collect(),
        _ => vec!["ANY".to_string()],
    }
}
