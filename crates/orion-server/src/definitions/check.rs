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

use crate::connector::ConnectorType;

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

/// What the workflow pass learned.
struct Workflows {
    ids: Vec<String>,
    /// (id-or-origin, tasks) in set order.
    tasks: Vec<(String, Value)>,
}

/// name → declared `connector_type`, for the closure checks.
fn check_connectors(
    set: &DefinitionSet,
    findings: &mut Vec<Finding>,
) -> BTreeMap<String, &'static str> {
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
        by_name.insert(req.name.clone(), req.connector_type.as_str());
    }
    by_name
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
        // An error, not a warning: unlike an operator name, `env://` at the
        // head of a string has no reading in which it is data. Reported
        // separately from `schema.workflow` so a pipeline can see which of the
        // two refused the set. `validate_create_workflow` refuses the same
        // documents, so a set that passes here is one the admin API accepts.
        for (path, message) in crate::validation::secret_reference_errors(&req.tasks) {
            findings.push(Finding::error(
                "env.unresolved",
                format!("workflow '{}' {path}", req.name),
                message,
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
) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut channel_ids: Vec<String> = Vec::new();
    // (canonical route, methods, priority, the channel that claimed it first)
    // — a list rather than a map because "same route" is now an overlap test,
    // not a key lookup.
    let mut routes: Vec<(String, Vec<String>, i64, String)> = Vec::new();

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
        //
        // Projected and compared exactly as activation does
        // (`ensure_route_is_unclaimed`): the canonical shape, so `/o/{id}` and
        // `/o/{orderId}` are the one route they will be at runtime; method
        // *overlap*, so an unrestricted channel collides with every method
        // rather than with nothing; and only at equal priority, because a
        // deliberate higher-priority override is how a route is meant to be
        // taken over and must not fail the gate.
        if let Some((route, route_methods)) = crate::channel::routing::declared_route_parts(
            req.protocol.as_str(),
            req.route_pattern.as_deref(),
            req.methods.as_deref().unwrap_or_default(),
        ) {
            let clash = routes
                .iter()
                .find(|(other_route, other_methods, priority, _)| {
                    *other_route == route
                        && *priority == req.priority
                        && crate::channel::routing::methods_overlap(other_methods, &route_methods)
                });
            match clash {
                Some((_, _, _, first)) => findings.push(Finding::error(
                    "duplicate.route_pattern",
                    format!("channel '{}'", req.name),
                    format!(
                        "{} {route} at priority {} is already served by channel '{first}'",
                        if route_methods.is_empty() {
                            "every method on".to_string()
                        } else {
                            route_methods.join("/")
                        },
                        req.priority,
                    ),
                )),
                None => routes.push((route, route_methods, req.priority, req.name.clone())),
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
    names
}

/// Task references that must resolve in the set or be declared on the
/// boundary.
fn check_closure(
    workflows: &Workflows,
    connectors: &BTreeMap<String, &'static str>,
    channels: &[String],
    boundary: &Boundary,
    findings: &mut Vec<Finding>,
) {
    for (workflow, tasks) in &workflows.tasks {
        let entity = format!("workflow '{workflow}'");
        for r in crate::engine::connector_refs(tasks) {
            match connectors.get(r.connector) {
                Some(declared) => {
                    // The type gate the artifact lint never applied: a
                    // `http_call` pointed at a `db` connector parses, imports,
                    // activates, and fails at the first request.
                    if let Some(allowed) = crate::engine::required_connector_types(r.function)
                        && !allowed.iter().any(|t| t.as_str() == *declared)
                    {
                        let allowed: Vec<&str> =
                            allowed.iter().map(ConnectorType::as_str).collect();
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
            if !channels.iter().any(|n| n == target) && !boundary.allows_channel(target) {
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

/// Every secret reference in the set, reported once, so an operator can see
/// what the set needs deployed alongside it — `env://` variables and the
/// other schemes this build resolves alike.
///
/// A note, not an error: this process is not the one that will serve the set,
/// so its environment says nothing about whether the variable will be present
/// where it matters. Not a warning either — there is nothing here to fix.
/// `env://` is the documented way to author a secret, so counting these as
/// warnings made `--deny-warnings` fail on every set that uses one.
fn check_env_refs(set: &DefinitionSet, findings: &mut Vec<Finding>) {
    let mut refs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut secrets: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for def in &set.definitions {
        collect_env(&def.doc, def, &mut refs, &mut secrets);
    }
    for (needs, mut where_used) in refs {
        where_used.sort();
        where_used.dedup();
        findings.push(Finding::note(
            "env.reference",
            where_used.join(", "),
            format!("requires {needs}"),
        ));
    }
    // The same inventory for the other half of the deployment checklist. A
    // separate id because the answer is a different action: an `env://`
    // reference needs a variable in the environment, a `{"secret": …}` needs an
    // entry in the serving instance's `[secrets]` section — and an instance
    // that lacks one quarantines the channel rather than failing a task.
    for (name, mut where_used) in secrets {
        where_used.sort();
        where_used.dedup();
        findings.push(Finding::note(
            "secrets.reference",
            where_used.join(", "),
            format!("requires a [secrets] entry named '{name}'"),
        ));
    }
}

fn collect_env(
    value: &Value,
    def: &super::set::Definition,
    out: &mut BTreeMap<String, Vec<String>>,
    secrets: &mut BTreeMap<String, Vec<String>>,
) {
    match value {
        Value::String(s) => {
            // The masking policy's predicate, not a `strip_prefix("env://")`:
            // it is the one place that decides which schemes this build can
            // resolve, so a `vault://` reference is inventoried too rather
            // than leaving a set that uses one with a clean report and a
            // missing secret at deploy. Strict by design — it does not mistake
            // `postgres://user:pw@host` for a reference.
            //
            // Deliberately not `secrets::collect_references`, which filters by
            // the *live* resolver registry: whether `vault://` resolves
            // depends on this process's `VAULT_ADDR`, and a lint whose report
            // changes with the laptop it runs on is not a gate.
            if crate::connector::secrets::is_resolvable_reference(s)
                && let Some((scheme, reference)) = crate::connector::secrets::parse_reference(s)
            {
                let needs = match scheme {
                    "env" => format!("environment variable '{reference}'"),
                    _ => format!("secret '{s}'"),
                };
                out.entry(needs).or_default().push(format!(
                    "{} '{}'",
                    def.entity.as_str(),
                    def.origin
                ));
            }
        }
        Value::Array(items) => items.iter().for_each(|v| collect_env(v, def, out, secrets)),
        // A `{"secret": "name"}` node names a declaration the serving instance
        // must carry, so it is inventoried and *not* descended into: the
        // argument is a name, never a reference.
        Value::Object(map) => {
            if let Some(name) = crate::engine::functions::secret_ref::secret_name(value) {
                secrets.entry(name.to_string()).or_default().push(format!(
                    "{} '{}'",
                    def.entity.as_str(),
                    def.origin
                ));
                return;
            }
            map.values().for_each(|v| collect_env(v, def, out, secrets));
        }
        _ => {}
    }
}
