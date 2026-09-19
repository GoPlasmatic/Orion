//! The cross-reference pass over a [`DefinitionSet`].
//!
//! Lifted out of `package_cli::run_lint`, which was the only consumer and so
//! owned the walk. The checks are unchanged in substance — they are the ones
//! that had been guarding promotion artifacts — with four added that a
//! directory needs and an artifact happened not to have.
//!
//! Every check reports through [`Diagnostic`] rather than a formatted string, so
//! the caller decides what fails the command. That distinction is the reason
//! this is not a `Vec<String>`: a `channel_call` resolved by `channel_logic`
//! cannot be verified statically and must not fail a gate, while a connector
//! that exists nowhere must.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::connector::ConnectorType;
use crate::engine::FunctionRegistry;

use super::diagnostic::Diagnostic;
use super::set::Definition;
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
/// One diagnostic per structured field error a validator refused with.
///
/// The whole error is used only when it carries no field errors — a refusal
/// that is not per-field. Otherwise each `FieldError` becomes its own
/// diagnostic with its own path, because collapsing five field problems into
/// one prose line was the defect: a set gate could say a workflow failed schema
/// validation but not which field, and `--deny-warnings` was the only lever
/// over the lot.
fn schema_diagnostics(
    check: &'static str,
    entity: &str,
    def: &Definition,
    err: &crate::errors::OrionError,
) -> Vec<Diagnostic> {
    let fields = err.field_errors();
    if fields.is_empty() {
        return vec![
            Diagnostic::error(check, entity.to_string(), err.to_string()).with_location(
                &def.origin,
                None,
                None,
            ),
        ];
    }
    fields
        .iter()
        .map(|f| {
            let d = Diagnostic::from_field_error(check, entity, f);
            // The field path doubles as a document coordinate, so a schema
            // refusal can now say `orders.json:14:5` instead of naming the
            // workflow and leaving the author to find the field.
            let path = d.path.clone();
            let line = path.as_deref().and_then(|p| def.locate(p));
            let via = path.as_deref().and_then(|p| def.source_of(p).describe());
            d.with_location(&def.origin, path.as_deref(), line)
                .with_via(via)
        })
        .collect()
}

/// A literal `db_read` statement that is not a read — refused by the handler
/// at run time on every execution, so certain to fail the moment it runs.
/// Offline only: the admin API's validator is unchanged. A templated
/// statement is skipped, since what runs is only known then.
fn check_read_only_statements(def: &Definition, name: &str, findings: &mut Vec<Diagnostic>) {
    use crate::sql_lex::{READ_STATEMENTS, ReadOnlyViolation};
    let Some(tasks) = def.doc.get("tasks") else {
        return;
    };
    for (path, task) in crate::engine::walk_steps(tasks).tasks {
        let function = task.get("function");
        if function.and_then(|f| f.get("name")).and_then(Value::as_str) != Some("db_read") {
            continue;
        }
        let Some(query) = function
            .and_then(|f| f.get("input"))
            .and_then(|i| i.get("query"))
            .and_then(Value::as_str)
            .filter(|q| !q.contains("{{"))
        else {
            continue;
        };
        let message = match crate::sql_lex::read_only_violation(query) {
            None | Some(ReadOnlyViolation::Empty) => continue,
            Some(ReadOnlyViolation::NotARead { keyword }) => format!(
                "db_read runs read statements only, but this one starts with '{keyword}' — use \
                 db_write for INSERT/UPDATE/DELETE. Reads start with {}",
                READ_STATEMENTS.join(", ")
            ),
            Some(ReadOnlyViolation::ModifyingCte { keyword }) => format!(
                "db_read runs read statements only, but this one carries a data-modifying \
                 '{keyword}' common table expression — use db_write"
            ),
        };
        let at = format!("{path}.function.input.query");
        let line = def.locate(&at);
        let via = def.source_of(&at).describe();
        findings.push(
            Diagnostic::error("sql.read_only", format!("workflow '{name}'"), message)
                .with_location(&def.origin, Some(&at), line)
                .with_via(via),
        );
    }
}

pub fn check(
    set: &DefinitionSet,
    boundary: &Boundary,
    require_explicit_ids: bool,
    functions: &FunctionRegistry,
) -> Vec<Diagnostic> {
    let mut findings = Vec::new();

    let connectors = check_connectors(set, &mut findings);
    let workflows = check_workflows(set, require_explicit_ids, functions, &mut findings);
    let channels = check_channels(set, &workflows.ids, require_explicit_ids, &mut findings);

    check_closure(
        set,
        &workflows,
        &connectors,
        &channels,
        boundary,
        functions,
        &mut findings,
    );
    check_env_refs(set, &mut findings);
    check_cron_slots(set, &mut findings);

    // The plugins the set carries, as inventory: which manifest, how many
    // functions, and whether the component was there to hash — the line an
    // operator reads to know what a promotion of this set will need.
    for plugin in &set.plugins {
        let functions = plugin.manifest.functions.len();
        let component = match &plugin.digest {
            Some(digest) => format!("component {digest}"),
            None => "no component beside the manifest — its functions validate here but cannot \
                     run offline"
                .to_string(),
        };
        findings.push(Diagnostic::note(
            "plugin.manifest",
            format!("plugin '{}'", plugin.manifest.name),
            format!(
                "{} declares {functions} function(s); {component}",
                plugin.origin
            ),
        ));
    }

    // The models, likewise: the manifest as inventory, then what the file
    // beside it says — the numbers admission would record, so an author sees
    // `parameters` before submitting — or the fact that there is no file,
    // which a serving instance never needs and so is a note, not an error.
    // What *is* an error is a file that does not read as a graph, or one
    // whose tensors the manifest does not name: admission would refuse it
    // at `parse`, and that is a defect the author can fix here.
    for model in &set.models {
        let entity = format!("model '{}'", model.manifest.name);
        findings.push(Diagnostic::note(
            "model.manifest",
            &entity,
            format!(
                "{} declares {} input(s), {} output(s), format '{}'{}",
                model.origin,
                model.manifest.inputs.len(),
                model.manifest.outputs.len(),
                model.manifest.format,
                match &model.manifest.reference {
                    Some(reference) => {
                        format!(
                            "; deployable as connector '{}', key '{}'",
                            reference.connector, reference.key
                        )
                    }
                    None => String::new(),
                }
            ),
        ));
        match (&model.graph, &model.digest, model.artifact_bytes) {
            (Some(Ok(graph)), Some(digest), Some(bytes)) => {
                findings.push(Diagnostic::note(
                    "model.stats",
                    &entity,
                    format!(
                        "artifact {digest} ({bytes} bytes): {} parameters, {} nodes, IR {}, \
                         opset {}; graph inputs {}, outputs {}",
                        graph.parameters,
                        graph.nodes,
                        graph.ir_version,
                        graph.opset,
                        quoted(&graph.input_names),
                        quoted(&graph.output_names),
                    ),
                ));
                if let Err(reason) = crate::model::check_boundary(&model.manifest, graph) {
                    findings.push(
                        Diagnostic::error("model.graph", &entity, reason).with_remedy(
                            "name the graph's tensors in the manifest's inputs and outputs",
                        ),
                    );
                }
            }
            (Some(Err(reason)), _, _) => findings.push(Diagnostic::error(
                "model.graph",
                &entity,
                format!(
                    "the artifact beside {} does not read as a model: {reason}",
                    model.origin
                ),
            )),
            _ => findings.push(
                Diagnostic::note(
                    "model.artifact_missing",
                    &entity,
                    match &model.manifest.artifact {
                        Some(rel) => format!(
                            "no artifact beside the manifest ({} names '{rel}') — references to \
                             the model validate here, but it cannot run offline or compile into \
                             an artifact",
                            model.origin
                        ),
                        None => format!(
                            "{} names no artifact — references to the model validate here, but \
                             it cannot run offline or compile into an artifact",
                            model.origin
                        ),
                    },
                )
                .with_remedy("put the file beside the manifest and name it with `artifact`"),
            ),
        }
    }

    findings
}

/// `'a', 'b'` for a message.
fn quoted(names: &[String]) -> String {
    names
        .iter()
        .map(|n| format!("'{n}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Every `model_infer` task in `tasks`, through task groups: the ones naming
/// a model by literal id, and the ones whose `model` is computed. Each with
/// the JSON path of the `model` field, so a finding points at what the
/// author wrote.
fn model_references(tasks: &Value) -> (Vec<(String, String)>, Vec<String>) {
    let mut literal = Vec::new();
    let mut dynamic = Vec::new();
    for (path, task) in crate::engine::walk_steps(tasks).tasks {
        let Some(function) = task.get("function") else {
            continue;
        };
        if function.get("name").and_then(Value::as_str)
            != Some(crate::model::loader::INFER_FUNCTION)
        {
            continue;
        }
        let field = format!(
            "{path}.function.input.{}",
            crate::model::loader::INFER_MODEL_FIELD
        );
        match function
            .get("input")
            .and_then(|i| i.get(crate::model::loader::INFER_MODEL_FIELD))
        {
            Some(Value::String(id)) => literal.push((field, id.clone())),
            Some(_) => dynamic.push(field),
            // A missing `model` is the schema validator's `REQUIRED`.
            None => {}
        }
    }
    (literal, dynamic)
}

/// What the workflow pass learned.
struct Workflows {
    ids: Vec<String>,
    /// (id-or-origin, tasks) in set order.
    tasks: Vec<(String, Value)>,
}

/// name → what the connector is, for the closure checks.
fn check_connectors(
    set: &DefinitionSet,
    findings: &mut Vec<Diagnostic>,
) -> BTreeMap<String, crate::engine::ConnectorFacts> {
    let mut by_name = BTreeMap::new();
    let mut seen: Vec<String> = Vec::new();
    for def in set.iter(Entity::Connector) {
        let req: CreateConnectorRequest = match serde_json::from_value(def.doc.clone()) {
            Ok(req) => req,
            Err(e) => {
                findings.push(Diagnostic::error(
                    "parse.connector",
                    &def.origin,
                    format!("not a connector import item: {e}"),
                ));
                continue;
            }
        };
        if let Err(e) = crate::validation::validate_create_connector(&req) {
            findings.extend(schema_diagnostics(
                "schema.connector",
                &format!("connector '{}'", req.name),
                def,
                &e,
            ));
        }
        for embedded in crate::connector::secrets::embedded_references(&req.config) {
            findings.push(
                Diagnostic::warning(
                    "env.embedded_reference",
                    format!("connector '{}' config.{}", req.name, embedded.path),
                    embedded.message(),
                )
                .with_remedy(embedded.remedy()),
            );
        }
        if seen.contains(&req.name) {
            findings.push(Diagnostic::error(
                "duplicate.connector_name",
                format!("connector '{}'", req.name),
                "two connectors in the set share this name",
            ));
        }
        seen.push(req.name.clone());
        by_name.insert(
            req.name.clone(),
            crate::engine::ConnectorFacts {
                connector_type: req.connector_type,
                // Read from the definition's own connection string. A string
                // still holding a `${VAR}` or an `env://` reference reads as
                // not-Mongo, so the rules that turn on it stay silent rather
                // than firing on a value this pass cannot see — the same
                // stance the rest of the offline checks take.
                is_mongo: req
                    .config
                    .get("connection_string")
                    .and_then(|c| c.as_str())
                    .is_some_and(crate::connector::is_mongo_url),
            },
        );
    }
    by_name
}

fn check_workflows(
    set: &DefinitionSet,
    require_explicit_ids: bool,
    functions: &FunctionRegistry,
    findings: &mut Vec<Diagnostic>,
) -> Workflows {
    let loop_cap = crate::config::EngineConfig::default().max_loop_iterations;
    let mut ids = Vec::new();
    let mut tasks = Vec::new();
    for def in set.iter(Entity::Workflow) {
        let req: CreateWorkflowRequest = match serde_json::from_value(def.doc.clone()) {
            Ok(req) => req,
            Err(e) => {
                findings.push(Diagnostic::error(
                    "parse.workflow",
                    &def.origin,
                    format!("not a workflow import item: {e}"),
                ));
                continue;
            }
        };
        // Plugin functions the set cannot vouch for. A plugin function is
        // `<plugin>.<label>` under a reverse-domain plugin id, so a dotted
        // name the registry does not know is one of two things: a function of
        // a plugin whose manifest *is* in the set but does not declare it — an
        // error, the manifest is the authority — or a function of a plugin
        // the set carries no manifest for, which is neither valid nor invalid
        // here: it is unverifiable, reported as a note, and the admin API
        // validates it against the active plugin when the workflow arrives.
        let mut unverifiable: Vec<String> = Vec::new();
        let mut undeclared: Vec<(String, String, String)> = Vec::new();
        for task in crate::engine::leaf_tasks(&req.tasks) {
            let Some(name) = task
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            if functions.contains(name) || !name.contains('.') {
                continue;
            }
            match set.plugin_of(name) {
                Some(plugin) => undeclared.push((
                    name.to_string(),
                    plugin.manifest.name.clone(),
                    plugin.origin.clone(),
                )),
                None if !unverifiable.iter().any(|f| f == name) => {
                    unverifiable.push(name.to_string());
                }
                None => {}
            }
        }
        check_read_only_statements(def, &req.name, findings);
        if let Err(e) = crate::validation::validate_create_workflow(&req, loop_cap, functions) {
            let entity = format!("workflow '{}'", req.name);
            for d in schema_diagnostics("schema.workflow", &entity, def, &e) {
                let unknown =
                    |name: &str| d.message.starts_with(&format!("Unknown function '{name}'"));
                if let Some(name) = unverifiable.iter().find(|n| unknown(n)) {
                    findings.push(
                        Diagnostic::note(
                            "plugin.unverifiable",
                            &entity,
                            format!(
                                "names plugin function '{name}', and the set carries no manifest \
                                 for its plugin, so its input cannot be checked here; the admin \
                                 API validates it against the active plugin"
                            ),
                        )
                        .with_remedy(
                            "add the plugin's plugin.toml to the set, or pass --plugin-dir",
                        ),
                    );
                } else if let Some((name, plugin, origin)) =
                    undeclared.iter().find(|(n, _, _)| unknown(n))
                {
                    findings.push(Diagnostic::error(
                        "closure.plugin",
                        &entity,
                        format!(
                            "names '{name}', which the manifest for plugin '{plugin}' ({origin}) \
                             does not declare"
                        ),
                    ));
                } else {
                    findings.push(d);
                }
            }
        }
        // An error, not a warning: unlike an operator name, `env://` at the
        // head of a string has no reading in which it is data. Reported
        // separately from `schema.workflow` so a pipeline can see which of the
        // two refused the set. `validate_create_workflow` refuses the same
        // documents, so a set that passes here is one the admin API accepts.
        for (path, message) in crate::validation::secret_reference_errors(&req.tasks, functions) {
            findings.push(Diagnostic::error(
                "env.unresolved",
                format!("workflow '{}' {path}", req.name),
                message,
            ));
        }
        // The advisory the single-file lint already emits, carried into set
        // mode so a directory gate is not weaker than the per-file one.
        for (path, message) in crate::validation::unresolvable_logic_warnings(&req.tasks, functions)
        {
            findings.push(Diagnostic::warning(
                "logic.unresolvable",
                format!("workflow '{}' {path}", req.name),
                message,
            ));
        }
        // Likewise for what the engine reports and does not refuse: a
        // `$`-prefixed key that loses one `$` when it is emitted, a
        // `validation` whose failure changes nothing, and `continue_on_error`
        // on a group, which parses and is dropped.
        for advisory in crate::validation::engine_advisories(&req.tasks, functions) {
            findings.push(Diagnostic::warning(
                advisory.check,
                format!("workflow '{}' {}", req.name, advisory.path),
                advisory.message,
            ));
        }
        match &req.workflow_id {
            Some(id) => {
                if ids.contains(id) {
                    findings.push(Diagnostic::error(
                        "duplicate.workflow_id",
                        format!("workflow '{}'", req.name),
                        format!("two workflows in the set share workflow_id '{id}'"),
                    ));
                }
                ids.push(id.clone());
                tasks.push((id.clone(), req.tasks.clone()));
            }
            None if require_explicit_ids => findings.push(
                Diagnostic::error(
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
    findings: &mut Vec<Diagnostic>,
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
                findings.push(Diagnostic::error(
                    "parse.channel",
                    &def.origin,
                    format!("not a channel import item: {e}"),
                ));
                continue;
            }
        };
        if let Err(e) = crate::validation::validate_create_channel(&req) {
            findings.extend(schema_diagnostics(
                "schema.channel",
                &format!("channel '{}'", req.name),
                def,
                &e,
            ));
        }
        match &req.channel_id {
            Some(id) => {
                if channel_ids.contains(id) {
                    findings.push(Diagnostic::error(
                        "duplicate.channel_id",
                        format!("channel '{}'", req.name),
                        format!("two channels in the set share channel_id '{id}'"),
                    ));
                }
                channel_ids.push(id.clone());
            }
            None if require_explicit_ids => findings.push(Diagnostic::error(
                "missing.channel_id",
                format!("channel '{}'", req.name),
                "a package channel must carry an explicit channel_id",
            )),
            None => {}
        }
        // K7: channel names are unique across channel_ids.
        if names.contains(&req.name) {
            findings.push(Diagnostic::error(
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
        //
        // A channel carrying `config.oauth2_login` claims two routes — its own
        // pattern and the IdP callback — and the projection returns both, so a
        // callback that collides is caught here rather than at activation on
        // the target instance.
        for (route, route_methods) in crate::channel::routing::declared_route_parts(
            req.protocol.as_str(),
            req.route_pattern.as_deref(),
            req.methods.as_deref().unwrap_or_default(),
            req.config
                .get("oauth2_login")
                .and_then(|o| o.get("callback_path"))
                .and_then(|p| p.as_str()),
        ) {
            let clash = routes
                .iter()
                .find(|(other_route, other_methods, priority, _)| {
                    *other_route == route
                        && *priority == req.priority
                        && crate::channel::routing::methods_overlap(other_methods, &route_methods)
                });
            match clash {
                Some((_, _, _, first)) => findings.push(Diagnostic::error(
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
                    findings.push(Diagnostic::error(
                        "closure.workflow",
                        format!("channel '{}'", req.name),
                        format!("workflow '{wf}' is not in the set"),
                    ));
                }
            }
            _ => findings.push(Diagnostic::error(
                "missing.workflow_id_ref",
                format!("channel '{}'", req.name),
                "no workflow_id — the channel can never activate",
            )),
        }
    }
    names
}

/// Cron channels that share a `concurrency.key` but declare different
/// `slots`. Coherent — each run is admitted only to the slots below its own
/// channel's bound, so the key's fleet-wide bound is the largest declared and
/// a one-slot channel waits for slot 0 however many others are free — but so
/// rarely intended that the set should say so. A warning: the runtime has a
/// defined answer, and the admin API cannot see a channel's peers to refuse
/// it anyway.
fn check_cron_slots(set: &DefinitionSet, findings: &mut Vec<Diagnostic>) {
    // key -> (the first channel naming it, that channel's slots)
    let mut seen: Vec<(String, String, u64)> = Vec::new();
    for def in set.iter(Entity::Channel) {
        let doc = &def.doc;
        if doc.get("protocol").and_then(Value::as_str) != Some("cron") {
            continue;
        }
        let Some(concurrency) = doc
            .get("transport_config")
            .and_then(|t| t.get("concurrency"))
        else {
            continue;
        };
        if concurrency.get("policy").and_then(Value::as_str) != Some("forbid") {
            continue;
        }
        // The key defaults to the channel id; with neither, the id is minted
        // at create time and so can collide with nothing.
        let Some(key) = concurrency
            .get("key")
            .or_else(|| doc.get("channel_id"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        // A `slots` that is not an integer is the schema check's to report.
        let slots = match concurrency.get("slots") {
            None | Some(Value::Null) => 1,
            Some(value) => match value.as_u64() {
                Some(slots) => slots,
                None => continue,
            },
        };
        let name = doc
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&def.origin);
        match seen.iter().find(|(k, _, _)| k == key) {
            Some((_, first, first_slots)) if *first_slots != slots => {
                let path = "channel.transport_config.concurrency.slots";
                findings.push(
                    Diagnostic::warning(
                        "cron.slots_mismatch",
                        format!("channel '{name}'"),
                        format!(
                            "channels '{first}' and '{name}' share concurrency key '{key}' but \
                             declare slots {first_slots} and {slots} — each is admitted against \
                             its own bound"
                        ),
                    )
                    .with_location(&def.origin, Some(path), def.locate(path))
                    .with_remedy(format!(
                        "declare the same slots on every channel naming '{key}', or give them \
                         different keys"
                    )),
                );
            }
            Some(_) => {}
            None => seen.push((key.to_string(), name.to_string(), slots)),
        }
    }
}

/// Task references that must resolve in the set or be declared on the
/// boundary.
fn check_closure(
    set: &DefinitionSet,
    workflows: &Workflows,
    connectors: &BTreeMap<String, crate::engine::ConnectorFacts>,
    channels: &[String],
    boundary: &Boundary,
    functions: &FunctionRegistry,
    findings: &mut Vec<Diagnostic>,
) {
    for (workflow, tasks) in &workflows.tasks {
        let entity = format!("workflow '{workflow}'");

        // A model named by literal id must have a manifest in the set (or be
        // on the boundary), as a connector must: on a node, a workflow
        // naming a model the generation does not serve is quarantined, and
        // this is that gate offline. The finding is a field error first —
        // `MODEL_UNKNOWN` at the `model` field's path — so it carries the
        // coordinate a loaded document can turn into `file:line:col`.
        let (literal, dynamic) = model_references(tasks);
        for (path, model) in literal {
            if set.model_of(&model).is_some() || boundary.allows_model(&model) {
                continue;
            }
            let refusal = crate::errors::FieldError::new(
                path,
                "MODEL_UNKNOWN",
                format!(
                    "model '{model}' is neither in the set nor declared on the boundary: a \
                     model_infer task naming a model by literal id needs its manifest here \
                     (a model.json, or --model-dir), or the workflow is quarantined on a node \
                     that does not serve it"
                ),
            );
            findings.push(Diagnostic::from_field_error(
                "closure.model",
                &entity,
                &refusal,
            ));
        }
        for path in dynamic {
            findings.push(Diagnostic::note(
                "model.unverifiable",
                &entity,
                format!(
                    "{path} is computed, so the model it names is decided per message and \
                     cannot be checked here; a message naming a model the node does not \
                     serve fails that task as `unavailable`"
                ),
            ));
        }
        // The rules are `engine::check_connector_refs`, shared with the
        // activation gate the admin API runs (`admin::services::workflows`).
        // Only the lookup differs: there a live registry, here the set's own
        // connector definitions.
        for problem in crate::engine::check_connector_refs(tasks, functions, |name| {
            connectors.get(name).copied()
        }) {
            match problem {
                crate::engine::RefProblem::Missing { connector } => {
                    if !boundary.allows_connector(connector) {
                        findings.push(Diagnostic::error(
                            "closure.connector",
                            &entity,
                            format!(
                                "connector '{connector}' is neither in the set nor declared \
                                 on the boundary"
                            ),
                        ));
                    }
                }
                crate::engine::RefProblem::WrongType {
                    function,
                    connector,
                    actual,
                    wanted,
                } => {
                    let wanted: Vec<&str> = wanted.iter().map(ConnectorType::as_str).collect();
                    findings.push(Diagnostic::error(
                        "type.connector",
                        &entity,
                        format!(
                            "'{function}' needs a {} connector, but '{connector}' is type \
                             '{actual}'",
                            wanted.join(" or ")
                        ),
                    ));
                }
                crate::engine::RefProblem::MissingMongoDatabase {
                    function,
                    connector,
                } => findings.push(Diagnostic::error(
                    "type.mongo_database",
                    &entity,
                    format!(
                        "'{function}' points at MongoDB connector '{connector}' but sets no \
                         'database' — MongoDB connection strings carry no default database"
                    ),
                )),
            }
        }

        let (targets, dynamic) = crate::engine::channel_call_targets(tasks);
        for target in targets {
            if !channels.iter().any(|n| n == target) && !boundary.allows_channel(target) {
                findings.push(Diagnostic::error(
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
            findings.push(Diagnostic::warning(
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
fn check_env_refs(set: &DefinitionSet, findings: &mut Vec<Diagnostic>) {
    let mut refs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut secrets: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for def in &set.definitions {
        collect_env(&def.doc, def, &mut refs, &mut secrets);
    }
    for (needs, mut where_used) in refs {
        where_used.sort();
        where_used.dedup();
        findings.push(Diagnostic::note(
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
        findings.push(Diagnostic::note(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definitions::{Boundary, Entity, ModelDefinition};
    use crate::model::fixture;
    use serde_json::json;

    fn infer(id: &str, model: Value) -> Value {
        json!({"id": id, "name": id, "function": {"name": "model_infer",
            "input": {"model": model, "input": {"var": ""}}}})
    }

    fn workflow(id: &str, tasks: Value) -> (Entity, String, Value) {
        (
            Entity::Workflow,
            format!("{id}.json"),
            json!({"workflow_id": id, "name": id, "tasks": tasks}),
        )
    }

    fn checks<'a>(findings: &'a [Diagnostic], check: &str) -> Vec<&'a Diagnostic> {
        findings.iter().filter(|d| d.check == check).collect()
    }

    /// A literal reference resolves against the set's manifests or the
    /// boundary and is an error otherwise — at the `model` field's path,
    /// through a task group; a computed one is a note, never an error.
    #[test]
    fn model_references_resolve_against_manifests_or_the_boundary() {
        let mut set = DefinitionSet::from_entries([workflow(
            "score",
            json!([
                infer("known", json!("ada.c4-tiny")),
                {"id": "group", "tasks": [infer("required", json!("ada.required"))]},
                infer("missing", json!("ada.missing")),
                infer("computed", json!({"var": "data.model"})),
            ]),
        )]);
        set.models.push(ModelDefinition::from_manifest(
            "models/c4.json".to_string(),
            fixture::manifest(),
        ));
        let boundary = Boundary {
            models: vec!["ada.required".to_string()],
            ..Boundary::default()
        };
        let findings = check(&set, &boundary, false, FunctionRegistry::builtin());

        let closure = checks(&findings, "closure.model");
        assert_eq!(closure.len(), 1, "{findings:#?}");
        assert!(closure[0].is_error());
        assert_eq!(
            closure[0].path.as_deref(),
            Some("tasks[2].function.input.model")
        );
        assert!(closure[0].message.contains("'ada.missing'"));
        let unverifiable = checks(&findings, "model.unverifiable");
        assert_eq!(unverifiable.len(), 1);
        assert!(!unverifiable[0].is_error() && !unverifiable[0].is_warning());
        assert!(
            unverifiable[0]
                .message
                .contains("tasks[3].function.input.model is computed")
        );
        // The manifest without a file is inventoried and noted, not refused.
        assert_eq!(checks(&findings, "model.manifest").len(), 1);
        assert_eq!(checks(&findings, "model.artifact_missing").len(), 1);
        assert!(checks(&findings, "model.stats").is_empty());
        assert_eq!(
            findings.iter().filter(|d| d.is_error()).count(),
            1,
            "{findings:#?}"
        );
    }

    /// With the artifact on disk the stats admission would record are a
    /// note, and a manifest naming a tensor the graph lacks is the error
    /// admission would give at `parse`.
    #[test]
    fn a_manifest_with_its_artifact_reports_the_graphs_stats_and_boundary() {
        let dir = std::env::temp_dir().join(format!("orion-check-models-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("c4-tiny.onnx"), fixture::ONNX).expect("write");
        std::fs::write(dir.join("model.json"), fixture::MANIFEST).expect("write");
        std::fs::write(
            dir.join("wrong.json"),
            fixture::MANIFEST
                .replace("ada.c4-tiny", "ada.wrong")
                .replace("\"name\": \"policy\"", "\"name\": \"logits\""),
        )
        .expect("write");
        std::fs::write(dir.join("garbage.onnx"), b"\x00\x01not a graph").expect("write");
        std::fs::write(
            dir.join("garbage.json"),
            fixture::MANIFEST
                .replace("ada.c4-tiny", "ada.garbage")
                .replace("c4-tiny.onnx", "garbage.onnx"),
        )
        .expect("write");

        let (set, _) = DefinitionSet::from_directory(&dir).expect("loads");
        assert_eq!(set.models.len(), 3);
        let findings = check(
            &set,
            &Boundary::default(),
            false,
            FunctionRegistry::builtin(),
        );
        let _ = std::fs::remove_dir_all(&dir);

        let stats = checks(&findings, "model.stats");
        assert_eq!(stats.len(), 2, "{findings:#?}");
        let c4 = stats
            .iter()
            .find(|d| d.entity == "model 'ada.c4-tiny'")
            .expect("the fixture's stats");
        assert!(c4.message.contains("1479 parameters"), "{}", c4.message);
        assert!(c4.message.contains("6171 bytes"), "{}", c4.message);
        assert!(c4.message.contains("opset 17"), "{}", c4.message);
        let graph = checks(&findings, "model.graph");
        assert_eq!(graph.len(), 2, "{findings:#?}");
        let wrong = graph
            .iter()
            .find(|d| d.entity == "model 'ada.wrong'")
            .expect("the boundary mismatch");
        assert!(wrong.is_error());
        assert!(
            wrong.message.contains("output 'logits'"),
            "{}",
            wrong.message
        );
        let garbage = graph
            .iter()
            .find(|d| d.entity == "model 'ada.garbage'")
            .expect("the unreadable file");
        assert!(garbage.message.contains("does not read as a model"));
        assert!(checks(&findings, "model.artifact_missing").is_empty());
    }

    /// The defect this module's `schema_diagnostics` exists to close.
    ///
    /// A workflow with several schema problems used to produce exactly one
    /// finding, whose message was the whole `OrionError` flattened with
    /// `to_string()` — so a set gate could say the workflow failed validation
    /// but not which field, and a pipeline had nothing to grandfather but the
    /// entire `schema.workflow` family. One diagnostic per field error, each
    /// carrying its own path, is the fix.
    #[test]
    fn a_schema_refusal_reports_one_diagnostic_per_field_with_its_path() {
        // Two independent problems: no name, and a task naming no function.
        let doc = serde_json::json!({
            "name": "",
            "tasks": [{"id": "t1"}],
        });
        let set = DefinitionSet::from_entries([(Entity::Workflow, "wf.json".to_string(), doc)]);

        let schema: Vec<_> = check(
            &set,
            &Boundary::default(),
            false,
            crate::engine::FunctionRegistry::builtin(),
        )
        .into_iter()
        .filter(|d| d.check == "schema.workflow")
        .collect();

        assert!(
            !schema.is_empty(),
            "an invalid workflow must be refused by the set check"
        );
        assert!(
            schema.iter().all(|d| d.path.is_some()),
            "every schema diagnostic must name the field it is about: {schema:#?}"
        );
    }

    /// The parse-once payoff: a set loaded from disk carries each document's
    /// spans, so a finding can say `file:line:col`.
    ///
    /// Before this, `definitions/json.rs` existed to produce exactly this and
    /// the set threw its output away — `lint`, `check` and `compile` findings
    /// had no origin at all, and clippy got one only by re-reading and
    /// re-parsing every file a third time.
    #[test]
    fn a_finding_from_a_loaded_directory_carries_its_file_and_line() {
        // `std::env::temp_dir` rather than a `tempfile` dependency — the same
        // thing `config::tests` and the backup tests do.
        let dir = std::env::temp_dir().join(format!(
            "orion-check-spans-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("orders.json");
        // Invalid: a task naming no function. The `name` is on line 2, the
        // offending task on line 4.
        std::fs::write(
            &path,
            "{\n  \"name\": \"\",\n  \"tasks\": [\n    { \"id\": \"t1\" }\n  ]\n}\n",
        )
        .expect("write");

        let (set, _report) = DefinitionSet::from_directory(&dir).expect("load");
        let located: Vec<_> = check(
            &set,
            &Boundary::default(),
            false,
            crate::engine::FunctionRegistry::builtin(),
        )
        .into_iter()
        .filter(|d| d.check == "schema.workflow")
        .collect();

        assert!(!located.is_empty(), "the workflow must be refused");
        for d in &located {
            assert_eq!(
                d.file.as_deref(),
                Some(path.display().to_string().as_str()),
                "a finding must name the file it came from"
            );
        }
        let any_line = located.iter().any(|d| d.line.is_some());
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            any_line,
            "at least one finding must resolve to a line:col — that is what \
             carrying the spans is for: {located:#?}"
        );
    }

    fn cron(name: &str, concurrency: Value) -> (Entity, String, Value) {
        (
            Entity::Channel,
            format!("{name}.json"),
            json!({"channel_id": name, "name": name, "protocol": "cron", "workflow_id": "wf",
                "transport_config": {"schedule": "0 * * * * *", "concurrency": concurrency}}),
        )
    }

    /// Two channels on one key with different `slots` warn; agreeing ones,
    /// different keys and `allow` do not. The default key is the channel id.
    #[test]
    fn a_shared_key_with_different_slots_is_a_warning() {
        let set = DefinitionSet::from_entries([
            workflow("wf", json!([])),
            cron(
                "a",
                json!({"policy": "forbid", "key": "worker", "slots": 4}),
            ),
            cron("b", json!({"policy": "forbid", "key": "worker"})),
            cron(
                "c",
                json!({"policy": "forbid", "key": "worker", "slots": 4}),
            ),
            cron("d", json!({"policy": "forbid", "key": "other", "slots": 2})),
            cron("e", json!({"policy": "allow", "key": "worker"})),
            cron("f", json!({"policy": "forbid", "key": "g", "slots": 2})),
            cron("g", json!({"policy": "forbid"})),
        ]);
        let findings = check(
            &set,
            &Boundary::default(),
            false,
            FunctionRegistry::builtin(),
        );
        let mismatches = checks(&findings, "cron.slots_mismatch");
        let messages: Vec<&str> = mismatches.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(messages[0].contains("'a' and 'b' share concurrency key 'worker'"));
        assert!(messages[0].contains("slots 4 and 1"));
        assert!(messages[1].contains("'f' and 'g' share concurrency key 'g'"));
        assert!(
            mismatches
                .iter()
                .all(|d| d.severity == crate::definitions::Severity::Warning)
        );
    }
}
