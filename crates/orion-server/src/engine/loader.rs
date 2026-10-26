//! Turning stored channel + workflow rows into the workflow sets the engines
//! are built from — including the per-channel quarantine that keeps one bad
//! row from taking the instance down.
//!
//! Every row is compiled on the generation's compiler ([`compile_row`]), and
//! its verdict decides quarantine ([`quarantines`]). A rollout set stays all
//! or nothing.

use std::collections::HashMap;

use dataflow_rs::Issue;
use dataflow_rs::def::{ParseOptions, parse};
use dataflow_rs::program::{
    Compiled, CompiledWorkflow, Compiler, IssueCode, Program, Severity, StepKind,
};
use serde_json::Value;

use crate::storage::models::{Channel, Workflow};

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

// -- dataflow-rs 4: the verdict that decides quarantine (I182) --

/// How Orion reads a workflow definition: with dataflow-rs's v3 compat
/// reader off. A v3 spelling — a task's `continue_on_error` or `halt_on`,
/// `validate` for `validation` — is then an unknown key or function, and
/// refused; [`name_v3_spellings`] says what to write instead.
pub fn parse_options() -> ParseOptions {
    ParseOptions::default().compat(false)
}

/// Names, in each issue of `issues` that sits at a v3 spelling of `json`,
/// the v4 spelling to write in its place.
///
/// The spellings are the compat reader's own: a parse with it on reports
/// each as a `V3_SPELLING` at the v3 key, the path the strict parse refuses
/// the key at (a function name's refusal is the compile's, at the same
/// `function.name`). An issue anywhere else is left as it is.
pub fn name_v3_spellings(json: &Value, issues: &mut [Issue]) {
    if issues.is_empty() {
        return;
    }
    let spellings: Vec<(String, String)> = parse(json, ParseOptions::default().compat(true))
        .issues
        .into_iter()
        .filter(|issue| issue.code == IssueCode::V3Spelling)
        .map(|issue| (issue.path.to_string(), issue.message))
        .collect();
    if spellings.is_empty() {
        return;
    }
    for issue in issues.iter_mut() {
        let path = issue.path.to_string();
        if let Some((_, spelling)) = spellings.iter().find(|(at, _)| *at == path) {
            // The reader's text says what it read the key as: "`halt_on:
            // failure` is a v3 spelling, read as `on_failure: fail, …`".
            let instead = match spelling.split_once(", read as ") {
                Some((v3, v4)) => format!("{v3}, which is no longer read: write {v4}"),
                None => spelling.clone(),
            };
            issue.message = format!("{} — {instead}", issue.message);
        }
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
/// `warn`, and `lint` reports it. Advisories never
/// quarantine. A severity a later dataflow-rs adds quarantines until it is
/// classified here: only what is known to be harmless passes.
pub fn quarantines(issue: &Issue) -> bool {
    match issue.severity {
        Severity::Advisory => false,
        Severity::Defect => matches!(issue.code, IssueCode::InputParse | IssueCode::LikelyTypo),
        _ => true,
    }
}

/// Parses and compiles one workflow definition on dataflow-rs 4
/// (`def::parse` with the compat reader off, then `Compiler::compile_one`).
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
    let parsed = parse(json, parse_options());
    let mut issues = parsed.issues;
    let Some(def) = parsed.def else {
        name_v3_spellings(json, &mut issues);
        return Err(issues);
    };
    match compiler.compile_one(&def) {
        Err(mut compiled) => {
            issues.append(&mut compiled);
            name_v3_spellings(json, &mut issues);
            Err(issues)
        }
        Ok(mut compiled) => {
            issues.append(&mut compiled.issues);
            if issues.iter().any(quarantines) {
                name_v3_spellings(json, &mut issues);
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
fn describe(issue: &Issue, plugins: &dyn IssueNotes, json: &Value) -> String {
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
/// that [`quarantines`] it, described.
pub fn refusal(issues: &[Issue], plugins: &dyn IssueNotes, json: &Value) -> String {
    issues
        .iter()
        .filter(|issue| quarantines(issue))
        .map(|issue| describe(issue, plugins, json))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Logs what a compile found and did not quarantine on: each Defect (a
/// `PAYLOAD_VAR`, say) and each Advisory, at `warn`, with the workflow,
/// version and path.
///
fn log_findings(workflow: &Workflow, compiled: &Compiled) {
    for issue in &compiled.issues {
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

/// Every active channel's workflows, compiled.
///
/// For each active channel, finds the associated workflow(s) and builds each
/// row's definition with the channel name injected as its channel, then
/// compiles it ([`compile_row`]); a row it refuses quarantines the channel
/// with its issues' codes and paths.
///
/// F33: a channel whose workflows cannot be built — missing `workflow_id`,
/// workflow not found among the active set, or a version that fails
/// conversion — is reported as a [`ChannelLoadIssue`] instead of being
/// silently skipped. Callers feed these into `ChannelLoader::build`, which
/// quarantines the channel: previously it stayed registered in the route
/// table with no workflow behind it, so requests got an opaque engine error.
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
            loaded.issues.push(ChannelLoadIssue {
                channel: channel.name.clone(),
                reason: "channel has no workflow_id".to_string(),
            });
            continue;
        };

        let Some(wf_versions) = workflow_map.get(wf_id) else {
            loaded.issues.push(ChannelLoadIssue {
                channel: channel.name.clone(),
                reason: format!("workflow '{wf_id}' not found among active workflows"),
            });
            continue;
        };

        match load_channel(channel, wf_id, wf_versions, load) {
            Ok(compiled) => loaded.workflows.extend(compiled),
            Err(reason) => loaded.issues.push(ChannelLoadIssue {
                channel: channel.name.clone(),
                reason,
            }),
        }
    }

    loaded
}

/// A channel the registry refused to load, and why.
///
/// Two classes produce one:
/// - **Config invariants (any mode).** A `config_json` that no longer parses
///   (N3) or a `validation_logic` that no longer compiles (N4) would load the
///   channel with its guards silently absent — serving unvalidated,
///   unthrottled traffic on the strength of a log line. Both are checked at
///   create/update time, so a failure here means the stored row is corrupt.
/// - **Backend degradation (cluster mode only).** A dedup/response-cache
///   backend that would fall back to per-node memory is a correctness loss in
///   a cluster but an acceptable degradation on one node.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChannelLoadIssue {
    pub channel: String,
    pub reason: String,
}

/// What a refusal can add about a function a task names and this build does
/// not have: a plugin set says which plugin it belongs to and why that plugin
/// did not load. The engine knows nothing of plugins, so it asks.
pub trait IssueNotes {
    /// Append to `reason` what is known about the function `issue` names in
    /// `workflow`, if anything.
    fn annotate_issue(&self, issue: &Issue, workflow: &Value, reason: &mut String);
}

/// Nothing to add: no plugins are loaded.
pub struct NoIssueNotes;

impl IssueNotes for NoIssueNotes {
    fn annotate_issue(&self, _issue: &Issue, _workflow: &Value, _reason: &mut String) {}
}

/// What [`build_engine_workflows`] loads against, from one generation's
/// functions.
pub struct EngineLoad<'a> {
    /// The generation's compiler: on its `Registry`, which holds Orion's
    /// functions, the plugin functions and the profile layer.
    pub compiler: &'a Compiler,
    /// The generation's plugins, which name the plugin a refused task's
    /// function belongs to and why it did not load.
    pub plugins: &'a dyn IssueNotes,
}

/// What [`build_engine_workflows`] produced.
#[derive(Default)]
pub struct LoadedWorkflows {
    /// The workflows that loaded, compiled, for the generation's program.
    pub workflows: Vec<CompiledWorkflow>,
    /// Every channel that did not load, and why.
    pub issues: Vec<ChannelLoadIssue>,
}

/// One channel's workflows, compiled, or its quarantine reason.
fn load_channel(
    channel: &Channel,
    wf_id: &str,
    wf_versions: &[&Workflow],
    load: &EngineLoad<'_>,
) -> Result<Vec<CompiledWorkflow>, String> {
    if wf_versions.len() == 1 && wf_versions[0].rollout_percentage == 100 {
        // Single version at 100% — convert normally
        let row = wf_versions[0];
        let json = workflow_json(
            row,
            &channel.name,
            &row.workflow_id,
            crate::storage::models::EntityStatus::Active.as_str(),
            None,
        )
        .map_err(|e| format!("workflow '{wf_id}' failed to convert: {e}"))?;
        let compiled = compile_row(load.compiler, &json).map_err(|issues| {
            format!(
                "workflow '{wf_id}' failed to compile: {}",
                refusal(&issues, load.plugins, &json)
            )
        })?;
        log_findings(row, &compiled);
        return Ok(vec![compiled.workflow]);
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
    let ranges = dataflow_rs::def::Rollout::partition(&percentages).map_err(|e| {
        format!(
            "rollout percentages for workflow '{wf_id}' do not partition the \
             bucket space: {e}"
        )
    })?;

    // All-or-nothing per channel: a partially-compiled rollout would silently
    // blackhole the failed version's bucket range.
    let mut compiled = Vec::with_capacity(sorted.len());
    for (wf, rollout) in sorted.iter().zip(&ranges) {
        let failed = |e: String| {
            format!(
                "workflow '{}' v{} failed to compile: {e}",
                wf.workflow_id, wf.version
            )
        };
        let json = workflow_json(
            wf,
            &channel.name,
            &format!("{}:v{}", wf.workflow_id, wf.version),
            &wf.status,
            Some((rollout.bucket_start, rollout.bucket_end)),
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

    Ok(compiled)
}

/// The workflow definition `channel` serves `workflow` as: the JSON the
/// engine reads a stored row through, which `def::parse` takes as it is.
///
/// `id` and `status` are the row's own for a workflow taking all of its
/// channel's traffic, and version-qualified for one version of a rollout;
/// `rollout` is the half-open bucket range `(start, end)` the version serves,
/// `None` for a workflow that takes all of its channel's traffic.
///
/// The `continue_on_error` column is written as dataflow-rs 4 spells it:
/// `"on_failure": "continue"` when it is set, and nothing when it is
/// not, which is the default. A task's own keys, inside `tasks_json`, are the
/// author's and are left as written.
///
/// `http_call`'s destination path used to be rewritten here from Orion's
/// `output` spelling to the upstream `response_path`, because
/// `{"name": "http_call"}` deserializes into dataflow-rs's own
/// `HttpCallConfig` and Orion could not annotate it. No function input has
/// an alias any more: a row is read as it is stored.
///
/// # Errors
///
/// A row whose `tasks_json`, `condition_json`, `tags_json` or `loop_json`
/// is not the JSON it must be.
pub fn workflow_json(
    workflow: &crate::storage::models::Workflow,
    channel: &str,
    id: &str,
    status: &str,
    rollout: Option<(u8, u8)>,
) -> Result<serde_json::Value, crate::errors::OrionError> {
    let tasks: serde_json::Value = serde_json::from_str(&workflow.tasks_json)?;
    let condition: serde_json::Value = serde_json::from_str(&workflow.condition_json)?;
    let tags: Vec<String> = serde_json::from_str(&workflow.tags_json)?;
    // `null` rather than an omitted key: the engine reads it as no loop,
    // and being explicit keeps the built JSON's shape the same for every
    // workflow.
    let loop_config: serde_json::Value = match workflow.loop_json.as_deref() {
        Some(json) => serde_json::from_str(json)?,
        None => serde_json::Value::Null,
    };

    let mut json = serde_json::json!({
        "id": id,
        "name": workflow.name,
        "description": workflow.description,
        "channel": channel,
        "priority": workflow.priority,
        "version": workflow.version,
        "status": status,
        "condition": condition,
        "tasks": tasks,
        "tags": tags,
        "loop": loop_config,
        "rollout": rollout.map(|(bucket_start, bucket_end)| serde_json::json!({
            "bucket_start": bucket_start,
            "bucket_end": bucket_end,
        })),
    });
    if workflow.continue_on_error
        && let Some(object) = json.as_object_mut()
    {
        object.insert(
            "on_failure".to_string(),
            serde_json::Value::String("continue".to_string()),
        );
    }
    Ok(json)
}

/// The loader's test fixtures: every Orion function on a compiler, channels
/// and workflows to load on it, and the named load breaks held to their
/// quarantines. The plugin host's tests load its functions through them.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
pub mod testing {
    use super::*;

    /// No functions: what [`Engines::orion`] adds beside Orion's own.
    pub struct NoFunction;

    impl dataflow_rs::Function for NoFunction {
        type Config = ();

        fn spec(&self) -> dataflow_rs::function::FunctionSpec {
            dataflow_rs::function::FunctionSpec::default()
        }

        async fn call(
            &self,
            _ctx: &mut dataflow_rs::Ctx<'_>,
            _cfg: &(),
        ) -> dataflow_rs::Result<dataflow_rs::Outcome> {
            unreachable!("never registered")
        }
    }

    impl crate::engine::NamedFunction for NoFunction {
        fn name(&self) -> &str {
            unreachable!("never registered")
        }
    }

    /// Every Orion function over inert dependencies, as a generation builds
    /// its registry.
    pub struct Engines {
        pub compiler: Compiler,
        plugins: NoIssueNotes,
        _offline: crate::engine::kit::OfflineDeps,
    }

    impl Engines {
        pub fn orion() -> Self {
            Self::orion_with::<NoFunction>(crate::engine::kit::test_exprs(), Vec::new())
        }

        /// Every Orion function on `exprs`, and each plugin function.
        pub fn orion_with<F: crate::engine::NamedFunction>(
            exprs: dataflow_rs::expr::ExprEngine,
            plugins: Vec<F>,
        ) -> Self {
            let offline = crate::engine::kit::OfflineDeps::new();
            let mut registry = dataflow_rs::Registry::new(exprs);
            crate::engine::register_functions(&mut registry, offline.deps());
            crate::engine::register_plugin_functions(&mut registry, plugins);
            Self {
                compiler: registry.compiler(dataflow_rs::program::CompileOptions::default()),
                plugins: NoIssueNotes,
                _offline: offline,
            }
        }

        /// The built-ins alone, which is all the `log` fixtures need.
        pub fn bare() -> Self {
            let registry = dataflow_rs::Registry::new(crate::engine::kit::test_exprs());
            Self {
                compiler: registry.compiler(dataflow_rs::program::CompileOptions::default()),
                plugins: NoIssueNotes,
                _offline: crate::engine::kit::OfflineDeps::new(),
            }
        }

        pub fn load(&self, channels: &[Channel], workflows: &[Workflow]) -> LoadedWorkflows {
            build_engine_workflows(
                channels,
                workflows,
                &EngineLoad {
                    compiler: &self.compiler,
                    plugins: &self.plugins,
                },
            )
        }
    }

    pub fn make_channel(name: &str) -> Channel {
        Channel {
            tags_json: "[]".to_string(),
            channel_id: name.to_string(),
            name: name.to_string(),
            version: 1,
            status: crate::storage::models::EntityStatus::Active
                .as_str()
                .to_string(),
            channel_type: "sync".to_string(),
            protocol: "http".to_string(),
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

    pub fn make_workflow(wf_id: &str, version: i64, rollout: i64) -> Workflow {
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

    pub fn workflow_with_raw_tasks(wf_id: &str, tasks_json: &str) -> Workflow {
        let mut wf = make_workflow(wf_id, 1, 100);
        wf.tasks_json = tasks_json.to_string();
        wf
    }

    /// One named load break: a task's input dataflow-rs 4 refuses at load,
    /// and the issue the channel's quarantine reason names for it.
    pub struct Break {
        /// The break, and the task whose report names it.
        pub cite: &'static str,
        pub function: &'static str,
        pub input: Value,
        /// The issue's code.
        pub code: &'static str,
        /// Its path below the task's `function.input`.
        pub at: &'static str,
    }

    pub fn row(
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
    pub fn reason_for(engines: &Engines, function: &str, input: &Value) -> Option<String> {
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
    pub fn assert_quarantined(engines: &Engines, rows: &[Break]) {
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

    /// Orion's expressions with the secret `s` declared, so a row's
    /// `{"secret": "s"}` is refused for where it sits and not as unknown,
    /// and `{"secret": "nope"}` is the undeclared one.
    pub fn exprs_with_a_secret() -> dataflow_rs::expr::ExprEngine {
        let mut values = serde_json::Map::new();
        values.insert("s".to_string(), Value::String("v".to_string()));
        crate::engine::operators::orion_exprs(
            &crate::engine::ResolvedSecrets::from_values(values),
            0,
        )
        .expect("Orion's expressions")
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    /// [`Engines::bare`]'s load, as `(the workflows, the issues)`.
    fn load(
        channels: &[Channel],
        workflows: &[Workflow],
    ) -> (Vec<CompiledWorkflow>, Vec<ChannelLoadIssue>) {
        let loaded = Engines::bare().load(channels, workflows);
        (loaded.workflows, loaded.issues)
    }

    /// An advisory never quarantines: a stripped `$` key and a `validation`
    /// that gates nothing both load, because the workflow runs. `$$set` is the documented fix for the first, so treating
    /// it as fatal would take down the channel of an author who did the right
    /// thing.
    #[test]
    fn an_advisory_never_quarantines() {
        let mut channel = make_channel("advised");
        channel.workflow_id = Some("wf-advised".to_string());
        let wfs = vec![workflow_with_raw_tasks(
            "wf-advised",
            r#"[
                {"id":"check","name":"check","function":{"name":"validation","input":{
                    "rules":[{"logic":{"==":[1,2]},"message":"no"}]}}},
                {"id":"g","name":"g","tasks":[
                    {"id":"m","name":"m","function":{"name":"map","input":{"mappings":[
                        {"path":"data.doc","logic":{"$set":{"a":1}}}]}}}]}
            ]"#,
        )];
        let (loaded, issues) = load(&[channel], &wfs);
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(loaded.len(), 1);
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

    /// The bucket ranges must partition 0–99 contiguously, newest version
    /// first: 20/50/30 across v3/v2/v1 → v3 [0,20), v2 [20,70), v1 [70,100).
    /// A gap or overlap in the offsets silently misroutes traffic, which is
    /// exactly what this accumulation arithmetic exists to prevent — so every
    /// bucket is run, and must be served by exactly the version that owns it.
    #[tokio::test]
    async fn test_rollout_bucket_offsets_partition_newest_first() {
        let mut channel = make_channel("rollout-ch");
        channel.workflow_id = Some("wf".to_string());
        let version = |v: i64, pct: i64| {
            let mut wf = make_workflow("wf", v, pct);
            wf.tasks_json = serde_json::json!([{"id": "mark", "name": "mark", "function": {
                "name": "map", "input": {"mappings": [{"path": "data.version", "logic": v}]}}}])
            .to_string();
            wf
        };
        let wfs = vec![version(1, 30), version(2, 50), version(3, 20)];
        let engines = Engines::bare();
        let loaded = engines.load(&[channel], &wfs);
        assert!(loaded.issues.is_empty(), "{:?}", loaded.issues);
        assert_eq!(
            loaded.workflows.iter().map(|w| w.id()).collect::<Vec<_>>(),
            ["wf:v3", "wf:v2", "wf:v1"]
        );

        let engine = serving_engine(engines.compiler.link(loaded.workflows));
        for bucket in 0u8..100 {
            let mut message = crate::engine::execute::serving_message(
                &serde_json::json!({}),
                &serde_json::json!({}),
            );
            let options = dataflow_rs::run::RunOptions::default()
                .channel("rollout-ch")
                .routing_bucket(bucket);
            let run = engine.run_with(&mut message, &options).await;
            assert_eq!(run.matched(), 1, "bucket {bucket} is served by one version");
            let expected = match bucket {
                0..20 => 3,
                20..70 => 2,
                _ => 1,
            };
            assert_eq!(
                Value::from(message.context().data())["version"],
                expected,
                "bucket {bucket}"
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
        assert_eq!(converted[0].id(), "wf-good");
        assert_eq!(issues.len(), 1, "issues = {issues:?}");
        assert_eq!(issues[0].channel, "bad");
        // The engine's verdict decides, so the reason is its issue's code
        // and path (I182).
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

    /// A v3 spelling is no longer read: a task's `continue_on_error` and
    /// `halt_on` are refused at the key, `validate` as an unknown function,
    /// and each refusal names the v4 spelling to write. A function input's
    /// 1.12 alias is an unknown key of the function's config.
    #[test]
    fn a_v3_spelling_is_refused_naming_its_v4_spelling() {
        let engines = Engines::orion();
        let task = |extra: Value, name: &str, input: Value| {
            let mut task =
                serde_json::json!({"id": "t", "function": {"name": name, "input": input}});
            if let (Some(task), Some(extra)) = (task.as_object_mut(), extra.as_object()) {
                task.extend(extra.clone());
            }
            serde_json::json!({"id": "w", "tasks": [task]})
        };
        let log = serde_json::json!({"message": "x"});
        let cases = [
            (
                task(
                    serde_json::json!({"continue_on_error": true}),
                    "log",
                    log.clone(),
                ),
                "tasks[0].continue_on_error",
                "write `on_failure: continue, on_rejection: continue`",
            ),
            (
                task(
                    serde_json::json!({"halt_on": "failure"}),
                    "log",
                    log.clone(),
                ),
                "tasks[0].halt_on",
                "write `on_failure: fail, on_rejection: halt`",
            ),
            (
                task(
                    serde_json::json!({}),
                    "validate",
                    serde_json::json!({"rules": [{"logic": true, "message": "m"}]}),
                ),
                "tasks[0].function.name",
                "write `validation`",
            ),
        ];
        for (json, path, instead) in cases {
            let issues = compile_row(&engines.compiler, &json).expect_err("refused");
            let issue = issues
                .iter()
                .find(|i| i.path.to_string() == path)
                .unwrap_or_else(|| unreachable!("no issue at {path}: {issues:?}"));
            assert!(issue.message.contains(instead), "{}", issue.message);
        }

        for (function, alias) in [
            ("http_call", "response_path"),
            ("http_call", "path_logic"),
            ("channel_call", "channel_logic"),
        ] {
            let json = task(
                serde_json::json!({}),
                function,
                serde_json::json!({"connector": "c", alias: "data.x"}),
            );
            let issues = compile_row(&engines.compiler, &json).expect_err("refused");
            let at = format!("tasks[0].function.input.{alias}");
            assert!(
                issues
                    .iter()
                    .any(|i| i.path.to_string() == at && i.severity == Severity::Rejected),
                "{function}.{alias}: {issues:?}"
            );
        }
    }

    /// A rollout set is all or nothing: a version the compiler refuses drops
    /// the whole set. The refusal is break 2a: `cache_write`'s `output`,
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
        assert!(loaded.workflows.is_empty());
        assert_eq!(loaded.issues.len(), 1, "{:?}", loaded.issues);
        let reason = &loaded.issues[0].reason;
        assert!(
            reason.starts_with(
                "workflow 'wf' v1 failed to compile: task 't1': UNKNOWN_KEY at \
                 tasks[0].function.input.output"
            ),
            "{reason}"
        );

        // The same set without the stray key loads whole.
        let mut fixed = wfs.clone();
        fixed[0].tasks_json = fixed[0].tasks_json.replace(r#","output":"data.x""#, "");
        let loaded = engines.load(&[channel], &fixed);
        assert!(loaded.issues.is_empty(), "{:?}", loaded.issues);
        assert_eq!(
            loaded.workflows.iter().map(|w| w.id()).collect::<Vec<_>>(),
            ["wf:v2", "wf:v1"]
        );
    }

    /// I185 and I182: break 21's root-output Defect quarantines, its code and
    /// path in the reason (1.12 loaded the row, and replaced `data` with the
    /// rows on every call). A `PAYLOAD_VAR` Defect does not: that row loads,
    /// and the compile keeps the Defect for the log.
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
        assert_eq!(
            loaded.workflows.iter().map(|w| w.id()).collect::<Vec<_>>(),
            ["wf-payload"]
        );

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

    /// Break 2: a near miss of a task key is a `LIKELY_TYPO`, and it
    /// quarantines, where 1.12 ignored the key and ran the task
    /// unconditionally. With the v3 compat reader off, a stray key that
    /// resembles none is refused too: a definition carries only its own keys,
    /// and an extension goes in `ext`.
    #[test]
    fn a_likely_typo_and_an_unknown_key_both_quarantine() {
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

        let (converted, issues) = load(&[typo, stray], &wfs);
        assert_eq!(issues.len(), 2, "{issues:?}");
        assert_eq!(issues[0].channel, "typo");
        assert!(
            issues[0]
                .reason
                .contains("task 't1': LIKELY_TYPO at tasks[0].conditon"),
            "{}",
            issues[0].reason
        );
        assert_eq!(issues[1].channel, "stray");
        assert!(
            issues[1]
                .reason
                .contains("task 't1': UNKNOWN_KEY at tasks[0].owner"),
            "{}",
            issues[1].reason
        );
        assert!(converted.is_empty());
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

    /// Every load refusal the cutover brings for Orion's own functions, each
    /// quarantining its channel with its code and path in the reason: 1.12
    /// failed every call on each, or refused it when the workflow was
    /// written, and loaded it (or for break 2a and break 21, ran it). Each
    /// row cites the break and the task that ported the field.
    #[tokio::test]
    async fn every_named_load_break_quarantines_its_channel() {
        use serde_json::json;
        let engines = Engines::orion_with::<NoFunction>(exprs_with_a_secret(), Vec::new());
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
        #[rustfmt::skip]
        let rows = [
            // I183: a field schema.rs marks required, missing.
            row("I183 (O3)", "http_call", json!({}), "INPUT_PARSE", ".connector"),
            row("I183 (O4)", "db_read", json!({"connector": "db"}), "INPUT_PARSE", ".query"),
            row("I183 (O4)", "db_write", json!({"connector": "db"}), "INPUT_PARSE", ".query"),
            row("I183 (O4)", "data_query", json!({"connector": "db"}), "INPUT_PARSE", ".query"),
            row("I183 (O4)", "data_write", json!({"connector": "db"}), "INPUT_PARSE", ".write"),
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
            row("I183 (O10)", "channel_call", json!({}), "INPUT_PARSE", ".channel"),
            // O2: key material read by the engine's secret checks.
            row("O2, KeyMaterial", "crypto", hmac(json!({"key": {"secret": "nope"}})), "UNKNOWN_SECRET", ".key"),
            row("O2, KeyMaterialList", "jwt_verify", verify(json!({"issuer": {"secret": "nope"}})), "UNKNOWN_SECRET", ".issuer"),
            row("O2, a nested secret in a key-list object", "jwt_verify", verify(json!({"audience": [{"a": {"secret": "s"}}]})), "SECRET_IN_DOCUMENT", ".audience"),
            // O3.
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
            row("I185, crypto", "crypto", hmac(json!({"output": "data"})), "INPUT_PARSE", ".output"),
            row("I185, jwt_sign", "jwt_sign", sign(json!({"output": "data"})), "INPUT_PARSE", ".output"),
            row("I185, cache_read with keys", "cache_read", json!({"connector": "c", "keys": ["k"]}), "INPUT_PARSE", ".output"),
            row("I185, cache_incr (O6)", "cache_incr", json!({"connector": "c", "key": "k", "output": "data"}), "INPUT_PARSE", ".output"),
            // Break 2: a near miss of a known key in a built-in's input.
            row("break 2", "map", json!({"mappings": [{"path": "data.x", "logic": 1}], "mapings": []}), "LIKELY_TYPO", ".mapings"),
        ];
        assert_quarantined(&engines, &rows);
    }

    // ---- one workflow, compiled as a generation compiles it ---------------

    fn one_task_workflow(function: &str, input: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "id": "wf-1",
            "name": "wf-1",
            "channel": "c",
            "condition": true,
            "tasks": [{
                "id": "t1",
                "name": "t1",
                "function": { "name": function, "input": input }
            }],
        })
    }

    /// A task naming a function no registry holds is refused when the
    /// workflow is compiled, naming the workflow and the task — an offline
    /// run or `POST /definitions/test` cannot give a green answer for a
    /// workflow boot would quarantine.
    #[test]
    fn single_engine_refuses_a_function_nothing_dispatches() {
        let engines = Engines::bare();
        let Err(err) = crate::engine::offline::single_engine(
            &engines.compiler,
            &one_task_workflow("enrich", serde_json::json!({ "connector": "c" })),
        ) else {
            unreachable!("`enrich` has no function behind it");
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

    /// The control: a self-contained built-in needs no host function, so the
    /// same call builds.
    #[test]
    fn single_engine_builds_a_workflow_every_engine_can_run() {
        assert!(
            crate::engine::offline::single_engine(
                &Engines::bare().compiler,
                &one_task_workflow("log", serde_json::json!({ "message": "x" })),
            )
            .is_ok(),
            "`log` is self-contained"
        );
    }
}

#[cfg(test)]
mod workflow_json_tests {
    use super::workflow_json;
    use crate::storage::models::{EntityStatus, Workflow};

    /// The strict reader the engine loads every stored definition with (the
    /// v3 compat reader off). Spelled here rather than borrowed from
    /// `engine`, which sits above storage.
    fn strict_parse(json: &serde_json::Value) -> dataflow_rs::def::Parsed {
        dataflow_rs::def::parse(
            json,
            dataflow_rs::def::ParseOptions::default().compat(false),
        )
    }

    #[test]
    fn test_workflow_json_basic() {
        let workflow = Workflow {
            workflow_id: "test-workflow".to_string(),
            name: "Test Workflow".to_string(),
            description: Some("A test workflow".to_string()),
            priority: 10,
            version: 1,
            status: EntityStatus::Active.as_str().to_string(),
            rollout_percentage: 100,
            condition_json: "true".to_string(),
            tasks_json: r#"[{"id":"log_task","name":"Log","function":{"name":"log","input":{"message":"hello"}}}]"#.to_string(),
            tags_json: "[]".to_string(),
            loop_json: None,
            continue_on_error: false,
            created_at: chrono::NaiveDateTime::default(),
            updated_at: chrono::NaiveDateTime::default(),
        };

        let json =
            workflow_json(&workflow, "default", "test-workflow", "active", None).expect("test");
        assert_eq!(json["id"], "test-workflow");
        assert_eq!(json["name"], "Test Workflow");
        assert_eq!(json["channel"], "default");
        assert_eq!(json["priority"], 10);
        let parsed = strict_parse(&json);
        assert!(parsed.def.is_some(), "{:?}", parsed.issues);
    }

    #[test]
    fn test_workflow_json_custom_channel() {
        let workflow = Workflow {
            workflow_id: "wf-orders".to_string(),
            name: "Order Workflow".to_string(),
            description: None,
            priority: 5,
            version: 2,
            status: EntityStatus::Active.as_str().to_string(),
            rollout_percentage: 100,
            condition_json: r#"{"==": [{"var": "type"}, "order"]}"#.to_string(),
            tasks_json: r#"[{"id":"t1","name":"Process","function":{"name":"log","input":{"message":"test"}}}]"#
                .to_string(),
            tags_json: r#"["orders"]"#.to_string(),
            loop_json: None,
            continue_on_error: true,
            created_at: chrono::NaiveDateTime::default(),
            updated_at: chrono::NaiveDateTime::default(),
        };

        let json = workflow_json(&workflow, "orders", "wf-orders", "active", None).expect("test");
        assert_eq!(json["channel"], "orders");
        assert_eq!(json["id"], "wf-orders");
    }

    #[test]
    fn test_workflow_json_with_rollout_carries_a_typed_range() {
        let workflow = Workflow {
            workflow_id: "rollout-wf".to_string(),
            name: "Rollout Test".to_string(),
            description: None,
            priority: 1,
            version: 3,
            status: EntityStatus::Active.as_str().to_string(),
            rollout_percentage: 50,
            condition_json: "true".to_string(),
            tasks_json: r#"[{"id":"t1","name":"Noop","function":{"name":"log","input":{"message":"test"}}}]"#
                .to_string(),
            tags_json: "[]".to_string(),
            loop_json: None,
            continue_on_error: false,
            created_at: chrono::NaiveDateTime::default(),
            updated_at: chrono::NaiveDateTime::default(),
        };

        let json = workflow_json(
            &workflow,
            "default",
            "rollout-wf:v3",
            "active",
            Some((0, 50)),
        )
        .expect("test");
        let parsed = strict_parse(&json);
        let df_workflow = parsed.def.expect("parses");
        assert_eq!(df_workflow.id(), "rollout-wf:v3");
        assert_eq!(df_workflow.channel(), "default");

        // The author's condition is passed through untouched. It used to be
        // wrapped in an `and` with two synthetic bucket comparisons against a
        // key the ingress injected into `data`; the range is a typed field the
        // engine gates on before it builds an arena.
        assert_eq!(json["condition"], serde_json::json!(true));
        let rollout = df_workflow.rollout().expect("rollout range");
        assert_eq!((rollout.bucket_start, rollout.bucket_end), (0, 50));
        assert!(rollout.accepts(0) && rollout.accepts(49));
        assert!(!rollout.accepts(50) && !rollout.accepts(99));
    }

    /// The `continue_on_error` column is spelled as dataflow-rs 4
    /// reads it, `"on_failure": "continue"` when set and absent when not
    /// (the default). A task's own keys are the author's and are left as
    /// written.
    #[test]
    fn workflow_json_writes_on_failure_from_the_column() {
        let mut workflow = Workflow {
            workflow_id: "wf".to_string(),
            name: "WF".to_string(),
            description: None,
            priority: 3,
            version: 2,
            status: EntityStatus::Active.as_str().to_string(),
            rollout_percentage: 100,
            condition_json: "true".to_string(),
            tasks_json: r#"[{"id":"t","name":"t","on_failure":"continue",
                "function":{"name":"log","input":{"message":"x"}}}]"#
                .to_string(),
            tags_json: r#"["a"]"#.to_string(),
            loop_json: None,
            continue_on_error: true,
            created_at: chrono::NaiveDateTime::default(),
            updated_at: chrono::NaiveDateTime::default(),
        };

        let json = workflow_json(&workflow, "orders", "wf", "active", None).expect("test");
        assert_eq!(json["on_failure"], "continue");
        assert!(json.get("continue_on_error").is_none(), "{json}");
        assert_eq!(json["tasks"][0]["on_failure"], "continue");
        assert_eq!(json["channel"], "orders");
        assert_eq!(
            (json["priority"].clone(), json["version"].clone()),
            (3.into(), 2.into())
        );
        assert!(json["rollout"].is_null() && json["loop"].is_null());

        workflow.continue_on_error = false;
        let json =
            workflow_json(&workflow, "orders", "wf:v2", "active", Some((20, 70))).expect("test");
        assert!(json.get("on_failure").is_none(), "{json}");
        assert_eq!(json["id"], "wf:v2");
        assert_eq!(
            json["rollout"],
            serde_json::json!({"bucket_start": 20, "bucket_end": 70})
        );

        // The engine reads the row with nothing to report.
        let parsed = strict_parse(&json);
        assert!(parsed.def.is_some(), "{:?}", parsed.issues);
        assert!(parsed.issues.is_empty(), "{:?}", parsed.issues);
    }

    /// The plain converter must leave `rollout` unset — a workflow with no
    /// range takes all of its channel's traffic, which is what the single
    /// version at 100% case means.
    #[test]
    fn test_workflow_json_has_no_rollout_range() {
        let workflow = Workflow {
            workflow_id: "wf-orders".to_string(),
            name: "Orders".to_string(),
            description: None,
            priority: 1,
            version: 1,
            status: EntityStatus::Active.as_str().to_string(),
            rollout_percentage: 100,
            condition_json: "true".to_string(),
            tasks_json: r#"[{"id":"t","function":{"name":"log","input":{"message":"x"}}}]"#
                .to_string(),
            tags_json: "[]".to_string(),
            loop_json: None,
            continue_on_error: false,
            created_at: chrono::NaiveDateTime::default(),
            updated_at: chrono::NaiveDateTime::default(),
        };
        let json = workflow_json(&workflow, "orders", "wf-orders", "active", None).expect("test");
        let parsed = strict_parse(&json);
        assert_eq!(parsed.def.expect("parses").rollout(), None);
    }
}
