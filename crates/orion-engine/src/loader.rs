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
/// `crate::ERROR_CONTEXT`). A reload that keeps the registry builds
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
            .error_context(crate::ERROR_CONTEXT.clone())
            .with_condition_error_policy(condition_policy)
            .observer(std::sync::Arc::new(crate::MetricsObserver)),
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
/// quarantines the channel; registered in the route table with no workflow
/// behind it, it would answer every request with an opaque engine error.
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
    // ranges start past bucket 99 and are unreachable. `Rollout::partition`
    // is the engine's own arithmetic for exactly this, error direction
    // included, so the invariant is stated once — in the crate that
    // routes on it.
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
/// author's and are left as written: no function input has an alias, so a
/// row is read as it is stored.
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
#[cfg(any(test, feature = "test-support"))]
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

    impl crate::NamedFunction for NoFunction {
        fn name(&self) -> &str {
            unreachable!("never registered")
        }
    }

    /// Every Orion function over inert dependencies, as a generation builds
    /// its registry.
    pub struct Engines {
        pub compiler: Compiler,
        plugins: NoIssueNotes,
        _offline: crate::kit::OfflineDeps,
    }

    impl Engines {
        pub fn orion() -> Self {
            Self::orion_with::<NoFunction>(crate::kit::test_exprs(), Vec::new())
        }

        /// Every Orion function on `exprs`, and each plugin function.
        pub fn orion_with<F: crate::NamedFunction>(
            exprs: dataflow_rs::expr::ExprEngine,
            plugins: Vec<F>,
        ) -> Self {
            let offline = crate::kit::OfflineDeps::new();
            let mut registry = dataflow_rs::Registry::new(exprs);
            crate::register_functions(&mut registry, offline.deps());
            crate::register_plugin_functions(&mut registry, plugins);
            Self {
                compiler: registry.compiler(dataflow_rs::program::CompileOptions::default()),
                plugins: NoIssueNotes,
                _offline: offline,
            }
        }

        /// The built-ins alone, which is all the `log` fixtures need.
        pub fn bare() -> Self {
            let registry = dataflow_rs::Registry::new(crate::kit::test_exprs());
            Self {
                compiler: registry.compiler(dataflow_rs::program::CompileOptions::default()),
                plugins: NoIssueNotes,
                _offline: crate::kit::OfflineDeps::new(),
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
        crate::operators::orion_exprs(&crate::ResolvedSecrets::from_values(values), 0)
            .expect("Orion's expressions")
    }
}

#[cfg(test)]
#[path = "loader_tests.rs"]
mod tests;

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

        // The author's condition is passed through untouched: the rollout
        // range is a typed field the engine gates on before it builds an
        // arena, not a bucket comparison folded into the condition.
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
