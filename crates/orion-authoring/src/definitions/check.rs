//! The cross-reference pass over a [`DefinitionSet`].
//!
//! Every check reports through [`Diagnostic`] rather than a formatted string, so
//! the caller decides what fails the command. That distinction is the reason
//! this is not a `Vec<String>`: a `channel_call` resolved by `channel_logic`
//! cannot be verified statically and must not fail a gate, while a connector
//! that exists nowhere must.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::connector::ConnectorType;
use crate::engine::FunctionRegistry;

use super::diagnostic::Diagnostic;
use super::set::Definition;
use super::set::{Boundary, DefinitionSet, Entity};
use crate::definitions::documents::CreateChannelRequest;
use crate::definitions::documents::CreateConnectorRequest;
use crate::definitions::documents::CreateWorkflowRequest;

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
/// diagnostic with its own path: collapsed into one prose line, five field
/// problems would let a set gate say a workflow failed schema validation but
/// not which field, leaving `--deny-warnings` the only lever over the lot.
/// The value at a validator field path, rooted at the entity document.
///
/// Segments are dotted with array indices inline — `tasks[2].function.name`,
/// `loop.setup[0].condition` — which is the shape `FieldError::path` uses.
fn value_at_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = root;
    for segment in path.split('.') {
        let (key, mut rest) = match segment.find('[') {
            Some(at) => (&segment[..at], &segment[at..]),
            None => (segment, ""),
        };
        if !key.is_empty() {
            current = current.get(key)?;
        }
        while let Some(after) = rest.strip_prefix('[') {
            let end = after.find(']')?;
            let index: usize = after[..end].parse().ok()?;
            current = current.get(index)?;
            rest = &after[end + 1..];
        }
    }
    Some(current)
}

/// The function a `UNKNOWN_FUNCTION` diagnostic names, read from the document
/// at the refused path rather than from the validator's message text (B-08).
fn unknown_function_name<'a>(def: &'a Definition, d: &Diagnostic) -> Option<&'a str> {
    if d.code.as_deref() != Some(orion_api::error::field_codes::UNKNOWN_FUNCTION) {
        return None;
    }
    value_at_path(&def.doc, d.path.as_deref()?)?.as_str()
}

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
    use orion_definitions::sql_lex::{READ_STATEMENTS, ReadOnlyViolation};
    for statement in super::statements::statements_of(def) {
        // A templated statement is skipped: what runs is only known then.
        if statement.function != "db_read" || statement.query.contains("{{") {
            continue;
        }
        let message = match orion_definitions::sql_lex::read_only_violation(statement.query) {
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
        let at = statement.path;
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
    let known_connector = |name: &str| {
        connectors.facts.contains_key(name)
            || connectors.provider_backed.contains(name)
            || boundary.allows_connector(name)
    };
    let drivers = boundary
        .channel_drivers
        .clone()
        .unwrap_or_else(crate::extensions::ChannelDriverRegistry::community);
    let channels = check_channels(
        set,
        &workflows,
        &drivers,
        &known_connector,
        require_explicit_ids,
        &mut findings,
    );

    check_closure(
        &workflows,
        &connectors.facts,
        &channels,
        boundary,
        functions,
        &mut findings,
    );
    check_operations(&workflows, boundary, &mut findings);
    check_bindings(set, boundary, &connectors, &mut findings);
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

    findings
}

/// Every `db` connector of a definition set names its `driver`.
///
/// A directory's rule, not an artifact's: [`super::gate_directory`] runs it,
/// and `package lint` does not, because an artifact exported from an
/// instance carries its connectors as they were stored and the admin API
/// never required the field. A set is what `compile --format deployment`
/// reads, and a connection's provider is its driver: a connection string
/// that is a secret reference or a `${VAR}` placeholder does not show its
/// scheme until the serving node resolves it, so only the field can say.
/// Requiring it of every `db` connector, reference or not, keeps one rule
/// instead of one that depends on how the URL happens to be spelled. Whether
/// a stated driver agrees with a visible scheme is the admin validator's
/// check, which the set pass already runs (`schema.connector`).
pub(super) fn check_db_drivers(set: &DefinitionSet, findings: &mut Vec<Diagnostic>) {
    for def in set.iter(Entity::Connector) {
        if def.doc.get("connector_type").and_then(Value::as_str) != Some("db") {
            continue;
        }
        if def
            .doc
            .get("config")
            .is_some_and(|config| config.get("driver").is_some())
        {
            continue;
        }
        let name = def
            .doc
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&def.origin);
        let line = def.locate("connector.config");
        findings.push(
            Diagnostic::error(
                "connector.db_driver",
                format!("connector '{name}'"),
                format!(
                    "a db connector in a definition set names its `driver` ({}): the \
                     connection string's scheme is not always visible before it is resolved, \
                     and a deployment's connection needs the driver to name its provider",
                    crate::connector::DbDriver::NAMES.join(", ")
                ),
            )
            .with_location(&def.origin, Some("connector.config"), line)
            .with_remedy("add `\"driver\": \"postgresql\"` (or mysql, sqlite) to `config`"),
        );
    }
}

/// What the workflow pass learned.
struct Workflows {
    ids: Vec<String>,
    /// workflow id → the explicit authoring `version`s the set carries. One
    /// entry means no split is possible; more than one is a canary candidate.
    versions: std::collections::BTreeMap<String, BTreeSet<u64>>,
    /// (id-or-origin, tasks, loop) in set order.
    tasks: Vec<(String, Value, Option<Value>)>,
}

/// What the connector pass learned. `facts` is what a typed function checks
/// against; `compiled` is what `compile` would write, which is where a
/// binding check finds a connector's driver.
struct Connectors {
    facts: BTreeMap<String, crate::engine::ConnectorFacts>,
    provider_backed: BTreeSet<String>,
    names: BTreeSet<String>,
    compiled: BTreeMap<String, orion_api::v2::CompiledConnection>,
}

/// name → what the connector is, for the closure checks.
fn check_connectors(set: &DefinitionSet, findings: &mut Vec<Diagnostic>) -> Connectors {
    let mut connectors = Connectors {
        facts: BTreeMap::new(),
        provider_backed: BTreeSet::new(),
        names: BTreeSet::new(),
        compiled: BTreeMap::new(),
    };
    let mut seen: Vec<String> = Vec::new();
    for def in set.iter(Entity::Connector) {
        // A connector over a Direct Rust driver named by id (an edition's, such
        // as `com.orion.enterprise.kafka`) has no Community connector type for
        // the typed functions to check; it is reached through `connector_call`
        // and a channel's `connections`. `compile`'s own conversion is the
        // judgement, so the two cannot disagree about it.
        if def.doc.get("provider").is_some() {
            let name = def.doc.get("name").and_then(Value::as_str).unwrap_or("?");
            match crate::definitions::deployment::connection(&def.doc, &def.origin) {
                Ok((connection, _notes)) => {
                    if seen.iter().any(|n| n == name) {
                        findings.push(Diagnostic::error(
                            "duplicate.connector_name",
                            format!("connector '{name}'"),
                            "two connectors in the set share this name",
                        ));
                    }
                    seen.push(name.to_string());
                    connectors.names.insert(name.to_string());
                    connectors.provider_backed.insert(name.to_string());
                    connectors.compiled.insert(name.to_string(), connection);
                }
                Err(e) => findings.push(Diagnostic::error(
                    "schema.connector",
                    format!("connector '{name}'"),
                    e,
                )),
            }
            continue;
        }
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
        connectors.names.insert(req.name.clone());
        connectors.facts.insert(
            req.name.clone(),
            crate::engine::ConnectorFacts {
                connector_type: req.connector_type,
            },
        );
        // A typed connector can be a binding target. Its driver comes from
        // the same conversion `compile` uses; a conversion problem is already
        // reported by the schema or db-driver pass, so it is not repeated.
        if let Ok((connection, _notes)) =
            crate::definitions::deployment::connection(&def.doc, &def.origin)
        {
            connectors.compiled.insert(req.name, connection);
        }
    }
    connectors
}

fn check_workflows(
    set: &DefinitionSet,
    require_explicit_ids: bool,
    functions: &FunctionRegistry,
    findings: &mut Vec<Diagnostic>,
) -> Workflows {
    let loop_cap = crate::config::EngineConfig::default().max_loop_iterations;
    let mut ids = Vec::new();
    let mut versions: std::collections::BTreeMap<String, BTreeSet<u64>> = Default::default();
    let mut unversioned: BTreeSet<String> = BTreeSet::new();
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
        for task in crate::engine::leaf_tasks(&req.tasks, req.loop_config.as_ref()) {
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
        // One engine parse for the refusal and the advisories below (P-27).
        let workflow_issues =
            crate::validation::workflow_issues(&req.tasks, req.loop_config.as_ref());
        if let Err(e) = crate::validation::validate_create_workflow_with_issues(
            &req,
            loop_cap,
            functions,
            workflow_issues
                .as_ref()
                .map(crate::validation::ParsedWorkflow::parse_issues),
        ) {
            let entity = format!("workflow '{}'", req.name);
            for d in schema_diagnostics("schema.workflow", &entity, def, &e) {
                let named = unknown_function_name(def, &d);
                if let Some(name) = named.and_then(|n| unverifiable.iter().find(|u| *u == n)) {
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
                    named.and_then(|n| undeclared.iter().find(|(u, _, _)| u == n))
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
        for (path, message) in crate::validation::secret_reference_errors(
            &req.tasks,
            req.loop_config.as_ref(),
            functions,
        ) {
            findings.push(Diagnostic::error(
                "env.unresolved",
                format!("workflow '{}' {path}", req.name),
                message,
            ));
        }
        // The advisory the single-file lint already emits, carried into set
        // mode so a directory gate is not weaker than the per-file one.
        for (path, message) in crate::validation::unresolvable_logic_warnings(
            &req.tasks,
            req.loop_config.as_ref(),
            functions,
        ) {
            findings.push(Diagnostic::warning(
                "logic.unresolvable",
                format!("workflow '{}' {path}", req.name),
                message,
            ));
        }
        // Likewise for what the engine reports and does not refuse: a
        // `$`-prefixed key that loses one `$` when it is emitted, and a
        // `validation` whose failure changes nothing.
        for advisory in crate::validation::engine_advisories_from(
            &req.tasks,
            req.loop_config.as_ref(),
            functions,
            workflow_issues,
        ) {
            findings.push(Diagnostic::warning(
                advisory.check,
                format!("workflow '{}' {}", req.name, advisory.path),
                advisory.message,
            ));
        }
        let version = match def.doc.as_object() {
            Some(doc) => match crate::definitions::deployment::identity_version(
                doc,
                &format!("workflow '{}'", req.name),
            ) {
                Ok(version) => version,
                Err(error) => {
                    findings.push(Diagnostic::error(
                        "schema.workflow",
                        format!("workflow '{}'", req.name),
                        error,
                    ));
                    None
                }
            },
            None => None,
        };
        match &req.workflow_id {
            Some(id) => {
                let duplicate = match version {
                    Some(version) => {
                        unversioned.contains(id)
                            || !versions.entry(id.clone()).or_default().insert(version)
                    }
                    None => versions.contains_key(id) || !unversioned.insert(id.clone()),
                };
                if duplicate {
                    findings.push(Diagnostic::error(
                        "duplicate.workflow_id",
                        format!("workflow '{}'", req.name),
                        match version {
                            Some(version) => format!(
                                "two workflows in the set share workflow_id '{id}' and \
                                 version {version}; each variant needs a distinct `version`"
                            ),
                            None => format!(
                                "a second workflow in the set shares workflow_id '{id}': give \
                                 each revision a distinct explicit `version`"
                            ),
                        },
                    ));
                }
                if !ids.iter().any(|known| known == id) {
                    ids.push(id.clone());
                }
                tasks.push((id.clone(), req.tasks.clone(), req.loop_config.clone()));
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
            None => tasks.push((
                def.origin.clone(),
                req.tasks.clone(),
                req.loop_config.clone(),
            )),
        }
    }
    Workflows {
        ids,
        versions,
        tasks,
    }
}

fn check_channels(
    set: &DefinitionSet,
    workflows: &Workflows,
    drivers: &crate::extensions::ChannelDriverRegistry,
    known_connector: &dyn Fn(&str) -> bool,
    require_explicit_ids: bool,
    findings: &mut Vec<Diagnostic>,
) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut channel_ids: Vec<String> = Vec::new();
    // (canonical route, methods, priority, the channel that claimed it first)
    // — a list rather than a map because "same route" is an overlap test, not
    // a key lookup.
    let mut routes: Vec<(String, Vec<String>, i64, String)> = Vec::new();

    // Offline, the drivers are the target node's when the caller described it
    // (`--capabilities`), and otherwise Community's: what a node built from
    // this binary serves.
    for def in set.iter(Entity::Channel) {
        // v1's Kafka routing fields. A channel definition has no place for
        // them (upload refuses them as unknown, and so does `compile`); a
        // protocol that consumes topics names them in its `transport_config`.
        for key in ["topic", "consumer_group"] {
            if def.doc.get(key).is_some_and(|v| !v.is_null()) {
                findings.push(Diagnostic::error(
                    "schema.channel",
                    &def.origin,
                    format!(
                        "`{key}` is not a channel field; a protocol that consumes topics \
                         names them in its transport_config"
                    ),
                ));
            }
        }
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
        if let Err(e) = crate::validation::validate_create_channel(&req, drivers) {
            findings.extend(schema_diagnostics(
                "schema.channel",
                &format!("channel '{}'", req.name),
                def,
                &e,
            ));
        }
        let rollouts = match crate::definitions::deployment::parse_channel_rollouts(
            &def.doc,
            &format!("channel '{}'", req.name),
        ) {
            Ok(rollouts) => Some(rollouts),
            Err(error) => {
                let at = "channel.rollouts";
                findings.push(
                    Diagnostic::error("rollouts.invalid", format!("channel '{}'", req.name), error)
                        .with_location(&def.origin, Some(at), def.locate(at)),
                );
                None
            }
        };
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
            drivers,
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

        // `connections`: what the channel's driver connects with. A set-document
        // key like `workflow_id`: `compile` turns each into a requirement of
        // the channel's member.
        {
            for name in &req.connections {
                if !known_connector(name) {
                    findings.push(Diagnostic::error(
                        "closure.connector",
                        format!("channel '{}'", req.name),
                        format!(
                            "connection '{name}' is neither in the set nor declared on the \
                             boundary"
                        ),
                    ));
                }
            }
        }

        match &req.workflow_id {
            Some(wf) if !wf.is_empty() => {
                if !workflows.ids.iter().any(|id| id == wf) {
                    findings.push(Diagnostic::error(
                        "closure.workflow",
                        format!("channel '{}'", req.name),
                        format!("workflow '{wf}' is not in the set"),
                    ));
                } else if let Some(rollouts) = &rollouts {
                    let versions = workflows.versions.get(wf);
                    if rollouts.is_empty() {
                        if versions.is_some_and(|versions| versions.len() > 1) {
                            findings.push(Diagnostic::error(
                                "rollouts.ambiguous",
                                format!("channel '{}'", req.name),
                                format!(
                                    "workflow '{wf}' has {} versions in the set, so the channel \
                                     must name which one each bucket serves with `rollouts`",
                                    versions.map_or(0, BTreeSet::len)
                                ),
                            ));
                        }
                    } else {
                        for (index, rollout) in rollouts.iter().enumerate() {
                            if versions.is_some_and(|versions| versions.contains(&rollout.version))
                            {
                                continue;
                            }
                            let at = format!("channel.rollouts[{index}].version");
                            findings.push(
                                Diagnostic::error(
                                    "rollouts.unknown_version",
                                    format!("channel '{}'", req.name),
                                    format!(
                                        "`rollouts` names version {} of workflow '{wf}', and \
                                         the set carries no such version",
                                        rollout.version
                                    ),
                                )
                                .with_location(
                                    &def.origin,
                                    Some(&at),
                                    def.locate(&at),
                                ),
                            );
                        }
                    }
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
/// A `connector_call` naming an operation its provider does not declare:
/// the check the node makes when the workflow loads, made offline. The
/// provider is the call's literal `target_id`; its declaration is a built-in
/// driver's, or the target node's (`--capabilities`). A provider that
/// declares nothing, or a target the set cannot name offline, is judged by
/// the node.
fn check_operations(workflows: &Workflows, boundary: &Boundary, findings: &mut Vec<Diagnostic>) {
    for (workflow, tasks, loop_config) in &workflows.tasks {
        for task in crate::engine::leaf_tasks(tasks, loop_config.as_ref()) {
            if task["function"]["name"] != crate::engine::functions::connector_call::NAME {
                continue;
            }
            let input = &task["function"]["input"];
            let (Some(target), Some(operation)) =
                (input["target_id"].as_str(), input["operation"].as_str())
            else {
                continue;
            };
            if operation.starts_with("orion.internal.") {
                continue;
            }
            let Some(described) = boundary.describe_provider(target) else {
                continue;
            };
            if described.operation(operation).is_none() {
                let declared: Vec<&str> = described.operations.keys().map(String::as_str).collect();
                findings.push(Diagnostic::error(
                    "closure.operation",
                    format!("workflow '{workflow}'"),
                    format!(
                        "provider '{target}' has no operation '{operation}' (it declares: {})",
                        declared.join(", ")
                    ),
                ));
            }
        }
    }
}

/// Every connection over a provider that declares dependencies, checked
/// against that declaration: an unknown alias, an unbound required alias, a
/// target on the wrong provider, a target that itself declares dependencies,
/// and bindings on a provider that declares none.
///
/// A provider the boundary cannot describe is skipped, like an operation
/// check on one: the target node judges it. The rule mirrors the server's
/// `binding_problems` (`runtime::extensions::workflow_provider::validation`),
/// so a set that lints clean is one the admin API accepts.
fn check_bindings(
    set: &DefinitionSet,
    boundary: &Boundary,
    connectors: &Connectors,
    findings: &mut Vec<Diagnostic>,
) {
    for def in set.iter(Entity::Connector) {
        let Some(name) = def.doc.get("name").and_then(Value::as_str) else {
            continue;
        };
        // Only a `provider` declares dependencies; a `connector_type` driver
        // declares none and cannot bind.
        if def.doc.get("provider").is_none() {
            continue;
        }
        let Some(connection) = connectors.compiled.get(name) else {
            continue;
        };
        let Some(described) = boundary.describe_provider(&connection.driver) else {
            continue;
        };
        let entity = format!("connector '{name}'");
        let report = |findings: &mut Vec<Diagnostic>,
                      code: &'static str,
                      alias: Option<&str>,
                      message: String| {
            let mut diagnostic = Diagnostic::error(code, &entity, message);
            if let Some(alias) = alias {
                let path = format!("connector.bindings.{alias}");
                diagnostic =
                    diagnostic.with_location(&def.origin, Some(path.as_str()), def.locate(&path));
            }
            findings.push(diagnostic);
        };

        if described.dependencies.is_empty() {
            if !connection.bindings.is_empty() {
                report(
                    findings,
                    "binding.undeclared",
                    None,
                    "this connection's provider declares no dependencies, so it binds none"
                        .to_string(),
                );
            }
            continue;
        }

        for (alias, dependency) in &described.dependencies {
            if !dependency.optional && !connection.bindings.contains_key(alias) {
                report(
                    findings,
                    "binding.required",
                    Some(alias),
                    format!(
                        "provider '{}' declares the dependency '{alias}': bind it to a \
                         connection",
                        connection.driver
                    ),
                );
            }
        }

        for (alias, target) in &connection.bindings {
            let Some(dependency) = described.dependencies.get(alias) else {
                report(
                    findings,
                    "binding.unknown_alias",
                    Some(alias),
                    format!(
                        "provider '{}' declares no dependency '{alias}'",
                        connection.driver
                    ),
                );
                continue;
            };
            if target == name {
                report(
                    findings,
                    "binding.self",
                    Some(alias),
                    "a connection cannot be its own dependency".to_string(),
                );
                continue;
            }
            let Some(target_connection) = connectors.compiled.get(target) else {
                if !connectors.names.contains(target) {
                    report(
                        findings,
                        "binding.target_missing",
                        Some(alias),
                        format!("no connector '{target}' is in the set"),
                    );
                }
                continue;
            };
            let target_driver = target_connection.driver.as_str();
            if let Some(wanted) = &dependency.provider
                && wanted != target_driver
            {
                report(
                    findings,
                    "binding.wrong_provider",
                    Some(alias),
                    format!(
                        "dependency '{alias}' runs on '{wanted}', and connection '{target}' \
                         on '{target_driver}'"
                    ),
                );
            }
            if boundary
                .describe_provider(target_driver)
                .is_some_and(|described| !described.dependencies.is_empty())
            {
                report(
                    findings,
                    "binding.chained",
                    Some(alias),
                    format!(
                        "connection '{target}' runs on '{target_driver}', which declares \
                         dependencies of its own: a dependency's dependencies are not carried, \
                         so they cannot chain"
                    ),
                );
            }
        }
    }
}

fn check_closure(
    workflows: &Workflows,
    connectors: &BTreeMap<String, crate::engine::ConnectorFacts>,
    channels: &[String],
    boundary: &Boundary,
    functions: &FunctionRegistry,
    findings: &mut Vec<Diagnostic>,
) {
    for (workflow, tasks, loop_config) in &workflows.tasks {
        let loop_config = loop_config.as_ref();
        let entity = format!("workflow '{workflow}'");

        // The rules are `engine::check_connector_refs`, shared with the
        // activation gate the admin API runs (`admin::services::workflows`).
        // Only the lookup differs: there a live registry, here the set's own
        // connector definitions.
        for problem in crate::engine::check_connector_refs(tasks, loop_config, functions, |name| {
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
            }
        }

        let (targets, dynamic) = crate::engine::channel_call_targets(tasks, loop_config);
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
    use crate::definitions::{Boundary, Entity};
    use serde_json::json;

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

    /// B-08: the unverifiable/undeclared classifier tells a schema refusal
    /// apart by its `FieldError` code and the function read from the document
    /// at the refused path — not by the validator's message wording.
    #[test]
    fn an_unknown_function_is_named_from_the_document_and_its_code() {
        let def = Definition {
            entity: Entity::Workflow,
            origin: "w.json".to_string(),
            doc: json!({
                "workflow_id": "w",
                "name": "w",
                "tasks": [{"function": {"name": "acme-lib.parse"}}],
                "loop": {"setup": [{"function": {"name": "acme-lib.next"}}]},
            }),
            spans: None,
            provenance: Default::default(),
        };

        let mut d = Diagnostic::from_field_error(
            "schema.workflow",
            "workflow 'w'",
            &orion_api::FieldError::new(
                "tasks[0].function.name",
                orion_api::error::field_codes::UNKNOWN_FUNCTION,
                "changed wording",
            ),
        );
        assert_eq!(unknown_function_name(&def, &d), Some("acme-lib.parse"));

        d.path = Some("loop.setup[0].function.name".to_string());
        assert_eq!(unknown_function_name(&def, &d), Some("acme-lib.next"));

        // A different code is not the unknown-function class at all.
        d.code = Some(orion_api::error::field_codes::REQUIRED.to_string());
        assert_eq!(unknown_function_name(&def, &d), None);
    }

    /// The defect this module's `schema_diagnostics` exists to close.
    ///
    /// A workflow with several schema problems must not collapse into one
    /// finding whose message is the whole `OrionError` flattened with
    /// `to_string()` — a set gate could then say the workflow failed
    /// validation but not which field, and a pipeline would have nothing to
    /// grandfather but the entire `schema.workflow` family. One diagnostic per
    /// field error, each carrying its own path.
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
    /// `definitions/json.rs` exists to produce exactly this; a set that threw
    /// its output away would leave `lint`, `check` and `compile` findings with
    /// no origin at all.
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

    /// Two revisions of one workflow are legal when each names a distinct
    /// `version`; a true repeat is still `duplicate.workflow_id`.
    #[test]
    fn workflow_variants_need_distinct_versions() {
        let versioned = |version: u64, name: &str| {
            (
                Entity::Workflow,
                format!("{name}.json"),
                json!({"workflow_id": "wf", "version": version, "name": name, "tasks": []}),
            )
        };
        let set = DefinitionSet::from_entries([versioned(1, "A"), versioned(2, "B")]);
        let findings = check(
            &set,
            &Boundary::default(),
            false,
            FunctionRegistry::builtin(),
        );
        assert!(
            checks(&findings, "duplicate.workflow_id").is_empty(),
            "{findings:?}"
        );

        let set = DefinitionSet::from_entries([versioned(1, "A"), versioned(1, "B")]);
        let findings = check(
            &set,
            &Boundary::default(),
            false,
            FunctionRegistry::builtin(),
        );
        assert_eq!(checks(&findings, "duplicate.workflow_id").len(), 1);
        assert!(
            checks(&findings, "duplicate.workflow_id")[0]
                .message
                .contains("version 1"),
            "{findings:?}"
        );
    }

    /// A channel's split names versions that exist, and a multi-version
    /// workflow cannot be served without one.
    #[test]
    fn channel_rollouts_must_name_existing_versions_and_cannot_be_ambiguous() {
        let versioned = |version: u64, name: &str| {
            (
                Entity::Workflow,
                format!("{name}.json"),
                json!({"workflow_id": "wf", "version": version, "name": name, "tasks": []}),
            )
        };
        let channel = |rollouts: Option<Value>| {
            let mut doc = json!({"channel_id": "c", "name": "c", "channel_type": "sync",
                                 "protocol": "rest", "workflow_id": "wf"});
            if let Some(rollouts) = rollouts {
                doc["rollouts"] = rollouts;
            }
            (Entity::Channel, "c.json".to_string(), doc)
        };

        let set = DefinitionSet::from_entries([
            versioned(1, "A"),
            versioned(2, "B"),
            channel(Some(json!([
                {"version": 1, "start_bucket": 0, "end_bucket": 50},
                {"version": 2, "start_bucket": 50, "end_bucket": 100}
            ]))),
        ]);
        let findings = check(
            &set,
            &Boundary::default(),
            false,
            FunctionRegistry::builtin(),
        );
        assert!(
            checks(&findings, "rollouts.invalid").is_empty(),
            "{findings:?}"
        );
        assert!(
            checks(&findings, "rollouts.unknown_version").is_empty(),
            "{findings:?}"
        );
        assert!(
            checks(&findings, "rollouts.ambiguous").is_empty(),
            "{findings:?}"
        );

        let set = DefinitionSet::from_entries([
            versioned(1, "A"),
            versioned(2, "B"),
            channel(Some(json!([
                {"version": 1, "start_bucket": 0, "end_bucket": 50},
                {"version": 9, "start_bucket": 50, "end_bucket": 100}
            ]))),
        ]);
        let findings = check(
            &set,
            &Boundary::default(),
            false,
            FunctionRegistry::builtin(),
        );
        assert_eq!(checks(&findings, "rollouts.unknown_version").len(), 1);
        assert!(
            checks(&findings, "rollouts.unknown_version")[0]
                .message
                .contains("version 9"),
            "{findings:?}"
        );

        let set =
            DefinitionSet::from_entries([versioned(1, "A"), versioned(2, "B"), channel(None)]);
        let findings = check(
            &set,
            &Boundary::default(),
            false,
            FunctionRegistry::builtin(),
        );
        assert_eq!(checks(&findings, "rollouts.ambiguous").len(), 1);

        let set = DefinitionSet::from_entries([
            versioned(1, "A"),
            versioned(2, "B"),
            channel(Some(json!([
                {"version": 1, "start_bucket": 0, "end_bucket": 40},
                {"version": 2, "start_bucket": 50, "end_bucket": 100}
            ]))),
        ]);
        let findings = check(
            &set,
            &Boundary::default(),
            false,
            FunctionRegistry::builtin(),
        );
        assert_eq!(checks(&findings, "rollouts.invalid").len(), 1);
    }

    /// The binding rule lives here now: a provider's declared aliases decide
    /// what a connection may bind, and a target must be able to serve.
    #[test]
    fn provider_bindings_are_checked_against_the_declaration() {
        use orion_extension_api::{
            DependencyDescription, OperationDescription, ProviderDescription,
        };

        let connector = |name: &str, provider: &str, bindings: Value| {
            let mut doc = json!({"name": name, "provider": provider, "config": {}});
            if !bindings.is_null() {
                doc["bindings"] = bindings;
            }
            (Entity::Connector, format!("{name}.json"), doc)
        };
        let describe = |dependencies: Vec<(&str, DependencyDescription)>| {
            let mut description =
                ProviderDescription::new().with_operation("get", OperationDescription::new());
            for (alias, dependency) in dependencies {
                description = description.with_dependency(alias, dependency);
            }
            description
        };
        let boundary = Boundary {
            provider_operations: Some(BTreeMap::from([
                (
                    "com.orion.models".to_string(),
                    describe(vec![(
                        "store",
                        DependencyDescription::new().on_provider("com.orion.storage"),
                    )]),
                ),
                ("com.orion.storage".to_string(), describe(Vec::new())),
                ("com.orion.http".to_string(), describe(Vec::new())),
            ])),
            ..Boundary::default()
        };
        let bindings = |findings: &[Diagnostic]| -> Vec<String> {
            findings
                .iter()
                .filter(|finding| finding.check.starts_with("binding."))
                .map(|finding| format!("{}: {}", finding.check, finding.message))
                .collect()
        };
        let run = |definitions| {
            let set = DefinitionSet::from_entries(definitions);
            check(&set, &boundary, false, FunctionRegistry::builtin())
        };

        // A required alias bound to a connection over the provider the
        // dependency names is clean.
        let findings = run(vec![
            connector("store", "com.orion.storage", Value::Null),
            connector("models", "com.orion.models", json!({"store": "store"})),
        ]);
        assert!(bindings(&findings).is_empty(), "{:?}", bindings(&findings));

        // Unbound required alias.
        let findings = run(vec![
            connector("store", "com.orion.storage", Value::Null),
            connector("models", "com.orion.models", Value::Null),
        ]);
        assert!(
            bindings(&findings)
                .iter()
                .any(|finding| finding.starts_with("binding.required:")),
            "{:?}",
            bindings(&findings)
        );

        // Unknown alias.
        let findings = run(vec![
            connector("store", "com.orion.storage", Value::Null),
            connector(
                "models",
                "com.orion.models",
                json!({"store": "store", "nope": "store"}),
            ),
        ]);
        assert!(
            bindings(&findings)
                .iter()
                .any(|finding| finding.starts_with("binding.unknown_alias:")),
            "{:?}",
            bindings(&findings)
        );

        // A provider that declares nothing binds nothing.
        let findings = run(vec![
            connector("store", "com.orion.storage", Value::Null),
            connector("http", "com.orion.http", json!({"anything": "store"})),
        ]);
        assert!(
            bindings(&findings)
                .iter()
                .any(|finding| finding.starts_with("binding.undeclared:")),
            "{:?}",
            bindings(&findings)
        );

        // A target on the wrong provider.
        let findings = run(vec![
            connector("api", "com.orion.http", Value::Null),
            connector("models", "com.orion.models", json!({"store": "api"})),
        ]);
        assert!(
            bindings(&findings)
                .iter()
                .any(|finding| finding.starts_with("binding.wrong_provider:")),
            "{:?}",
            bindings(&findings)
        );

        // A target that is not in the set.
        let findings = run(vec![connector(
            "models",
            "com.orion.models",
            json!({"store": "nowhere"}),
        )]);
        assert!(
            bindings(&findings)
                .iter()
                .any(|finding| finding.starts_with("binding.target_missing:")),
            "{:?}",
            bindings(&findings)
        );

        // Dependencies do not chain: a target over a provider that declares
        // its own dependencies is refused.
        let findings = run(vec![
            connector("store", "com.orion.storage", Value::Null),
            connector("other", "com.orion.models", Value::Null),
            connector("models", "com.orion.models", json!({"store": "other"})),
        ]);
        assert!(
            bindings(&findings)
                .iter()
                .any(|finding| finding.starts_with("binding.chained:")),
            "{:?}",
            bindings(&findings)
        );
    }
}
