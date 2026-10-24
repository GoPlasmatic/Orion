//! Turning stored channel + workflow rows into the workflow sets the engines
//! are built from — including the per-channel quarantine that keeps one bad
//! row from taking the instance down.
//!
//! **dataflow-rs 4 decides which rows load** (I182, the building cutover).
//! Every row is compiled on dataflow-rs 4 first ([`compile_row`]), and its
//! verdict decides quarantine ([`quarantines`]). dataflow-rs 3 serves, from
//! the rows dataflow-rs 4 accepted; any row dataflow-rs 3 refuses still
//! quarantines its channel, with dataflow-rs 3's reason. A rollout set stays
//! all or nothing on both.

use std::collections::HashMap;

use dataflow_rs::Issue;
use dataflow_rs::def::{ParseOptions, parse};
use dataflow_rs::program::{
    Compiled, CompiledWorkflow, Compiler, IssueCode, Program, Severity, StepKind,
};
use serde_json::{Map, Value};

use crate::engine::FunctionRegistry;
use crate::plugin::PluginSet;
use crate::storage::models::{Channel, Workflow};
use crate::storage::repositories::workflows::{
    workflow_json, workflow_to_dataflow, workflow_to_dataflow_with_rollout,
};

/// Filter channels based on include/exclude glob patterns from [`ChannelFilterConfig`].
///
/// - If `include` is non-empty, only channels matching at least one include pattern are kept.
/// - Channels matching any `exclude` pattern are removed (applied after include).
/// - Supports simple `*` wildcards (e.g., `internal-*`, `*-debug`).
///
/// [`ChannelFilterConfig`]: crate::config::ChannelFilterConfig
pub fn filter_channels(
    channels: Vec<Channel>,
    config: &crate::config::ChannelFilterConfig,
) -> Vec<Channel> {
    if config.include.is_empty() && config.exclude.is_empty() {
        return channels;
    }

    channels
        .into_iter()
        .filter(|ch| {
            // Include filter: if non-empty, channel must match at least one pattern
            if !config.include.is_empty() && !config.include.iter().any(|p| glob_match(p, &ch.name))
            {
                return false;
            }
            // Exclude filter: channel must not match any exclude pattern
            !config.exclude.iter().any(|p| glob_match(p, &ch.name))
        })
        .collect()
}

/// Glob matching supporting `*` wildcards, with real backtracking.
///
/// F32: the previous successive-`find` implementation diverged from glob
/// semantics on patterns needing backtracking (`a*bc` vs `abxbc` matched the
/// first `b` and failed) — and a wrong `channels.include`/`exclude` pattern
/// silently drops a channel. This is the standard two-pointer matcher:
/// linear scan with a single saved star position to retry from.
fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            // Backtrack: let the last `*` swallow one more character.
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Something that can answer "will the handlers that run this workflow
/// actually dispatch every task in it, and do their inputs parse?".
///
/// Two implementations, because the two callers hold different things. At boot
/// the handler map is still on an [`EngineBuilder`] that has not been built
/// yet; on reload the handlers live on the running [`Engine`], which
/// `with_new_workflows` carries across. Both expose the same
/// `check_workflow`, so this trait exists only to name that shared shape.
///
/// [`Engine`]: dataflow_rs_v3::Engine
/// [`EngineBuilder`]: dataflow_rs_v3::engine::EngineBuilder
pub trait HandlerScreen {
    /// Every reason this workflow would not run, without building anything.
    fn check_workflow(
        &self,
        workflow: &dataflow_rs_v3::Workflow,
    ) -> Vec<dataflow_rs_v3::WorkflowIssue>;
}

impl HandlerScreen for dataflow_rs_v3::Engine {
    fn check_workflow(
        &self,
        workflow: &dataflow_rs_v3::Workflow,
    ) -> Vec<dataflow_rs_v3::WorkflowIssue> {
        dataflow_rs_v3::Engine::check_workflow(self, workflow)
    }
}

impl HandlerScreen for dataflow_rs_v3::engine::EngineBuilder {
    fn check_workflow(
        &self,
        workflow: &dataflow_rs_v3::Workflow,
    ) -> Vec<dataflow_rs_v3::WorkflowIssue> {
        dataflow_rs_v3::engine::EngineBuilder::check_workflow(self, workflow)
    }
}

/// Screen one converted workflow against the handlers that will run it.
///
/// Without this, a task naming an unregistered function or carrying an input
/// its handler cannot parse is invisible until engine construction — which is
/// a whole-instance failure, not a per-channel one: at boot the process
/// aborts, and on reload every channel on every node goes down because one
/// stored row is unusable. That defeats the F33/F35 quarantine, whose premise
/// is that one broken row must never stop the instance. Checking here turns it
/// back into a `ChannelLoadIssue` (F41).
///
/// This used to be a hand-written mirror: a membership test against a
/// hand-kept list of registered names, a `match` naming the one handler with a typed `Input`, and
/// a locally-built datalogic engine standing in for the crate-private
/// `TemplateCompiler`. dataflow-rs 3.7's `check_workflow` does all three
/// against the real registry and the real compiler, so the mirror is gone and
/// with it three ways for it to drift:
///
/// - A new handler no longer has to be added to a list to be screened.
/// - A handler that grows a typed `Input` is screened for it immediately,
///   rather than silently going unchecked until someone remembers the `match`.
/// - `http_call`, `publish_kafka` and `enrich` are covered too. They
///   deserialize into typed built-in variants, so they never reached the
///   `Custom` arm the mirror inspected — an `enrich` task in a stored row
///   built cleanly and then failed every request with `FunctionNotFound`.
///
/// Issues are joined into one message: the caller reports per workflow, and a
/// workflow that cannot run is quarantined whole regardless of how many of its
/// tasks are the reason.
///
/// **Not every issue `check_workflow` reports makes a workflow unusable**, and
/// the engine is what says which. Until dataflow-rs 3.9 every code it returned
/// was one `Engine::build` also refused, so "any issue" and "cannot run" were
/// the same set and this screen could take the whole list. `ESCAPED_TEMPLATE_KEY`
/// broke that and 3.10 added two more, which Orion tracked in a hand-kept list
/// that could only ever be wrong in one direction — silently, on upgrade,
/// quarantining channels that were fine. 3.11's `Severity` replaced it:
/// `severity()` is a match with no wildcard arm, so a code added in a later
/// minor is classified upstream before it can reach here.
///
/// `Advisory` is the only class this may pass. **`Defect` must still
/// quarantine**: `MISSING_HANDLER` is a config-only integration whose typed
/// variant parses, so `build` accepts it and every message then fails — F54,
/// the `enrich` case that made this function exist. Screening on "would this
/// build" would call that healthy.
///
/// Filtered here rather than at each caller because all four surfaces — boot,
/// reload, `dry-run` and the test endpoint — must agree on what "unusable"
/// means.
fn screen_workflow(
    workflow: &dataflow_rs_v3::Workflow,
    screen: &dyn HandlerScreen,
) -> Result<(), String> {
    // `check_workflow` asks about handlers, not structure: it never runs
    // `Workflow::validate()`, which `build` runs on every workflow and fails
    // on the first refusal. Unscreened, a duplicate step id across
    // `loop.setup` and `tasks`, or a loop `as` that is not a plain key, failed
    // every reload and stopped a node at boot.
    workflow.validate().map_err(|e| e.to_string())?;
    let issues = screen.check_workflow(workflow);
    let (advisories, issues): (Vec<_>, Vec<_>) = issues
        .into_iter()
        .partition(|i| i.severity() == dataflow_rs_v3::Severity::Advisory);
    // Dropped from the fatal set but not dropped. Each of the three says
    // something a load is the last chance to notice: a stripped `$` changes the
    // shape of a composed document without failing anything, a `validation`
    // asserts nothing, a group's `continue_on_error` is discarded. None of them
    // stops the workflow running, which is why they are logged here rather than
    // returned. `lint` and `preflight` report the same findings against a
    // document the author can still edit.
    for advisory in &advisories {
        tracing::warn!(
            workflow = %workflow.id,
            task = advisory.task_id.as_deref().unwrap_or("-"),
            path = advisory.path.as_deref().unwrap_or("-"),
            "{}",
            advisory.message
        );
    }
    if issues.is_empty() {
        return Ok(());
    }
    Err(issues
        .iter()
        .map(|issue| match &issue.task_id {
            Some(task_id) => format!("task '{task_id}': {}", issue.message),
            None => issue.message.clone(),
        })
        .collect::<Vec<_>>()
        .join("; "))
}

/// Build an engine over exactly one workflow, screened the way the serving
/// engine screens.
///
/// Four places in the tree build a workflow engine, and until this existed only
/// two of them went through the [`HandlerScreen`]: boot and reload. The other
/// two — `orion-server dry-run` and `POST /workflows/{id}/test` — assembled the
/// builder by hand and called `.build()`, so they answered a *different
/// question* from the one they are asked.
///
/// `.build()` alone is not the same check. It refuses an unregistered *custom*
/// function, but `http_call`, `publish_kafka` and `enrich` deserialize into
/// typed built-in variants, so a workflow naming one builds cleanly with no
/// handler behind it and fails every request with `FunctionNotFound`. Orion
/// registers the first two and never registers `enrich` — which is exactly the
/// case that made this screen exist (F54). The author saw a green dry run and a
/// channel that would not serve; worse in reverse, since "test this workflow"
/// is the endpoint people trust before activating.
///
/// So the screen is not optional and not the caller's to remember. `handlers`
/// go on the builder first, because that is what the screen consults — pass
/// the same map the surface would run with (the real handlers for the test
/// endpoint, the stub table for a dry run), and the answer is about *that*
/// engine.
///
/// `Err` is the joined screen message, in the same wording
/// [`build_engine_workflows`] puts in a `ChannelLoadIssue`, so an author reads
/// one explanation of an unusable task wherever they meet it.
pub fn build_single(
    workflow: dataflow_rs_v3::Workflow,
    handlers: HashMap<String, dataflow_rs_v3::BoxedFunctionHandler>,
    secrets: &crate::engine::ResolvedSecrets,
    ops_budget: u64,
) -> Result<dataflow_rs_v3::Engine, crate::errors::OrionError> {
    // `ops_budget` is `engine.ops_budget` where a config exists (the admin
    // test endpoint) and `0` where none does (an offline `dry-run`), so a
    // test run on the server is bounded exactly as serving is.
    let builder = crate::engine::operators::with_ops_budget(
        crate::engine::operators::with_orion_engine_defaults(
            dataflow_rs_v3::Engine::builder(),
            secrets,
        ),
        ops_budget,
    )
    .with_handlers(handlers);

    screen_workflow(&workflow, &builder).map_err(|e| {
        crate::errors::OrionError::validation(format!(
            "workflow '{}' has an unusable task: {e}",
            workflow.id
        ))
    })?;

    builder
        .with_workflow(workflow)
        .build()
        .map_err(crate::errors::OrionError::Engine)
}

// -- dataflow-rs 4: the verdict that decides quarantine (I182) --

/// One alias the loader rewrote to its canonical key (I186).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasRewrite {
    /// The task's path in the workflow: `tasks[0]`, `tasks[1].tasks[0]`,
    /// `loop.setup[0]`.
    pub task: String,
    /// The task's id, as authored, when it has one.
    pub task_id: Option<String>,
    /// The function the task calls, by its canonical name.
    pub function: String,
    /// The spelling the row stores.
    pub alias: String,
    /// The spelling dataflow-rs 4's config reads.
    pub canonical: String,
}

/// The aliases 1.12 read through a serde alias on a handler's own input
/// struct and that its `schema.rs` row does not declare: `(function, alias,
/// canonical)`.
///
/// One: `channel_call`'s `response_path`, `ChannelCallInput::output`'s serde
/// alias, which its field row describes as "still accepted" without
/// declaring it, so the authoring check neither refuses both spellings at
/// once nor reads the alias's kind. Declaring it on the row would change what
/// create refuses; rewriting it here keeps what 1.12 loaded loading, and
/// nothing else moves. Every other alias is read from the rows.
const SERDE_ONLY_ALIASES: &[(&str, &str, &str)] = &[("channel_call", "response_path", "output")];

/// Rewrites each stored alias in a task's `function.input` to its canonical
/// key, over every step: `tasks`, every group's members and `loop.setup`
/// (I186). dataflow-rs 4's derived configs have no aliases (D5, I132), so a
/// row that spells `http_call`'s `path_logic`, `body_logic` or
/// `response_path`, `publish_kafka`'s `key_logic` or `value_logic`, or
/// `channel_call`'s `channel_logic`, `data_logic` or `response_path`, is
/// read here as 1.12's serde read it.
///
/// The aliases are the `alias` of each field row of `functions` (the
/// `schema.rs` rows), plus `channel_call`'s `response_path`, 1.12's serde
/// alias its row does not declare. A key keeps its place in the input, since
/// an input's key order is the author's. A task holding both spellings is
/// left as written, and its config refuses it, as 1.12's serde refused a
/// duplicate field. Rewrites `workflow` only, never a stored row; each
/// rewrite is returned for the caller to log, with the workflow and version
/// it knows.
pub fn rewrite_aliases(workflow: &mut Value, functions: &FunctionRegistry) -> Vec<AliasRewrite> {
    let mut rewrites = Vec::new();
    if let Some(tasks) = workflow.get_mut("tasks") {
        rewrite_steps(tasks, "tasks", functions, &mut rewrites);
    }
    if let Some(setup) = workflow
        .get_mut("loop")
        .and_then(|config| config.get_mut("setup"))
    {
        rewrite_steps(setup, "loop.setup", functions, &mut rewrites);
    }
    rewrites
}

/// [`rewrite_aliases`] over one list of steps at `at`. A step carrying a
/// `tasks` key is a group, as dataflow-rs 4's parser reads it.
fn rewrite_steps(
    steps: &mut Value,
    at: &str,
    functions: &FunctionRegistry,
    out: &mut Vec<AliasRewrite>,
) {
    let Some(steps) = steps.as_array_mut() else {
        return;
    };
    for (i, step) in steps.iter_mut().enumerate() {
        let path = format!("{at}[{i}]");
        if let Some(members) = step.get_mut("tasks") {
            rewrite_steps(members, &format!("{path}.tasks"), functions, out);
        } else {
            rewrite_task(step, path, functions, out);
        }
    }
}

/// [`rewrite_aliases`] over one task's input.
fn rewrite_task(
    task: &mut Value,
    path: String,
    functions: &FunctionRegistry,
    out: &mut Vec<AliasRewrite>,
) {
    let Some(entry) = task
        .pointer("/function/name")
        .and_then(Value::as_str)
        .and_then(|name| functions.get(name))
    else {
        return;
    };
    let aliases: Vec<(&str, &str)> = entry
        .input_fields
        .iter()
        .flatten()
        .filter_map(|field| {
            field
                .alias
                .as_deref()
                .map(|alias| (alias, field.name.as_str()))
        })
        .chain(
            SERDE_ONLY_ALIASES
                .iter()
                .filter(|(function, ..)| *function == entry.name)
                .map(|&(_, alias, canonical)| (alias, canonical)),
        )
        .collect();
    if aliases.is_empty() {
        return;
    }
    let task_id = task.get("id").and_then(Value::as_str).map(str::to_string);
    let Some(input) = task
        .pointer_mut("/function/input")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    for (alias, canonical) in aliases {
        if !input.contains_key(alias) || input.contains_key(canonical) {
            continue;
        }
        rename_key(input, alias, canonical);
        out.push(AliasRewrite {
            task: path.clone(),
            task_id: task_id.clone(),
            function: entry.name.clone(),
            alias: alias.to_string(),
            canonical: canonical.to_string(),
        });
    }
}

/// Renames `from` to `to` in `map`, in its place.
fn rename_key(map: &mut Map<String, Value>, from: &str, to: &str) {
    for (key, value) in std::mem::take(map) {
        let key = if key == from { to.to_string() } else { key };
        map.insert(key, value);
    }
}

/// Whether `issue` quarantines its channel (I182, as refined).
///
/// Every **Rejected** issue does. A **Defect** does only where dataflow-rs 4
/// would fail every message or the plan names it:
///
/// - break 21's root-output Defect (I185), `INPUT_PARSE` at `output`, which
///   a function's `prepare` raises for a result that is never an object
///   written at a literal context root (`kit::root_output_defect`, the only
///   `INPUT_PARSE` raised as a Defect);
/// - break 2's `LIKELY_TYPO`, a near miss of a known key, whose setting is
///   silently lost.
///
/// Every other Defect, `PAYLOAD_VAR` included, loads: the loader logs it at
/// `warn`, and `POST /workflows/validate` reports it. Advisories never
/// quarantine. A severity a later dataflow-rs adds quarantines until it is
/// classified here, as the dataflow-rs 3 screen passes only what it knows to
/// be harmless.
pub fn quarantines(issue: &Issue) -> bool {
    match issue.severity {
        Severity::Advisory => false,
        Severity::Defect => matches!(issue.code, IssueCode::InputParse | IssueCode::LikelyTypo),
        _ => true,
    }
}

/// Parses and compiles one workflow definition on dataflow-rs 4
/// (`def::parse` with the compat reader on, then `Compiler::compile_one`).
///
/// `Ok` holds the compiled workflow, with every issue that does not
/// [quarantine](quarantines) it in `issues`: the parse's first, then the
/// compile's. `Err` holds every issue found, in that order, when any does:
/// a Rejected issue, break 21's root-output Defect or break 2's typo.
///
/// # Errors
///
/// Every issue, when one quarantines the workflow.
pub fn compile_row(compiler: &Compiler, json: &Value) -> Result<Compiled, Vec<Issue>> {
    let parsed = parse(json, ParseOptions::default());
    let mut issues = parsed.issues;
    let Some(def) = parsed.def else {
        return Err(issues);
    };
    match compiler.compile_one(&def) {
        Err(mut compiled) => {
            issues.append(&mut compiled);
            Err(issues)
        }
        Ok(mut compiled) => {
            issues.append(&mut compiled.issues);
            if issues.iter().any(quarantines) {
                return Err(issues);
            }
            compiled.issues = issues;
            Ok(compiled)
        }
    }
}

/// The serving engine over `program`: Orion's one error context (I184,
/// `crate::engine::ERROR_CONTEXT`). A reload that keeps the registry builds
/// its engine with `Engine::with_program`, which keeps this config.
pub fn serving_engine(program: Program) -> dataflow_rs::Engine {
    serving_engine_with_policy(program, dataflow_rs::run::ConditionErrorPolicy::default())
}

/// Build a `dataflow-rs` 4 serving engine over `program` with Orion's error
/// context and an explicit [`dataflow_rs::run::ConditionErrorPolicy`]
/// (`ConditionErrorPolicy::Stop` for v2 fail-closed condition evaluation, or
/// `ConditionErrorPolicy::Skip` for v1 compatibility).
pub fn serving_engine_with_policy(
    program: Program,
    condition_policy: dataflow_rs::run::ConditionErrorPolicy,
) -> dataflow_rs::Engine {
    dataflow_rs::Engine::new(
        program,
        dataflow_rs::run::EngineConfig::default()
            .error_context(crate::engine::ERROR_CONTEXT.clone())
            .with_condition_error_policy(condition_policy)
            .observer(std::sync::Arc::new(crate::engine::MetricsObserver)),
    )
}

/// One issue as a quarantine reason names it: the step it sits in, its code,
/// its authored path and its message, then, for a task naming a function of
/// a plugin that did not load, the plugin and why.
fn describe(issue: &Issue, plugins: &PluginSet, json: &Value) -> String {
    let step = match (issue.step_kind, issue.step_id.as_deref()) {
        (Some(kind), Some(id)) if !id.is_empty() => {
            let kind = match kind {
                StepKind::Group => "group",
                StepKind::Setup => "setup step",
                _ => "task",
            };
            format!("{kind} '{id}': ")
        }
        _ => String::new(),
    };
    let path = issue.path.to_string();
    let path = if path.is_empty() { "(root)" } else { &path };
    let mut text = format!("{step}{} at {path}: {}", issue.code.as_str(), issue.message);
    plugins.annotate_issue(issue, json, &mut text);
    text
}

/// The quarantine reason of a workflow dataflow-rs 4 refused: every issue
/// that [quarantines](quarantines) it, described.
fn refusal(issues: &[Issue], plugins: &PluginSet, json: &Value) -> String {
    issues
        .iter()
        .filter(|issue| quarantines(issue))
        .map(|issue| describe(issue, plugins, json))
        .collect::<Vec<_>>()
        .join("; ")
}

/// One row's JSON with its aliases rewritten, each rewrite logged.
fn row_json(
    workflow: &Workflow,
    channel: &str,
    id: &str,
    status: &str,
    rollout: Option<(u8, u8)>,
    functions: &FunctionRegistry,
) -> Result<Value, crate::errors::OrionError> {
    let mut json = workflow_json(workflow, channel, id, status, rollout)?;
    for rewrite in rewrite_aliases(&mut json, functions) {
        tracing::info!(
            workflow = %workflow.workflow_id,
            version = workflow.version,
            task = %rewrite.task,
            task_id = rewrite.task_id.as_deref().unwrap_or("-"),
            function = %rewrite.function,
            alias = %rewrite.alias,
            canonical = %rewrite.canonical,
            "Read a stored alias as its canonical key: dataflow-rs 4 has no alias for it"
        );
    }
    Ok(json)
}

/// Logs what a compile found and did not quarantine on: each Defect (a
/// `PAYLOAD_VAR`, say) and each Advisory, at `warn`, as the dataflow-rs 3
/// screen logs its advisories, with the workflow, version and path.
///
/// A `V3_SPELLING` is logged at `debug`: it says the compat reader read a
/// 1.12 spelling as 1.12 did (a task's `continue_on_error`, say), which every
/// such row raises on every reload, and which changes nothing.
fn log_findings(workflow: &Workflow, compiled: &Compiled) {
    for issue in &compiled.issues {
        if issue.code == IssueCode::V3Spelling {
            tracing::debug!(
                workflow = %workflow.workflow_id,
                version = workflow.version,
                task = issue.step_id.as_deref().unwrap_or("-"),
                path = %issue.path,
                "{}",
                issue.message
            );
            continue;
        }
        tracing::warn!(
            workflow = %workflow.workflow_id,
            version = workflow.version,
            task = issue.step_id.as_deref().unwrap_or("-"),
            path = %issue.path,
            code = issue.code.as_str(),
            severity = issue.severity.as_str(),
            "{}",
            issue.message
        );
    }
}

/// Every active channel's workflows, compiled and screened by both engines.
///
/// For each active channel, finds the associated workflow(s) and builds each
/// row's definition with the channel name injected as its channel.
/// dataflow-rs 4 compiles each first ([`compile_row`]); a row it refuses
/// quarantines the channel with its issues' codes and paths. dataflow-rs 3
/// then converts and screens the rows dataflow-rs 4 accepted, and a row it
/// refuses quarantines the channel with dataflow-rs 3's reason. A channel
/// that loads has its workflows in both returned sets, and only such a
/// channel does: the two engines serve the same channels.
///
/// F33: a channel whose workflows cannot be built — missing `workflow_id`,
/// workflow not found among the active set, or a version that fails
/// conversion — is reported as a [`ChannelLoadIssue`] instead of being
/// silently skipped. Callers feed these into `ChannelLoader::build`, which
/// quarantines the channel: previously it stayed registered in the route
/// table with no workflow behind it, so requests got an opaque engine error.
///
/// [`ChannelLoadIssue`]: crate::channel::ChannelLoadIssue
pub fn build_engine_workflows(
    channels: &[Channel],
    workflows: &[Workflow],
    load: &EngineLoad<'_>,
) -> LoadedWorkflows {
    // Index workflows by workflow_id for fast lookup
    let mut workflow_map: HashMap<String, Vec<&Workflow>> = HashMap::new();
    for workflow in workflows {
        workflow_map
            .entry(workflow.workflow_id.clone())
            .or_default()
            .push(workflow);
    }

    let mut loaded = LoadedWorkflows::default();

    for channel in channels {
        let Some(ref wf_id) = channel.workflow_id else {
            loaded.issues.push(crate::channel::ChannelLoadIssue {
                channel: channel.name.clone(),
                reason: "channel has no workflow_id".to_string(),
            });
            continue;
        };

        let Some(wf_versions) = workflow_map.get(wf_id) else {
            loaded.issues.push(crate::channel::ChannelLoadIssue {
                channel: channel.name.clone(),
                reason: format!("workflow '{wf_id}' not found among active workflows"),
            });
            continue;
        };

        match load_channel(channel, wf_id, wf_versions, load) {
            Ok((v3, v4)) => {
                loaded.v3.extend(v3);
                loaded.v4.extend(v4);
            }
            Err(reason) => loaded.issues.push(crate::channel::ChannelLoadIssue {
                channel: channel.name.clone(),
                reason,
            }),
        }
    }

    loaded
}

/// What [`build_engine_workflows`] loads against: dataflow-rs 4's compiler
/// and dataflow-rs 3's screen, from one generation's functions.
pub struct EngineLoad<'a> {
    /// The generation's dataflow-rs 4 compiler: on its `Registry`, which
    /// holds Orion's functions, the plugin functions and the profile layer.
    pub compiler: &'a Compiler,
    /// The generation's function vocabulary, whose field rows name the
    /// aliases the loader rewrites.
    pub functions: &'a FunctionRegistry,
    /// The generation's plugins, which name the plugin a refused task's
    /// function belongs to and why it did not load.
    pub plugins: &'a PluginSet,
    /// The handlers dataflow-rs 3 dispatches, which screen what it serves.
    pub screen: &'a dyn HandlerScreen,
}

/// What [`build_engine_workflows`] produced.
#[derive(Default)]
pub struct LoadedWorkflows {
    /// The workflows dataflow-rs 3 serves.
    pub v3: Vec<dataflow_rs_v3::Workflow>,
    /// The same workflows, compiled on dataflow-rs 4, for its program.
    pub v4: Vec<CompiledWorkflow>,
    /// Every channel that did not load, and why.
    pub issues: Vec<crate::channel::ChannelLoadIssue>,
}

/// One channel's workflows on both engines, or its quarantine reason.
fn load_channel(
    channel: &Channel,
    wf_id: &str,
    wf_versions: &[&Workflow],
    load: &EngineLoad<'_>,
) -> Result<(Vec<dataflow_rs_v3::Workflow>, Vec<CompiledWorkflow>), String> {
    if wf_versions.len() == 1 && wf_versions[0].rollout_percentage == 100 {
        // Single version at 100% — convert normally
        let row = wf_versions[0];
        let json = row_json(
            row,
            &channel.name,
            &row.workflow_id,
            crate::storage::models::EntityStatus::Active.as_str(),
            None,
            load.functions,
        )
        .map_err(|e| format!("workflow '{wf_id}' failed to convert: {e}"))?;
        let compiled = compile_row(load.compiler, &json).map_err(|issues| {
            format!(
                "workflow '{wf_id}' failed to compile: {}",
                refusal(&issues, load.plugins, &json)
            )
        })?;
        log_findings(row, &compiled);
        let w = workflow_to_dataflow(row, &channel.name)
            .map_err(|e| format!("workflow '{wf_id}' failed to convert: {e}"))?;
        screen_workflow(&w, load.screen).map_err(|e| {
            let mut reason = format!("workflow '{wf_id}' has an unusable task: {e}");
            load.plugins.annotate(&mut reason);
            reason
        })?;
        return Ok((vec![w], vec![compiled.workflow]));
    }

    // Multiple versions or partial rollout — wrap with bucket ranges.
    let mut sorted: Vec<&Workflow> = wf_versions.to_vec();
    sorted.sort_by_key(|b| std::cmp::Reverse(b.version));

    // Newest first, because input order is traffic order: the version
    // being rolled out takes the low buckets and the one it replaces
    // keeps the rest.
    //
    // A percentage outside `0..=100` cannot come through the API — the
    // repository bounds it on write — but a hand-edited row can carry
    // one, and it must not wrap into something plausible. Saturating
    // at `u8::MAX` guarantees the sum overshoots and the channel is
    // quarantined with the over-100 message.
    let percentages: Vec<u8> = sorted
        .iter()
        .map(|wf| u8::try_from(wf.rollout_percentage).unwrap_or(u8::MAX))
        .collect();

    // F30: buckets are 0–99, and percentages that do not sum to 100
    // misroute silently — under 100, the remainder of the traffic
    // matches no workflow version at all; over 100, later versions'
    // ranges start past bucket 99 and are unreachable. Both used to be
    // an accumulator and an `!= 100` check here. `Rollout::partition`
    // is the engine's own arithmetic for exactly this, error direction
    // included, so the invariant is now stated once — in the crate
    // that routes on it.
    let ranges = dataflow_rs_v3::Rollout::partition(&percentages).map_err(|e| {
        format!(
            "rollout percentages for workflow '{wf_id}' do not partition the \
             bucket space: {e}"
        )
    })?;

    // All-or-nothing per channel, on each engine: a partially-converted
    // rollout would silently blackhole the failed version's bucket range.
    // dataflow-rs 4 compiles the whole set first, and its verdict decides;
    // dataflow-rs 3 converts and screens the set it accepted.
    let mut compiled = Vec::with_capacity(sorted.len());
    for (wf, rollout) in sorted.iter().zip(&ranges) {
        let failed = |e: String| {
            format!(
                "workflow '{}' v{} failed to compile: {e}",
                wf.workflow_id, wf.version
            )
        };
        let json = row_json(
            wf,
            &channel.name,
            &format!("{}:v{}", wf.workflow_id, wf.version),
            &wf.status,
            Some((rollout.bucket_start, rollout.bucket_end)),
            load.functions,
        )
        .map_err(|e| {
            format!(
                "workflow '{}' v{} failed to convert: {e}",
                wf.workflow_id, wf.version
            )
        })?;
        let version = compile_row(load.compiler, &json)
            .map_err(|issues| failed(refusal(&issues, load.plugins, &json)))?;
        log_findings(wf, &version);
        compiled.push(version.workflow);
    }

    let mut converted = Vec::with_capacity(sorted.len());
    for (wf, rollout) in sorted.iter().zip(ranges) {
        let w = workflow_to_dataflow_with_rollout(wf, &channel.name, rollout)
            .map_err(|e| e.to_string())
            .and_then(|w| screen_workflow(&w, load.screen).map(|()| w))
            .map_err(|e| {
                let mut reason = format!(
                    "workflow '{}' v{} failed to convert: {e}",
                    wf.workflow_id, wf.version
                );
                load.plugins.annotate(&mut reason);
                reason
            })?;
        converted.push(w);
    }
    Ok((converted, compiled))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The screen these tests convert against.
    ///
    /// A bare builder registers no custom handlers, which is all the fixtures
    /// below need — they are about rollout arithmetic and quarantine, and
    /// their tasks are `log`, a self-contained built-in every engine
    /// dispatches.
    fn screen() -> dataflow_rs_v3::engine::EngineBuilder {
        dataflow_rs_v3::Engine::builder()
    }

    /// Both engines with every Orion function, over inert dependencies: the
    /// dataflow-rs 4 `Registry` from `register_functions` and the
    /// dataflow-rs 3 screen from `build_custom_functions`, as a generation
    /// builds them.
    struct Engines {
        compiler: Compiler,
        screen: dataflow_rs_v3::engine::EngineBuilder,
        plugins: PluginSet,
        _offline: crate::engine::kit::OfflineDeps,
    }

    impl Engines {
        fn orion() -> Self {
            Self::orion_with(crate::engine::kit::test_exprs(), Vec::new())
        }

        /// Every Orion function on `exprs`, and each plugin function on
        /// both engines.
        fn orion_with(
            exprs: dataflow_rs::expr::ExprEngine,
            plugins: Vec<(
                crate::plugin::PluginFunctionHandler,
                crate::plugin::PluginFunction,
            )>,
        ) -> Self {
            let offline = crate::engine::kit::OfflineDeps::new();
            let mut registry = dataflow_rs::Registry::new(exprs);
            crate::engine::register_functions(&mut registry, offline.deps());
            let mut handlers = crate::engine::build_custom_functions(offline.deps());
            for (handler, function) in plugins {
                handlers.insert(handler.name().to_string(), Box::new(handler));
                crate::engine::register_plugin_functions(&mut registry, [function]);
            }
            Self {
                compiler: registry.compiler(dataflow_rs::program::CompileOptions::default()),
                screen: dataflow_rs_v3::Engine::builder().with_handlers(handlers),
                plugins: PluginSet::empty(),
                _offline: offline,
            }
        }

        /// The built-ins alone, which is all the `log` fixtures need.
        fn bare() -> Self {
            let registry = dataflow_rs::Registry::new(crate::engine::kit::test_exprs());
            Self {
                compiler: registry.compiler(dataflow_rs::program::CompileOptions::default()),
                screen: screen(),
                plugins: PluginSet::empty(),
                _offline: crate::engine::kit::OfflineDeps::new(),
            }
        }

        fn load(&self, channels: &[Channel], workflows: &[Workflow]) -> LoadedWorkflows {
            build_engine_workflows(
                channels,
                workflows,
                &EngineLoad {
                    compiler: &self.compiler,
                    functions: FunctionRegistry::builtin(),
                    plugins: &self.plugins,
                    screen: &self.screen,
                },
            )
        }
    }

    /// [`Engines::bare`]'s load, as `(dataflow-rs 3's workflows, issues)`,
    /// holding the two engines to one set of channels.
    fn load(
        channels: &[Channel],
        workflows: &[Workflow],
    ) -> (
        Vec<dataflow_rs_v3::Workflow>,
        Vec<crate::channel::ChannelLoadIssue>,
    ) {
        let loaded = Engines::bare().load(channels, workflows);
        assert_eq!(
            loaded.v3.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(),
            loaded.v4.iter().map(|w| w.id()).collect::<Vec<_>>(),
            "both engines load the same workflows"
        );
        (loaded.v3, loaded.issues)
    }

    /// `ESCAPED_TEMPLATE_KEY` must not quarantine a workflow.
    ///
    /// It is the one code `check_workflow` reports that `Engine::build` does
    /// not refuse, and this screen used to take the whole list — correct while
    /// every code was also a build refusal, wrong from dataflow-rs 3.9 on. It
    /// fires on *correct* code too: `$$set` is the documented fix, so treating
    /// it as fatal took down the channel of an author who had done the right
    /// thing.
    #[test]
    fn an_escaped_template_key_is_an_advisory_not_a_quarantine() {
        let workflow = dataflow_rs_v3::Workflow::from_json(
            r#"{"id":"w","name":"w","condition":true,"tasks":[
                {"id":"t","name":"t","function":{"name":"map","input":{"mappings":[
                  {"path":"data.a","logic":{"$set":{"x":1}}},
                  {"path":"data.b","logic":{"$$oid":"abc"}}]}}}]}"#,
        )
        .expect("the fixture is a valid workflow");

        // The engine does report both keys — this is not a claim that they are
        // invisible, only that they are not a reason to refuse to serve.
        assert_eq!(
            screen()
                .check_workflow(&workflow)
                .iter()
                .filter(|i| i.code == dataflow_rs_v3::IssueCode::EscapedTemplateKey)
                .count(),
            2
        );
        assert!(screen_workflow(&workflow, &screen()).is_ok());
    }

    /// The 3.10 pair, and the regression that upgrade would otherwise have been.
    ///
    /// Both are `Severity::Advisory` — reported by `check_workflow`, never
    /// refused by `build` — and both fire on shapes that are common in a real
    /// estate: a `validation` that collects errors and carries on is what the
    /// built-in is documented to do, and `continue_on_error` on a group is old
    /// enough that stored definitions carry it. Treating either as fatal
    /// quarantines working channels, which is what a hand-kept list did until
    /// it was edited: `dry-run` answered "workflow has an unusable task" for a
    /// workflow that runs perfectly.
    ///
    /// The severity table itself is upstream's to pin. What this asserts is
    /// Orion's half — that `screen_workflow` acts on it.
    #[test]
    fn the_advisory_issues_do_not_quarantine() {
        let cases = [
            (
                dataflow_rs_v3::IssueCode::UnguardedValidation,
                r#"{"id":"w","name":"w","condition":true,"tasks":[
                    {"id":"check","name":"check","function":{"name":"validation","input":{
                      "rules":[{"logic":{"==":[1,2]},"message":"no"}]}}},
                    {"id":"after","name":"after","function":{"name":"map","input":{
                      "mappings":[{"path":"data.x","logic":true}]}}}]}"#,
            ),
            (
                dataflow_rs_v3::IssueCode::GroupContinueOnError,
                r#"{"id":"w","name":"w","condition":true,"tasks":[
                    {"id":"g","name":"g","continue_on_error":true,"tasks":[
                      {"id":"t","name":"t","function":{"name":"log","input":{
                        "message":"x"}}}]}]}"#,
            ),
        ];

        for (code, json) in cases {
            let workflow =
                dataflow_rs_v3::Workflow::from_json(json).expect("the fixture is a valid workflow");
            // The engine reports it — so this is a claim about severity, not
            // about the finding being absent.
            assert!(
                screen()
                    .check_workflow(&workflow)
                    .iter()
                    .any(|i| i.code == code),
                "{code:?} must be reported by check_workflow"
            );
            assert_eq!(
                code.severity(),
                dataflow_rs_v3::Severity::Advisory,
                "{code:?} must be advisory"
            );
            assert!(
                screen_workflow(&workflow, &screen()).is_ok(),
                "{code:?} must not quarantine"
            );
        }
    }

    /// `check_workflow` does not run `Workflow::validate()`; `build` does, and
    /// fails the whole engine on the first refusal. The screen must catch what
    /// `validate()` refuses, or one row takes every channel down.
    #[test]
    fn a_workflow_validate_refuses_is_screened() {
        let workflow = dataflow_rs_v3::Workflow::from_json(
            r#"{"id":"w","name":"w","condition":true,
                "loop":{"max":2,"setup":[
                  {"id":"t","name":"s","function":{"name":"log","input":{"message":"x"}}}]},
                "tasks":[
                  {"id":"t","name":"t","function":{"name":"log","input":{"message":"x"}}}]}"#,
        )
        .expect("a duplicate id still parses");

        assert!(
            screen().check_workflow(&workflow).is_empty(),
            "if check_workflow starts reporting this, the validate() call can go"
        );
        let err = screen_workflow(&workflow, &screen()).expect_err("must be screened");
        assert!(err.contains("Duplicate"), "{err}");
    }

    #[test]
    fn test_glob_match_exact() {
        assert!(glob_match("orders", "orders"));
        assert!(!glob_match("orders", "events"));
    }

    #[test]
    fn test_glob_match_prefix_wildcard() {
        assert!(glob_match("internal-*", "internal-debug"));
        assert!(glob_match("internal-*", "internal-"));
        assert!(!glob_match("internal-*", "external-debug"));
    }

    #[test]
    fn test_glob_match_suffix_wildcard() {
        assert!(glob_match("*-debug", "internal-debug"));
        assert!(!glob_match("*-debug", "internal-prod"));
    }

    #[test]
    fn test_glob_match_star_only() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn test_glob_match_middle_wildcard() {
        assert!(glob_match("pre*suf", "presuf"));
        assert!(glob_match("pre*suf", "pre-middle-suf"));
        assert!(!glob_match("pre*suf", "pre-middle"));
    }

    /// F32: cases the old successive-`find` matcher got wrong.
    #[test]
    fn test_glob_match_backtracking() {
        // The old matcher bound `b` to the first occurrence and failed.
        assert!(glob_match("a*bc", "abxbc"));
        assert!(glob_match("a*bc", "abcbc"));
        assert!(!glob_match("a*bc", "abxbd"));
    }

    #[test]
    fn test_glob_match_multi_star() {
        assert!(glob_match("a*b*c", "a-x-b-y-c"));
        assert!(glob_match("*orders*", "internal-orders-debug"));
        assert!(!glob_match("a*b*c", "a-x-c-y-b"));
        assert!(glob_match("**", "anything"));
    }

    fn make_channel(name: &str) -> Channel {
        Channel {
            tags_json: "[]".to_string(),
            channel_id: name.to_string(),
            name: name.to_string(),
            version: 1,
            status: crate::storage::models::EntityStatus::Active
                .as_str()
                .to_string(),
            channel_type: "sync".to_string(),
            protocol: crate::storage::models::ChannelProtocol::Http
                .as_str()
                .to_string(),
            methods_json: Some("POST".to_string()),
            workflow_id: None,
            topic: None,
            consumer_group: None,
            route_pattern: None,
            description: None,
            transport_config_json: "{}".to_string(),
            config_json: "{}".to_string(),
            priority: 0,
            created_at: chrono::NaiveDateTime::default(),
            updated_at: chrono::NaiveDateTime::default(),
        }
    }

    fn make_workflow(wf_id: &str, version: i64, rollout: i64) -> Workflow {
        Workflow {
            workflow_id: wf_id.to_string(),
            version,
            name: format!("{wf_id}-v{version}"),
            description: None,
            priority: 0,
            status: "active".to_string(),
            rollout_percentage: rollout,
            condition_json: "true".to_string(),
            tasks_json:
                r#"[{"id":"t1","name":"log","function":{"name":"log","input":{"message":"x"}}}]"#
                    .to_string(),
            tags_json: "[]".to_string(),
            loop_json: None,
            continue_on_error: false,
            created_at: chrono::NaiveDateTime::default(),
            updated_at: chrono::NaiveDateTime::default(),
        }
    }

    /// F30: rollout percentages that don't sum to 100 quarantine the channel
    /// instead of silently blackholing (or shadowing) part of the traffic.
    #[test]
    fn test_rollout_sum_must_be_100() {
        let mut channel = make_channel("rollout-ch");
        channel.workflow_id = Some("wf".to_string());

        // 50 + 30 = 80 — buckets 80–99 would match no version.
        let wfs = vec![make_workflow("wf", 1, 30), make_workflow("wf", 2, 50)];
        let (converted, issues) = load(&[channel.clone()], &wfs);
        assert!(converted.is_empty(), "under-100 rollout must not half-load");
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].reason.contains("sum to 80"),
            "{}",
            issues[0].reason
        );

        // 60 + 60 = 120 — later versions unreachable.
        let wfs = vec![make_workflow("wf", 1, 60), make_workflow("wf", 2, 60)];
        let (converted, issues) = load(&[channel.clone()], &wfs);
        assert!(converted.is_empty());
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].reason.contains("sum to 120"),
            "{}",
            issues[0].reason
        );

        // 50 + 50 = 100 — loads both versions cleanly.
        let wfs = vec![make_workflow("wf", 1, 50), make_workflow("wf", 2, 50)];
        let (converted, issues) = load(&[channel], &wfs);
        assert_eq!(converted.len(), 2);
        assert!(issues.is_empty(), "{issues:?}");
    }

    /// The `[min, max)` rollout bucket range a converted workflow serves.
    ///
    /// This used to parse the range back out of a synthetic condition
    /// (`and[1]` is `>= min`, `and[2]` is `< max`) — which is why the split
    /// could be inert for as long as it was: the assertion only ever checked
    /// the *shape* of the generated JSON, never that a bucket routed anywhere.
    fn bucket_range(wf: &dataflow_rs_v3::Workflow) -> (u8, u8) {
        let rollout = wf
            .rollout
            .expect("converted rollout workflow carries a range");
        (rollout.bucket_start, rollout.bucket_end)
    }

    /// The bucket ranges must partition 0–99 contiguously, newest version
    /// first: 20/50/30 across v3/v2/v1 → v3 [0,20), v2 [20,70), v1 [70,100).
    /// A gap or overlap in the offsets silently misroutes traffic, which is
    /// exactly what this accumulation arithmetic exists to prevent.
    #[test]
    fn test_rollout_bucket_offsets_partition_newest_first() {
        let mut channel = make_channel("rollout-ch");
        channel.workflow_id = Some("wf".to_string());
        let wfs = vec![
            make_workflow("wf", 1, 30),
            make_workflow("wf", 2, 50),
            make_workflow("wf", 3, 20),
        ];
        let (converted, issues) = load(&[channel], &wfs);
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(converted.len(), 3);

        let by_id: std::collections::HashMap<String, (u8, u8)> = converted
            .iter()
            .map(|w| (w.id.clone(), bucket_range(w)))
            .collect();
        assert_eq!(
            by_id["wf:v3"],
            (0, 20),
            "newest version gets the first bucket"
        );
        assert_eq!(by_id["wf:v2"], (20, 70));
        assert_eq!(by_id["wf:v1"], (70, 100));

        // …and the ranges actually route. Every bucket 0–99 must be served by
        // exactly one version. The previous wiring passed the shape assertion
        // above while sending 100% of traffic to whichever version started at
        // 0, because the condition's `var` addressed the context root and the
        // bucket was injected under `data`.
        for bucket in 0u8..100 {
            let serving: Vec<&str> = converted
                .iter()
                .filter(|w| w.rollout.is_some_and(|r| r.accepts(bucket)))
                .map(|w| w.id.as_str())
                .collect();
            assert_eq!(
                serving.len(),
                1,
                "bucket {bucket} is served by {serving:?}, not exactly one version"
            );
        }
    }

    /// F33: a channel with no workflow_id, and one pointing at a workflow
    /// that is not in the active set, must each surface a load issue naming
    /// the problem rather than being silently skipped.
    #[test]
    fn test_missing_and_unknown_workflow_are_reported_as_issues() {
        let no_wf = make_channel("no-wf");
        let mut unknown = make_channel("unknown-wf");
        unknown.workflow_id = Some("ghost".to_string());

        let (converted, issues) = load(&[no_wf, unknown], &[]);
        assert!(converted.is_empty());
        assert_eq!(issues.len(), 2);
        assert!(
            issues[0].reason.contains("no workflow_id"),
            "{}",
            issues[0].reason
        );
        assert!(
            issues[1].reason.contains("'ghost' not found"),
            "{}",
            issues[1].reason
        );
    }

    /// All-or-nothing per channel: when one version of a rollout fails to
    /// convert, the versions that DID convert must not load — a partial
    /// rollout would silently blackhole the failed version's bucket range.
    #[test]
    fn test_partial_rollout_conversion_failure_loads_nothing() {
        let mut channel = make_channel("rollout-ch");
        channel.workflow_id = Some("wf".to_string());

        // v2 (processed first, newest) converts fine; v1 has broken tasks.
        let mut bad_v1 = make_workflow("wf", 1, 50);
        bad_v1.tasks_json = "not json".to_string();
        let wfs = vec![bad_v1, make_workflow("wf", 2, 50)];

        let (converted, issues) = load(&[channel], &wfs);
        assert!(
            converted.is_empty(),
            "the successfully-converted v2 must be discarded with v1"
        );
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].reason.contains("v1 failed to convert"),
            "{}",
            issues[0].reason
        );
    }

    #[test]
    fn test_filter_channels_no_config() {
        let channels = vec![make_channel("orders"), make_channel("events")];
        let config = crate::config::ChannelFilterConfig::default();
        let filtered = filter_channels(channels, &config);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_filter_channels_include_only() {
        let channels = vec![
            make_channel("orders"),
            make_channel("events"),
            make_channel("internal-debug"),
        ];
        let config = crate::config::ChannelFilterConfig {
            include: vec!["orders".to_string(), "events".to_string()],
            exclude: vec![],
        };
        let filtered = filter_channels(channels, &config);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|c| c.name != "internal-debug"));
    }

    #[test]
    fn test_filter_channels_exclude_only() {
        let channels = vec![
            make_channel("orders"),
            make_channel("events"),
            make_channel("internal-debug"),
        ];
        let config = crate::config::ChannelFilterConfig {
            include: vec![],
            exclude: vec!["internal-*".to_string()],
        };
        let filtered = filter_channels(channels, &config);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|c| c.name != "internal-debug"));
    }

    #[test]
    fn test_filter_channels_include_and_exclude() {
        let channels = vec![
            make_channel("orders"),
            make_channel("orders-debug"),
            make_channel("events"),
        ];
        let config = crate::config::ChannelFilterConfig {
            include: vec!["orders*".to_string()],
            exclude: vec!["*-debug".to_string()],
        };
        let filtered = filter_channels(channels, &config);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "orders");
    }

    // -- F41: a malformed task input quarantines its channel, not the engine --

    fn workflow_with_raw_tasks(wf_id: &str, tasks_json: &str) -> Workflow {
        let mut wf = make_workflow(wf_id, 1, 100);
        wf.tasks_json = tasks_json.to_string();
        wf
    }

    #[test]
    fn unregistered_custom_function_quarantines_only_its_channel() {
        // A task naming a function that is neither a dataflow-rs builtin nor a
        // registered Orion handler lands in FunctionConfig::Custom, where
        // precompile_custom_inputs fails with FunctionNotFound — aborting the
        // *entire* engine build. Before F41 that meant boot aborted, or a reload
        // took down every channel on every node, because of one stored row.
        let mut good = make_channel("good");
        good.workflow_id = Some("wf-good".to_string());
        let mut bad = make_channel("bad");
        bad.workflow_id = Some("wf-bad".to_string());

        let wfs = vec![
            make_workflow("wf-good", 1, 100),
            workflow_with_raw_tasks(
                "wf-bad",
                r#"[{"id":"t1","name":"oops","function":{"name":"totally_not_a_function",
                   "input":{}}}]"#,
            ),
        ];

        let (converted, issues) = load(&[good, bad], &wfs);

        assert_eq!(
            converted.len(),
            1,
            "the healthy channel must still be built"
        );
        assert_eq!(converted[0].channel, "good");
        assert_eq!(issues.len(), 1, "issues = {issues:?}");
        assert_eq!(issues[0].channel, "bad");
        // dataflow-rs 4's verdict decides, so the reason is its issue's code
        // and path (I182), where it was dataflow-rs 3's `FunctionNotFound`.
        assert!(
            issues[0].reason.contains("`totally_not_a_function`")
                && issues[0]
                    .reason
                    .contains("task 't1': UNKNOWN_FUNCTION at tasks[0].function.name"),
            "reason should name the offending task and function: {}",
            issues[0].reason
        );
    }

    // ---- dataflow-rs 4 decides which rows load (I182) ---------------------

    /// I186: every alias of `schema.rs`'s rows (and `channel_call`'s
    /// `response_path`, 1.12's serde alias the row does not declare) is
    /// rewritten to its canonical key, in a task, in a group and in
    /// `loop.setup`, in its place in the input, and each is reported. A task
    /// holding both spellings is left as written, for its config to refuse.
    #[test]
    fn rewrite_aliases_rewrites_every_alias_in_tasks_groups_and_setup() {
        let mut workflow = serde_json::json!({
            "id": "w",
            "tasks": [
                {"id": "call", "function": {"name": "http_call", "input": {
                    "connector": "api", "path_logic": {"var": "data.p"},
                    "body_logic": {"a": 1}, "response_path": "data.r", "timeout_ms": 5}}},
                {"id": "g", "tasks": [
                    {"id": "pub", "function": {"name": "publish_kafka", "input": {
                        "connector": "k", "topic": "t",
                        "key_logic": {"var": "data.k"}, "value_logic": {"var": "data"}}}},
                    {"id": "both", "function": {"name": "http_call", "input": {
                        "connector": "api", "output": "data.a", "response_path": "data.b"}}}
                ]},
                {"id": "plain", "function": {"name": "log", "input": {"message": "x"}}}
            ],
            "loop": {"max": 1, "setup": [
                {"id": "fan", "function": {"name": "channel_call", "input": {
                    "channel_logic": "target", "data_logic": {"x": 1},
                    "response_path": "data.child"}}}
            ]}
        });

        let rewrites = rewrite_aliases(&mut workflow, FunctionRegistry::builtin());

        let seen: Vec<(&str, &str, &str)> = rewrites
            .iter()
            .map(|r| (r.task.as_str(), r.alias.as_str(), r.canonical.as_str()))
            .collect();
        assert_eq!(
            seen,
            [
                ("tasks[0]", "path_logic", "path"),
                ("tasks[0]", "body_logic", "body"),
                ("tasks[0]", "response_path", "output"),
                ("tasks[1].tasks[0]", "key_logic", "key"),
                ("tasks[1].tasks[0]", "value_logic", "value"),
                ("loop.setup[0]", "channel_logic", "channel"),
                ("loop.setup[0]", "data_logic", "data"),
                ("loop.setup[0]", "response_path", "output"),
            ]
        );
        assert_eq!(rewrites[0].task_id.as_deref(), Some("call"));
        assert_eq!(rewrites[0].function, "http_call");

        // Each key keeps its place in the input.
        let keys = |at: &str| -> Vec<String> {
            workflow
                .pointer(at)
                .and_then(Value::as_object)
                .expect("an input")
                .keys()
                .cloned()
                .collect()
        };
        assert_eq!(
            keys("/tasks/0/function/input"),
            ["connector", "path", "body", "output", "timeout_ms"]
        );
        assert_eq!(
            keys("/loop/setup/0/function/input"),
            ["channel", "data", "output"]
        );
        assert_eq!(
            workflow.pointer("/tasks/0/function/input/path"),
            Some(&serde_json::json!({"var": "data.p"}))
        );
        // Both spellings at once: left as written.
        assert_eq!(
            keys("/tasks/1/tasks/1/function/input"),
            ["connector", "output", "response_path"]
        );
    }

    /// A rollout set is all or nothing on dataflow-rs 4 too: a version it
    /// refuses drops the whole set, though dataflow-rs 3 would have taken
    /// every version. The refusal is break 2a: `cache_write`'s `output`,
    /// which 1.12 resolved and ignored, is an unknown key of its config.
    #[tokio::test]
    async fn a_rollout_set_one_of_whose_versions_is_rejected_is_dropped_whole() {
        let mut channel = make_channel("rollout-ch");
        channel.workflow_id = Some("wf".to_string());
        let mut v1 = make_workflow("wf", 1, 50);
        v1.tasks_json = r#"[{"id":"t1","name":"put","function":{"name":"cache_write","input":{
            "connector":"c","key":"k","value":{"a":1},"output":"data.x"}}}]"#
            .to_string();
        let wfs = vec![v1, make_workflow("wf", 2, 50)];

        let engines = Engines::orion();
        let loaded = engines.load(std::slice::from_ref(&channel), &wfs);
        assert!(loaded.v3.is_empty() && loaded.v4.is_empty());
        assert_eq!(loaded.issues.len(), 1, "{:?}", loaded.issues);
        let reason = &loaded.issues[0].reason;
        assert!(
            reason.starts_with(
                "workflow 'wf' v1 failed to compile: task 't1': UNKNOWN_KEY at \
                 tasks[0].function.input.output"
            ),
            "{reason}"
        );

        // The same set without the stray key loads whole, on both engines.
        let mut fixed = wfs.clone();
        fixed[0].tasks_json = fixed[0].tasks_json.replace(r#","output":"data.x""#, "");
        let loaded = engines.load(&[channel], &fixed);
        assert!(loaded.issues.is_empty(), "{:?}", loaded.issues);
        assert_eq!(loaded.v3.len(), 2);
        assert_eq!(
            loaded.v4.iter().map(|w| w.id()).collect::<Vec<_>>(),
            ["wf:v2", "wf:v1"]
        );
    }

    /// I185 and I182: break 21's root-output Defect quarantines, its code and
    /// path in the reason, where dataflow-rs 3 loads the row (and replaced
    /// `data` with the rows on every call). A `PAYLOAD_VAR` Defect does not:
    /// that row loads on both engines and the compile keeps the Defect for
    /// the log.
    #[tokio::test]
    async fn a_root_output_defect_quarantines_its_channel_and_a_payload_var_does_not() {
        let engines = Engines::orion();
        let mut rows = make_channel("rows");
        rows.workflow_id = Some("wf-rows".to_string());
        let mut payload = make_channel("payload");
        payload.workflow_id = Some("wf-payload".to_string());
        let wfs = vec![
            workflow_with_raw_tasks(
                "wf-rows",
                r#"[{"id":"t1","name":"read","function":{"name":"db_read","input":{
                    "connector":"db","query":"SELECT 1","output":"data"}}}]"#,
            ),
            workflow_with_raw_tasks(
                "wf-payload",
                r#"[{"id":"t1","name":"m","function":{"name":"map","input":{"mappings":[
                    {"path":"data.x","logic":{"var":"payload.x"}}]}}}]"#,
            ),
        ];
        for wf in &wfs {
            let w = workflow_to_dataflow(wf, "c").expect("converts");
            assert!(
                screen_workflow(&w, &engines.screen).is_ok(),
                "dataflow-rs 3 loads {}",
                wf.workflow_id
            );
        }

        let loaded = engines.load(&[rows, payload], &wfs);
        assert_eq!(loaded.issues.len(), 1, "{:?}", loaded.issues);
        assert_eq!(loaded.issues[0].channel, "rows");
        assert!(
            loaded.issues[0].reason.starts_with(
                "workflow 'wf-rows' failed to compile: task 't1': INPUT_PARSE at \
                 tasks[0].function.input.output: `db_read` writes"
            ),
            "{}",
            loaded.issues[0].reason
        );
        assert_eq!(loaded.v3.len(), 1);
        assert_eq!(loaded.v3[0].channel, "payload");

        let json = workflow_json(&wfs[1], "payload", "wf-payload", "active", None).expect("json");
        let compiled = compile_row(&engines.compiler, &json).expect("loads");
        assert!(
            compiled
                .issues
                .iter()
                .any(|i| i.code == IssueCode::PayloadVar && i.severity == Severity::Defect),
            "{:?}",
            compiled.issues
        );
    }

    /// Break 2: a near miss of a task key is a `LIKELY_TYPO` Defect, and it
    /// quarantines, where dataflow-rs 3 ignored the key and ran the task
    /// unconditionally. A stray key that resembles none is an Advisory and
    /// loads.
    #[test]
    fn a_likely_typo_quarantines_and_an_unknown_key_loads() {
        let mut typo = make_channel("typo");
        typo.workflow_id = Some("wf-typo".to_string());
        let mut stray = make_channel("stray");
        stray.workflow_id = Some("wf-stray".to_string());
        let wfs = vec![
            workflow_with_raw_tasks(
                "wf-typo",
                r#"[{"id":"t1","name":"log","conditon":false,
                    "function":{"name":"log","input":{"message":"x"}}}]"#,
            ),
            workflow_with_raw_tasks(
                "wf-stray",
                r#"[{"id":"t1","name":"log","owner":"ops",
                    "function":{"name":"log","input":{"message":"x"}}}]"#,
            ),
        ];

        let w = workflow_to_dataflow(&wfs[0], "typo").expect("converts");
        assert!(
            screen_workflow(&w, &screen()).is_ok(),
            "dataflow-rs 3 loads the typo, ignoring the key"
        );

        let (converted, issues) = load(&[typo, stray], &wfs);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].channel, "typo");
        assert!(
            issues[0]
                .reason
                .contains("task 't1': LIKELY_TYPO at tasks[0].conditon"),
            "{}",
            issues[0].reason
        );
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].channel, "stray");
    }

    /// The rule itself, one severity and code at a time.
    #[test]
    fn quarantines_takes_rejected_issues_and_two_defects_only() {
        let at = dataflow_rs::program::AuthoredPath::root();
        let issue = |code, severity| Issue::new(code, at.clone(), "x").with_severity(severity);
        assert!(quarantines(&issue(
            IssueCode::UnknownKey,
            Severity::Rejected
        )));
        assert!(quarantines(&issue(IssueCode::InputParse, Severity::Defect)));
        assert!(quarantines(&issue(IssueCode::LikelyTypo, Severity::Defect)));
        assert!(!quarantines(&issue(
            IssueCode::PayloadVar,
            Severity::Defect
        )));
        assert!(!quarantines(&issue(
            IssueCode::UnresolvedReference,
            Severity::Defect
        )));
        assert!(!quarantines(&issue(
            IssueCode::InputParse,
            Severity::Advisory
        )));
        assert!(!quarantines(&issue(
            IssueCode::V3Spelling,
            Severity::Advisory
        )));
    }

    // ---- The named load breaks, each a quarantine (CHANGELOG, Unreleased) --

    /// One named load break: a task's input dataflow-rs 4 refuses at load,
    /// and the issue the channel's quarantine reason names for it.
    struct Break {
        /// The break, and the task whose report names it.
        cite: &'static str,
        function: &'static str,
        input: Value,
        /// The issue's code.
        code: &'static str,
        /// Its path below the task's `function.input`.
        at: &'static str,
    }

    fn row(
        cite: &'static str,
        function: &'static str,
        input: Value,
        code: &'static str,
        at: &'static str,
    ) -> Break {
        Break {
            cite,
            function,
            input,
            code,
            at,
        }
    }

    /// The quarantine reason of a channel whose one workflow is one task
    /// calling `function` with `input`, or `None` when it loads.
    fn reason_for(engines: &Engines, function: &str, input: &Value) -> Option<String> {
        let mut channel = make_channel("breaks");
        channel.workflow_id = Some("wf-breaks".to_string());
        let task = serde_json::json!([
            {"id": "t1", "name": "t1", "function": {"name": function, "input": input}}
        ]);
        let wf = workflow_with_raw_tasks("wf-breaks", &task.to_string());
        let loaded = engines.load(&[channel], &[wf]);
        loaded.issues.first().map(|issue| issue.reason.clone())
    }

    /// Holds each row to its quarantine, reporting every miss at once.
    fn assert_quarantined(engines: &Engines, rows: &[Break]) {
        let mut misses = Vec::new();
        for row in rows {
            let expected = format!("{} at tasks[0].function.input{}", row.code, row.at);
            let reason = reason_for(engines, row.function, &row.input);
            if std::env::var_os("ORION_PRINT_BREAKS").is_some() {
                eprintln!("{}: {reason:?}", row.cite);
            }
            match reason {
                Some(reason) if reason.contains(&expected) => {}
                other => misses.push(format!(
                    "{} ({} {}): expected `{expected}`, got {other:?}",
                    row.cite, row.function, row.input
                )),
            }
        }
        assert!(misses.is_empty(), "{}", misses.join("\n"));
    }

    /// `base` with each key of `extra` set over it.
    fn over(mut base: Value, extra: Value) -> Value {
        let Value::Object(extra) = extra else {
            unreachable!("the overrides are an object")
        };
        base.as_object_mut()
            .expect("an input is an object")
            .extend(extra);
        base
    }

    /// Orion's expressions with the secret `s` declared, so a row's
    /// `{"secret": "s"}` is refused for where it sits and not as unknown,
    /// and `{"secret": "nope"}` is the undeclared one.
    fn exprs_with_a_secret() -> dataflow_rs::expr::ExprEngine {
        let mut values = serde_json::Map::new();
        values.insert("s".to_string(), Value::String("v".to_string()));
        crate::engine::operators::orion_exprs(
            &crate::engine::ResolvedSecrets::from_values(values),
            0,
        )
        .expect("Orion's expressions")
    }

    /// Every load refusal the cutover brings for Orion's own functions, each
    /// quarantining its channel with its code and path in the reason: 1.12
    /// failed every call on each, or refused it when the workflow was
    /// written, and loaded it (or for break 2a and break 21, ran it). Each
    /// row cites the break and the task that ported the field.
    #[tokio::test]
    async fn every_named_load_break_quarantines_its_channel() {
        use serde_json::json;
        let engines = Engines::orion_with(exprs_with_a_secret(), Vec::new());
        let smtp = |extra| {
            over(
                json!({"connector": "smtp", "to": "a@b.c", "subject": "s"}),
                extra,
            )
        };
        let mongo = |extra| {
            over(
                json!({"connector": "m", "database": "d", "collection": "c"}),
                extra,
            )
        };
        let hmac = |extra| {
            over(
                json!({"op": "hmac", "algorithm": "sha256", "data": "x", "key": "k",
                    "output": "data.d"}),
                extra,
            )
        };
        let verify = |extra| {
            over(
                json!({"token": "t", "algorithms": ["HS256"],
                    "keys": [{"algorithm": "HS256", "key": "k"}]}),
                extra,
            )
        };
        let sign = |extra| {
            over(
                json!({"algorithm": "HS256", "key": "k", "expires_in": "5m",
                    "output": "data.token"}),
                extra,
            )
        };
        let infer = |extra| over(json!({"model": "m", "input": {"x": 1}}), extra);
        #[rustfmt::skip]
        let rows = [
            // I183: a field schema.rs marks required, missing.
            row("I183 (O3)", "http_call", json!({}), "INPUT_PARSE", ".connector"),
            row("I183 (O3)", "send_email", json!({"connector": "smtp", "subject": "s"}), "INPUT_PARSE", ".to"),
            row("I183 (O3)", "send_email", json!({"connector": "smtp", "to": "a@b.c"}), "INPUT_PARSE", ".subject"),
            row("I183 (O3)", "storage_presign", json!({"connector": "s3", "expires_in": 60}), "INPUT_PARSE", ".key"),
            row("I183 (O3)", "storage_presign", json!({"connector": "s3", "key": "k"}), "INPUT_PARSE", ".expires_in"),
            row("I183 (O3)", "storage_head", json!({"connector": "s3"}), "INPUT_PARSE", ".key"),
            row("I183 (O3)", "publish_kafka", json!({"connector": "k"}), "INPUT_PARSE", ".topic"),
            row("I183 (O4)", "db_read", json!({"connector": "db"}), "INPUT_PARSE", ".query"),
            row("I183 (O4)", "db_write", json!({"connector": "db"}), "INPUT_PARSE", ".query"),
            row("I183 (O4)", "data_query", json!({"connector": "db"}), "INPUT_PARSE", ".query"),
            row("I183 (O4)", "data_write", json!({"connector": "db"}), "INPUT_PARSE", ".write"),
            row("I183 (O5)", "mongo_read", json!({"connector": "m", "database": "d"}), "INPUT_PARSE", ".collection"),
            row("I183 (O5)", "mongo_write", mongo(json!({})), "INPUT_PARSE", ".op"),
            row("I183 (O5)", "mongo_aggregate", mongo(json!({})), "INPUT_PARSE", ".pipeline"),
            row("I183 (O6)", "cache_read", json!({"key": "k"}), "INPUT_PARSE", ".connector"),
            row("I183 (O6)", "cache_write", json!({"connector": "c", "value": 1}), "INPUT_PARSE", ".key"),
            row("I183 (O6)", "cache_write", json!({"connector": "c", "key": "k"}), "INPUT_PARSE", ".value"),
            row("I183 (O6)", "cache_delete", json!({"connector": "c"}), "INPUT_PARSE", ".keys"),
            row("I183 (O6)", "cache_incr", json!({"connector": "c"}), "INPUT_PARSE", ".key"),
            row("I183 (O6)", "cache_invalidate", json!({}), "INPUT_PARSE", ".namespaces"),
            row("I183 (O7)", "crypto", json!({"algorithm": "sha256", "data": "x"}), "INPUT_PARSE", ".op"),
            row("I183 (O7)", "jwt_sign", json!({"key": "k", "expires_in": "5m"}), "INPUT_PARSE", ".algorithm"),
            row("I183 (O7)", "jwt_sign", json!({"algorithm": "HS256", "expires_in": "5m"}), "INPUT_PARSE", ".key"),
            row("I183 (O7)", "jwt_verify", json!({"algorithms": ["HS256"], "jwks_url": "https://x"}), "INPUT_PARSE", ".token"),
            row("I183 (O7)", "jwt_verify", json!({"token": "t", "jwks_url": "https://x"}), "INPUT_PARSE", ".algorithms"),
            row("I183 (O8)", "model_infer", json!({"input": {}}), "INPUT_PARSE", ".model"),
            row("I183 (O8)", "model_infer", json!({"model": "m"}), "INPUT_PARSE", ".input"),
            row("I183 (O10)", "channel_call", json!({}), "INPUT_PARSE", ".channel"),
            // O2: key material read by the engine's secret checks.
            row("O2, KeyMaterial", "crypto", hmac(json!({"key": {"secret": "nope"}})), "UNKNOWN_SECRET", ".key"),
            row("O2, KeyMaterialList", "jwt_verify", verify(json!({"issuer": {"secret": "nope"}})), "UNKNOWN_SECRET", ".issuer"),
            row("O2, a nested secret in a key-list object", "jwt_verify", verify(json!({"audience": [{"a": {"secret": "s"}}]})), "SECRET_IN_DOCUMENT", ".audience"),
            // O3.
            row("O3, non-object headers", "send_email", smtp(json!({"headers": "x"})), "INPUT_PARSE", ".headers"),
            row("O3, a secret in to", "send_email", smtp(json!({"to": {"secret": "s"}})), "SECRET_IN_DOCUMENT", ".to"),
            row("O3, a secret in cc", "send_email", smtp(json!({"cc": [{"secret": "s"}]})), "SECRET_IN_DOCUMENT", ".cc"),
            row("O3, a secret in bcc", "send_email", smtp(json!({"bcc": {"secret": "s"}})), "SECRET_IN_DOCUMENT", ".bcc"),
            row("O3, an output that is not a path", "storage_head", json!({"connector": "s3", "key": "k", "output": "result"}), "INVALID_PATH", ".output"),
            row("O3, an output that is not a path", "http_call", json!({"connector": "api", "output": "response"}), "INVALID_PATH", ".output"),
            // O4.
            row("O4, a non-string connector", "db_read", json!({"connector": 5, "query": "SELECT 1", "output": "data.r"}), "INPUT_PARSE", ".connector"),
            row("O4, a non-string query", "db_read", json!({"connector": "db", "query": 5, "output": "data.r"}), "INPUT_PARSE", ".query"),
            row("O4, a non-string query", "db_write", json!({"connector": "db", "query": 5}), "INPUT_PARSE", ".query"),
            row("O4, a secret in params", "db_read", json!({"connector": "db", "query": "q", "output": "data.r", "params": [{"secret": "s"}]}), "SECRET_IN_DOCUMENT", ".params"),
            row("O4, a secret in numeric_as", "db_read", json!({"connector": "db", "query": "q", "output": "data.r", "numeric_as": {"secret": "s"}}), "SECRET_IN_DOCUMENT", ".numeric_as"),
            row("O4, a secret in binary_as", "db_read", json!({"connector": "db", "query": "q", "output": "data.r", "binary_as": {"secret": "s"}}), "SECRET_IN_DOCUMENT", ".binary_as"),
            row("O4, a malformed var hole", "db_read", json!({"connector": "db", "query": "q", "output": "data.r", "params": [{"var": 5}]}), "INVALID_PATH", ".params"),
            row("O4, a malformed var hole", "db_write", json!({"connector": "db", "query": "q", "params": [{"var": []}]}), "INVALID_PATH", ".params"),
            row("O4, a malformed var hole", "db_write", json!({"connector": "db", "query": "q", "params": [{"var": ["a", 1, 2]}]}), "INVALID_PATH", ".params"),
            // A literal hole rooted at no context root; one rooted at `payload` is
            // a `PAYLOAD_VAR` Defect, which loads.
            row("O4, a malformed var hole", "db_write", json!({"connector": "db", "query": "q", "params": [{"var": "order.id"}]}), "INVALID_PATH", ".params"),
            // O5.
            row("O5, an unknown op", "mongo_write", mongo(json!({"op": "frobnicate", "document": {}})), "INPUT_PARSE", ".op"),
            row("O5, a document missing for the op", "mongo_write", mongo(json!({"op": "insert_one"})), "INPUT_PARSE", ".document"),
            row("O5, a filtered op without a filter", "mongo_write", mongo(json!({"op": "delete_many"})), "INPUT_PARSE", ".filter"),
            row("O5, a field the op does not take", "mongo_write", mongo(json!({"op": "insert_one", "document": {"a": 1}, "update": {"$set": {"a": 1}}})), "INPUT_PARSE", ".update"),
            row("O5, an empty update", "mongo_write", mongo(json!({"op": "update_one", "filter": {"a": 1}, "update": {}})), "INPUT_PARSE", ".update"),
            row("O5, a plain-key update", "mongo_write", mongo(json!({"op": "update_one", "filter": {"a": 1}, "update": {"a": 2}})), "INPUT_PARSE", ".update"),
            row("O5, an operator key in a replacement", "mongo_write", mongo(json!({"op": "replace_one", "filter": {"a": 1}, "document": {"$set": {"a": 2}}})), "INPUT_PARSE", ".document"),
            row("O5, a pipeline that is not an array", "mongo_aggregate", mongo(json!({"pipeline": {"$match": {}}})), "INPUT_PARSE", ".pipeline"),
            row("O5, an empty pipeline", "mongo_aggregate", mongo(json!({"pipeline": []})), "INPUT_PARSE", ".pipeline"),
            row("O5, a stage that is not one $ key", "mongo_aggregate", mongo(json!({"pipeline": [{"match": {}}]})), "INPUT_PARSE", ".pipeline"),
            row("O5, an unknown stage", "mongo_aggregate", mongo(json!({"pipeline": [{"$frobnicate": {}}]})), "INPUT_PARSE", ".pipeline"),
            row("O5, a non-string database", "mongo_read", json!({"connector": "m", "database": 5, "collection": "c", "output": "data.r"}), "INPUT_PARSE", ".database"),
            row("O5, a non-string op", "mongo_write", mongo(json!({"op": 5})), "INPUT_PARSE", ".op"),
            row("O5, a non-boolean all", "mongo_write", mongo(json!({"op": "delete_many", "all": "yes"})), "INPUT_PARSE", ".all"),
            row("O5, a secret in a document", "mongo_read", mongo(json!({"output": "data.r", "filter": {"a": {"secret": "s"}}})), "SECRET_IN_DOCUMENT", ".filter"),
            row("O5, a malformed var hole", "mongo_read", mongo(json!({"output": "data.r", "filter": {"a": {"var": 5}}})), "INVALID_PATH", ".filter"),
            // O6.
            row("O6, both key and keys", "cache_read", json!({"connector": "c", "key": "k", "keys": ["k"], "output": "data.r"}), "INPUT_PARSE", ".keys"),
            row("O6, neither key nor keys", "cache_read", json!({"connector": "c", "output": "data.r"}), "INPUT_PARSE", ".key"),
            row("O6, a non-string connector", "cache_write", json!({"connector": 5, "key": "k", "value": 1}), "INPUT_PARSE", ".connector"),
            row("O6, break 2a: cache_write's output", "cache_write", json!({"connector": "c", "key": "k", "value": 1, "output": "data.x"}), "UNKNOWN_KEY", ".output"),
            row("O6, break 2a", "cache_read", json!({"connector": "c", "key": "k", "output": "data.r", "tll": 5}), "UNKNOWN_KEY", ".tll"),
            row("O6, break 2a", "cache_delete", json!({"connector": "c", "keys": ["k"], "prefix": "x"}), "UNKNOWN_KEY", ".prefix"),
            row("O6, break 2a", "cache_incr", json!({"connector": "c", "key": "k", "step": 2}), "UNKNOWN_KEY", ".step"),
            row("O6, a secret in value", "cache_write", json!({"connector": "c", "key": "k", "value": {"a": {"secret": "s"}}}), "SECRET_IN_DOCUMENT", ".value"),
            row("O6, a malformed var hole in value", "cache_write", json!({"connector": "c", "key": "k", "value": {"a": {"var": 5}}}), "INVALID_PATH", ".value"),
            row("O6, a secret in ttl_secs", "cache_write", json!({"connector": "c", "key": "k", "value": 1, "ttl_secs": {"secret": "s"}}), "SECRET_IN_DOCUMENT", ".ttl_secs"),
            row("O6, a malformed var hole in ttl_secs", "cache_write", json!({"connector": "c", "key": "k", "value": 1, "ttl_secs": {"var": 5}}), "INVALID_PATH", ".ttl_secs"),
            // O7.
            row("O7, crypto: an unknown op", "crypto", json!({"op": "frobnicate"}), "INPUT_PARSE", ""),
            row("O7, crypto: the op x algorithm table", "crypto", hmac(json!({"algorithm": "md5"})), "INPUT_PARSE", ".algorithm"),
            row("O7, crypto: a field the op does not take", "crypto", json!({"op": "hash", "algorithm": "sha256", "data": "x", "key": "k"}), "INPUT_PARSE", ".key"),
            row("O7, crypto: an unknown encoding", "crypto", hmac(json!({"encoding": "base99"})), "INPUT_PARSE", ".encoding"),
            row("O7, crypto: a non-string op", "crypto", json!({"op": 5}), "INPUT_PARSE", ".op"),
            row("O7, jwt_sign: an unsupported algorithm", "jwt_sign", sign(json!({"algorithm": "none"})), "INPUT_PARSE", ".algorithm"),
            row("O7, jwt_sign: no expires_in and no exp", "jwt_sign", json!({"algorithm": "HS256", "key": "k", "output": "data.t"}), "INPUT_PARSE", ".expires_in"),
            row("O7, jwt_sign: an unparseable expires_in", "jwt_sign", sign(json!({"expires_in": "soon"})), "INPUT_PARSE", ".expires_in"),
            row("O7, jwt_verify: empty algorithms", "jwt_verify", verify(json!({"algorithms": []})), "INPUT_PARSE", ".algorithms"),
            row("O7, jwt_verify: neither keys nor jwks_url", "jwt_verify", json!({"token": "t", "algorithms": ["HS256"]}), "INPUT_PARSE", ": jwt_verify requires 'keys' and/or 'jwks_url'"),
            row("O7, jwt_verify: a jwks_url that is not HTTPS", "jwt_verify", json!({"token": "t", "algorithms": ["RS256"], "jwks_url": "http://x"}), "INPUT_PARSE", ".jwks_url"),
            row("O7, jwt_verify: a non-array algorithms", "jwt_verify", verify(json!({"algorithms": "HS256"})), "INPUT_PARSE", ".algorithms"),
            row("O7, jwt_verify: a non-boolean require_exp", "jwt_verify", verify(json!({"require_exp": "yes"})), "INPUT_PARSE", ".require_exp"),
            row("O7, jwt_verify: a non-string algorithms entry", "jwt_verify", verify(json!({"algorithms": [5]})), "INPUT_PARSE", ".algorithms"),
            row("O7, jwt_verify: a keys element that is not an object", "jwt_verify", verify(json!({"keys": ["k"]})), "INPUT_PARSE", ".keys[0]"),
            row("O7, jwt_verify: an undeclared secret in keys[].key", "jwt_verify", verify(json!({"keys": [{"algorithm": "HS256", "key": {"secret": "nope"}}]})), "UNKNOWN_SECRET", ".keys[0].key"),
            row("O7, jwt_verify: break 2a in a keys element", "jwt_verify", verify(json!({"keys": [{"algorithm": "HS256", "key": "k", "use": "sig"}]})), "UNKNOWN_KEY", ".keys[0].use"),
            row("O7, jwt_sign: a secret in claims", "jwt_sign", sign(json!({"claims": {"sub": {"secret": "s"}}})), "SECRET_IN_DOCUMENT", ".claims"),
            row("O7, jwt_sign: a malformed var hole in claims", "jwt_sign", sign(json!({"claims": {"sub": {"var": 5}}})), "INVALID_PATH", ".claims"),
            row("O7, jwt_sign: a secret in audience", "jwt_sign", sign(json!({"audience": {"secret": "s"}})), "SECRET_IN_DOCUMENT", ".audience"),
            row("O7, a secret in an output", "jwt_verify", verify(json!({"output": {"secret": "s"}})), "SECRET_IN_MESSAGE_WRITE", ".output"),
            // O8.
            row("O8, an unknown runtime", "model_infer", infer(json!({"runtime": "torch"})), "INPUT_PARSE", ".runtime"),
            row("O8, a timeout_ms that is not a positive integer", "model_infer", infer(json!({"timeout_ms": 0})), "INPUT_PARSE", ".timeout_ms"),
            row("O8, raw beside select", "model_infer", infer(json!({"raw": true, "select": {"var": "x"}})), "INPUT_PARSE", ".select"),
            row("O8, a blank model", "model_infer", infer(json!({"model": " "})), "INPUT_PARSE", ".model"),
            row("O8, a non-string runtime", "model_infer", infer(json!({"runtime": 5})), "INPUT_PARSE", ".runtime"),
            row("O8, a non-boolean raw", "model_infer", infer(json!({"raw": "yes"})), "INPUT_PARSE", ".raw"),
            row("O8, an output not written as a string", "model_infer", infer(json!({"output": 5})), "INPUT_PARSE", ".output"),
            row("O8, an empty output", "model_infer", infer(json!({"output": ""})), "INVALID_PATH", ".output"),
            row("O8, an output that is not a path", "model_infer", infer(json!({"output": "result"})), "INVALID_PATH", ".output"),
            row("O8, break 2a", "model_infer", infer(json!({"temperature": 1})), "UNKNOWN_KEY", ".temperature"),
            row("O8, a secret read in select (K52)", "model_infer", infer(json!({"select": {"secret": "s"}})), "SECRET_IN_MESSAGE_WRITE", ".select"),
            // O10.
            row("O10, \"channel\": null", "channel_call", json!({"channel": null}), "INPUT_PARSE", ".channel"),
            row("O10, break 2a", "channel_call", json!({"channel": "x", "retries": 2}), "UNKNOWN_KEY", ".retries"),
            row("O10, an output of 7", "channel_call", json!({"channel": "x", "output": 7}), "INPUT_PARSE", ".output"),
            row("O10, an output of \"\"", "channel_call", json!({"channel": "x", "output": ""}), "INVALID_PATH", ".output"),
            row("O10, an output of \"result\"", "channel_call", json!({"channel": "x", "output": "result"}), "INVALID_PATH", ".output"),
            // I186: an alias beside its canonical key.
            row("I186, both spellings", "http_call", json!({"connector": "api", "output": "data.a", "response_path": "data.b"}), "UNKNOWN_KEY", ".response_path"),
            row("I186, both spellings", "channel_call", json!({"channel": "x", "channel_logic": "y"}), "UNKNOWN_KEY", ".channel_logic"),
            // Break 21 (I185): a result that is never an object, at a
            // literal root (the default `data` included).
            row("I185, db_read", "db_read", json!({"connector": "db", "query": "q"}), "INPUT_PARSE", ".output"),
            row("I185, data_query reading rows", "data_query", json!({"connector": "db", "query": {"source": "t"}}), "INPUT_PARSE", ".output"),
            row("I185, mongo_read", "mongo_read", mongo(json!({"output": "data"})), "INPUT_PARSE", ".output"),
            row("I185, mongo_aggregate", "mongo_aggregate", mongo(json!({"pipeline": [{"$match": {}}]})), "INPUT_PARSE", ".output"),
            row("I185, storage_presign", "storage_presign", json!({"connector": "s3", "key": "k", "expires_in": 60, "output": "temp_data"}), "INPUT_PARSE", ".output"),
            row("I185, crypto", "crypto", hmac(json!({"output": "data"})), "INPUT_PARSE", ".output"),
            row("I185, jwt_sign", "jwt_sign", sign(json!({"output": "data"})), "INPUT_PARSE", ".output"),
            row("I185, cache_read with keys", "cache_read", json!({"connector": "c", "keys": ["k"]}), "INPUT_PARSE", ".output"),
            row("I185, cache_incr (O6)", "cache_incr", json!({"connector": "c", "key": "k", "output": "data"}), "INPUT_PARSE", ".output"),
            // Break 2: a near miss of a known key in a built-in's input.
            row("break 2", "map", json!({"mappings": [{"path": "data.x", "logic": 1}], "mapings": []}), "LIKELY_TYPO", ".mapings"),
        ];
        assert_quarantined(&engines, &rows);
    }

    /// The plugin functions' load refusals (O9), over the test fixture's
    /// component: the same rule, a Rejected issue quarantining its channel
    /// with its code and path.
    #[tokio::test]
    async fn every_named_plugin_load_break_quarantines_its_channel() {
        use serde_json::json;
        const COMPONENT: &[u8] = include_bytes!("../../tests/fixtures/plugins/fixture.wasm");
        const MANIFEST: &str = include_str!("../../tests/fixtures/plugins/fixture.toml");
        let config = crate::config::PluginsConfig {
            enabled: true,
            ..crate::config::PluginsConfig::default()
        };
        let runtime = crate::plugin::WasmRuntime::new(&config).expect("the sandbox");
        let loaded = runtime.load_blocking(COMPONENT).expect("the fixture loads");
        let manifest = crate::plugin::Manifest::parse(MANIFEST).expect("the manifest");
        let binding = crate::engine::PluginBinding {
            id: manifest.name.clone(),
            version: 1,
            digest: loaded.digest.clone(),
            abi: crate::plugin::ABI.to_string(),
        };
        let limits = crate::plugin::Limits::effective(&config, &manifest.name);
        let functions = manifest
            .entries(&binding)
            .into_iter()
            .zip(&manifest.functions)
            .map(|(entry, decl)| {
                let handler = crate::plugin::PluginFunctionHandler::new(
                    std::sync::Arc::new(entry),
                    loaded.clone(),
                    runtime.clone(),
                    limits,
                );
                let function =
                    crate::plugin::PluginFunction::new(handler.clone(), decl.runtime_fields());
                (handler, function)
            })
            .collect();
        let engines = Engines::orion_with(exprs_with_a_secret(), functions);

        let identity = "test.fixture.identity";
        let upper = "test.fixture.upper";
        #[rustfmt::skip]
        let rows = [
            row("O9, no output and no default root", upper, json!({"text": "x"}), "INPUT_PARSE", ".output"),
            row("O9, an empty output", identity, json!({"message": 1, "output": ""}), "INVALID_PATH", ".output"),
            row("O9, a blank output", identity, json!({"message": 1, "output": "  "}), "INVALID_PATH", ".output"),
            row("O9, an output that is not a path", identity, json!({"message": 1, "output": "result"}), "INVALID_PATH", ".output"),
            row("O9, a folded constant not of its kind", upper, json!({"text": ["a"], "output": "data.x"}), "INPUT_PARSE", ".text"),
            row("O9, a secret in an interpolated field", identity, json!({"message": {"a": {"secret": "s"}}}), "SECRET_IN_DOCUMENT", ".message"),
            row("O9, a malformed var in a resolvable field", identity, json!({"message": {"var": 5}}), "INVALID_PATH", ".message"),
            row("O9, a secret read in a template field (K53)", upper, json!({"text": {"secret": "s"}, "output": "data.x"}), "SECRET_IN_MESSAGE_WRITE", ".text"),
        ];
        assert_quarantined(&engines, &rows);

        // The control: the fixture's functions load when used as declared.
        assert_eq!(
            reason_for(&engines, identity, &json!({"message": {"var": "data.x"}})),
            None
        );
    }

    // ---- build_single: the screen is not the caller's to remember --------

    fn one_task_workflow(function: &str, input: serde_json::Value) -> dataflow_rs_v3::Workflow {
        serde_json::from_value(serde_json::json!({
            "id": "wf-1",
            "name": "wf-1",
            "condition": true,
            "tasks": [{
                "id": "t1",
                "name": "t1",
                "function": { "name": function, "input": input }
            }],
        }))
        .expect("a well-formed workflow")
    }

    /// The divergence this closes, in its sharpest form.
    ///
    /// `enrich` is a dataflow-rs built-in that *requires a handler*, and Orion
    /// never registers one (F54). So it deserializes, `.build()` accepts it,
    /// and every request fails with `FunctionNotFound` — which is why
    /// `dry-run` and `POST /workflows/{id}/test` calling `.build()` by hand
    /// gave a green answer for a workflow boot would quarantine.
    ///
    /// It is also the trap in screening by severity: this is
    /// `Severity::Defect`, the class between "refused" and "advisory", and the
    /// only one where `build` and the first message disagree. A screen written
    /// as "pass anything `build` would accept" lets it through. Both halves of
    /// that are asserted below, so the prose above is checked rather than
    /// trusted.
    #[test]
    fn build_single_refuses_a_built_in_with_no_handler_behind_it() {
        let task = || {
            one_task_workflow(
                "enrich",
                serde_json::json!({ "connector": "c", "merge_path": "data" }),
            )
        };
        assert!(
            screen()
                .check_workflow(&task())
                .iter()
                .any(|i| i.severity() == dataflow_rs_v3::Severity::Defect),
            "`enrich` with no handler is the Defect class"
        );
        assert!(
            dataflow_rs_v3::Engine::builder()
                .with_workflow(task())
                .build()
                .is_ok(),
            "`build` accepts it — the premise that makes this screen necessary"
        );

        // `dataflow_rs_v3::Engine` is not `Debug`, so unwrap the arms by hand.
        let Err(err) = build_single(
            task(),
            std::collections::HashMap::new(),
            &crate::engine::ResolvedSecrets::empty(),
            0,
        ) else {
            unreachable!(
                "`enrich` has no handler in any Orion engine — building it is \
                 the bug this screen exists to catch"
            );
        };

        let msg = err.to_string();
        assert!(
            msg.contains("wf-1") && msg.contains("t1"),
            "the refusal must name the workflow and the task: {msg}"
        );
        assert!(
            matches!(err, crate::errors::OrionError::Validation { .. }),
            "an unusable task is the author's document to fix, not a server \
             fault: {err:?}"
        );
    }

    /// The control: a self-contained built-in needs no handler, so the same
    /// call builds. Without this the test above would pass for a `build_single`
    /// that refuses everything.
    #[test]
    fn build_single_builds_a_workflow_every_engine_can_run() {
        assert!(
            build_single(
                one_task_workflow("log", serde_json::json!({ "message": "x" })),
                std::collections::HashMap::new(),
                &crate::engine::ResolvedSecrets::empty(),
                0,
            )
            .is_ok(),
            "`log` is self-contained — every engine dispatches it"
        );
    }
}
