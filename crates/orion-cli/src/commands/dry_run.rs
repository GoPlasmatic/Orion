use super::authoring::{
    Catalog, OfflinePluginHandlers, format_lint_error, offline_registry, read_expanded_workflow,
    unverifiable_functions,
};

/// Dry-run a workflow against a JSON input file.
///
/// Boots an in-process engine with just the supplied workflow, then prints
/// the per-task execution trace from dataflow_rs.
///
/// Connector-backed tasks (`http_call`, `db_read`, `data_query`,
/// `channel_call`, …) are answered from `--stubs`; nothing reaches a real
/// backend. Without a stub file such a task fails naming the stub that
/// would satisfy it, so a workflow is never silently half-run.
#[derive(clap::Args)]
pub struct DryRunCmd {
    /// Path to a workflow JSON file.
    #[arg(short, long)]
    pub workflow: String,
    /// Path to a JSON file used as the message payload.
    #[arg(short, long)]
    pub input: String,
    /// Path to a JSON file used as the message metadata — `headers`,
    /// `params`, `query`, `cookies`, `auth.claims`, `channel`, as the HTTP
    /// ingress would have built them. Header keys are lowercased and
    /// credential headers masked, so an offline run sees what production
    /// would.
    #[arg(short, long)]
    pub metadata: Option<String>,
    /// Path to a JSON file of canned connector responses:
    /// `{"http_call": {"crm": {...}}, "db_read": {"*": [...]}}`.
    /// The inner key is the task's `connector` (or `channel` for
    /// `channel_call`); `"*"` matches any.
    #[arg(short, long)]
    pub stubs: Option<String>,
    /// Path to a JSON file of stand-in values for the
    /// `{"secret": "name"}` references the workflow reads:
    /// `{"partner_hmac": "test-key"}`. An offline run has no `[secrets]`
    /// config to resolve, and an engine with no store refuses a workflow
    /// that names a secret. Values are used verbatim — use throwaway ones.
    #[arg(long)]
    pub secrets: Option<String>,
    /// Directory holding the set's shared definitions — the `constants`,
    /// `errors` and `fragments` documents a `$from` or a `use` resolves
    /// against. Expansion happens before validation, so what is checked
    /// and run is the expanded form.
    #[arg(long, value_name = "DIR")]
    pub definitions: Option<String>,
    /// Directory of plugin manifests and their components. A plugin
    /// function runs for real in the sandbox, never stubbed; a workflow
    /// naming one whose component is absent fails as
    /// PLUGIN_ARTIFACT_UNAVAILABLE. Repeatable.
    #[arg(long = "plugin-dir", value_name = "DIR")]
    pub plugin_dirs: Vec<String>,
    /// How much of the run to record: `full` (every step with a message
    /// snapshot and its changes), `steps` (ids, timing and changes, no
    /// snapshots) or `none` (no trace and no per-write capture, as a node
    /// runs an untraced message). `tasks` lists what ran in every mode.
    #[arg(long, value_enum, default_value = "full")]
    pub trace: DryRunTrace,
}

impl DryRunCmd {
    pub async fn run(&self) -> anyhow::Result<i32> {
        run_dry_run(DryRunRequest {
            workflow: &self.workflow,
            input: &self.input,
            stubs: self.stubs.as_deref(),
            metadata: self.metadata.as_deref(),
            secrets: self.secrets.as_deref(),
            definitions: self.definitions.as_deref(),
            plugin_dirs: &self.plugin_dirs,
            trace: self.trace,
        })
        .await
        .map_err(super::authoring::boxed)?;
        Ok(0)
    }
}

/// Run a directory of workflow test cases.
///
/// Each `*.case.json` case names a workflow, an input, optional connector stubs
/// and the values expected in the output:
///
///     {"name": "flags high-value orders", "workflow": "wf.json",
///      "input": {...}, "stubs": {...},
///      "expect": {"data.order.flagged": true}}
///
/// Paths inside a case are resolved relative to the case file. Prints a
/// per-case diff and exits non-zero on any failure, so it gates CI the way
/// `lint` and `validate-config` already do.
#[derive(clap::Args)]
pub struct TestCmd {
    /// Directory of case files, or a single case file.
    pub path: String,
    /// Directory holding the set's shared definitions — the `constants`,
    /// `errors` and `fragments` documents a `$from` or a `use` resolves
    /// against. Expansion happens before validation, so what is checked
    /// and run is the expanded form.
    #[arg(long, value_name = "DIR")]
    pub definitions: Option<String>,
    /// Directory of plugin manifests and their components, loaded once
    /// for the whole suite. A plugin function runs for real; a case
    /// naming one whose component is absent fails as
    /// PLUGIN_ARTIFACT_UNAVAILABLE. Repeatable.
    #[arg(long = "plugin-dir", value_name = "DIR")]
    pub plugin_dirs: Vec<String>,
}

impl TestCmd {
    pub async fn run(&self) -> anyhow::Result<i32> {
        run_test(&self.path, self.definitions.as_deref(), &self.plugin_dirs)
            .await
            .map_err(super::authoring::boxed)?;
        Ok(0)
    }
}

/// Build a dry-run engine over one workflow file, with connector-backed
/// functions answered from `stubs_path` instead of from real backends.
///
/// Shared by [`run_dry_run`] and the `test` runner, which differ only in what
/// they do with the result.
pub(crate) fn build_dry_run_engine(
    workflow_path: &str,
    stubs_path: Option<&str>,
    definitions: Option<&Catalog>,
    secrets: &orion_engine::ResolvedSecrets,
) -> Result<OfflineRun, Box<dyn std::error::Error>> {
    let stubs = match stubs_path {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| format!("Failed to read stubs '{path}': {e}"))?;
            orion_engine::functions::stub::parse_stubs(&raw, path)?
        }
        None => orion_engine::functions::stub::StubTable::new(),
    };
    build_dry_run_engine_with_stubs(workflow_path, stubs, definitions, secrets)
}

/// Read a `--secrets` / case-file secrets document into a store.
///
/// The values are **stand-ins**, used verbatim: an offline run has no config
/// file and no reason to reach a real vault, and a suite that depends on the
/// machine's environment is a suite that passes on one laptop. Use throwaway
/// values, and keep the file out of the repository if any of them is not.
pub(crate) fn offline_secrets(
    value: &serde_json::Value,
    source: &str,
) -> Result<orion_engine::ResolvedSecrets, String> {
    match value {
        serde_json::Value::Object(map) => {
            // Checked here rather than left to the first task that reads one.
            // `ResolvedSecrets::resolve` always produces strings, so this is
            // the only store that can hold a shape the reader rejects — and
            // that rejection arrives as a task error inside a `*.case.json`
            // run, where the case name is the only context the author gets.
            for (name, value) in map {
                if !value.is_string() {
                    return Err(format!(
                        "{source}: secrets.{name} must be a string, got {}",
                        orion_engine::utils::json_kind(value)
                    ));
                }
            }
            Ok(orion_engine::ResolvedSecrets::from_values(map.clone()))
        }
        other => Err(format!(
            "{source}: secrets must be a JSON object of name -> value, got {}",
            orion_engine::utils::json_kind(other)
        )),
    }
}

/// The channel an offline run's workflow is compiled and run on.
const DRY_RUN_CHANNEL: &str = "__dry_run__";

/// Everything an offline run needs beyond the engine itself.
pub(crate) struct OfflineRun {
    pub engine: dataflow_rs::Engine,
    /// The connector calls the run makes, filled in as it runs, each one
    /// labelled with the task that made it.
    pub log: std::sync::Arc<orion_engine::functions::stub::CallLog>,
    /// The tasks the run dispatched, in order.
    pub tasks: std::sync::Arc<TaskRecorder>,
    /// The stubs, around every host call.
    layers: Vec<std::sync::Arc<dyn dataflow_rs::function::CallLayer>>,
}

impl OfflineRun {
    /// Run the workflow once over `input`, from the message a node builds.
    async fn run(
        &self,
        input: &serde_json::Value,
        metadata: &serde_json::Value,
        trace: Option<orion_engine::offline::OfflineTrace>,
        capture_changes: bool,
    ) -> orion_engine::offline::OfflineResult {
        orion_engine::offline::run_single(
            &self.engine,
            input,
            metadata,
            orion_engine::offline::OfflineRun {
                trace,
                max_snapshot_bytes: 0,
                capture_changes,
                layers: &self.layers,
                observer: Some(&*self.tasks),
            },
        )
        .await
    }
}

/// The ids of the tasks an offline run dispatched, in dispatch order: once per
/// sweep of a loop and once per `for_each` element.
///
/// An engine observer rather than the execution trace, so a run can say which
/// tasks ran without the trace's cost. A trace snapshots the message at every
/// step; a long loop made that tens of gigabytes where this is one id per
/// task run (#353). A task whose condition is false is not dispatched and is
/// not reported, like a workflow whose condition is false.
#[derive(Default)]
pub(crate) struct TaskRecorder {
    tasks: std::sync::Mutex<Vec<String>>,
}

impl TaskRecorder {
    /// The ids recorded so far, leaving the recorder empty.
    pub fn take(&self) -> Vec<String> {
        self.tasks
            .lock()
            .map(|mut tasks| std::mem::take(&mut *tasks))
            .unwrap_or_default()
    }
}

impl dataflow_rs::Observer for TaskRecorder {
    fn interest(&self) -> dataflow_rs::observe::Interest {
        dataflow_rs::observe::Interest::TASKS
    }

    fn on(&self, event: &dataflow_rs::observe::Event<'_>) {
        if let dataflow_rs::observe::Event::TaskFinished { info, .. } = event
            && let Ok(mut tasks) = self.tasks.lock()
        {
            tasks.push(info.task_id.to_string());
        }
    }
}

/// [`build_dry_run_engine`] over an already-parsed stub table, for the `test`
/// runner — whose stubs may be inline in the case file rather than on disk.
pub(crate) fn build_dry_run_engine_with_stubs(
    workflow_path: &str,
    stubs: orion_engine::functions::stub::StubTable,
    definitions: Option<&Catalog>,
    secrets: &orion_engine::ResolvedSecrets,
) -> Result<OfflineRun, Box<dyn std::error::Error>> {
    use orion_authoring::definitions::documents::CreateWorkflowRequest;
    use orion_engine::workflow_json;

    let doc = read_expanded_workflow(workflow_path, definitions)?;
    let req: CreateWorkflowRequest = serde_json::from_value(doc)
        .map_err(|e| format!("'{workflow_path}' is not a valid workflow JSON: {e}"))?;
    // Validated against the catalog's manifests, so a plugin task's input is
    // checked here as the server checks it. A plugin function no manifest
    // accounts for cannot run offline at all — there is nothing to load —
    // and is refused by name rather than reaching the engine as unknown.
    let registry = offline_registry(definitions)?;
    let manifests = definitions.map(|c| c.plugins.as_slice()).unwrap_or(&[]);
    let unverifiable =
        unverifiable_functions(&req.tasks, req.loop_config.as_ref(), &registry, manifests);
    if let Some(name) = unverifiable.first() {
        return Err(format!(
            "PLUGIN_ARTIFACT_UNAVAILABLE: '{workflow_path}' names plugin function '{name}', \
             and no manifest for its plugin was given — an offline run executes plugin \
             functions for real, so pass --plugin-dir <dir> holding the plugin's plugin.toml \
             and its component"
        )
        .into());
    }
    orion_authoring::validation::validate_create_workflow(
        &req,
        orion_common::config::EngineConfig::default().max_loop_iterations,
        &registry,
    )
    .map_err(|e| format_lint_error(workflow_path, e))?;

    // Build a synthetic Workflow row from the request to reuse the
    // existing dataflow conversion. Version + timestamps are placeholders.
    let synthetic = orion_authoring::definitions::documents::synthetic_workflow(
        &req,
        req.workflow_id.as_deref().unwrap_or("dry-run"),
    )?;
    let json = workflow_json(
        &synthetic,
        DRY_RUN_CHANNEL,
        &synthetic.workflow_id,
        "active",
        None,
    )?;

    // The stub layer answers every connector-backed call, even with no stub
    // file: an unstubbed call then reports which stub to add.
    let log = std::sync::Arc::new(orion_engine::functions::stub::CallLog::new());
    // Plugin functions are never stubbed: a plugin is capability-free, so the
    // real component runs, exactly as `crypto` does. A manifest whose
    // component is not beside it has functions this run cannot execute, and
    // a workflow naming one fails here by name rather than as a missing
    // function at compile.
    let mut plugin_functions = Vec::new();
    if let Some(catalog) = definitions {
        let OfflinePluginHandlers {
            handlers,
            unavailable,
        } = catalog.plugin_handlers()?;
        for task in orion_engine::leaf_tasks(&req.tasks, req.loop_config.as_ref()) {
            let Some(name) = task
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            if let Some((_, origin)) = unavailable.iter().find(|(f, _)| f == name) {
                return Err(format!(
                    "PLUGIN_ARTIFACT_UNAVAILABLE: '{workflow_path}' names plugin function \
                     '{name}', whose manifest ({origin}) has no component beside it — build \
                     the component, or name it with `component = …`, so the run can execute \
                     it rather than stub it"
                )
                .into());
            }
        }
        plugin_functions = handlers;
    }
    // Compiled on the real functions, over inert dependencies, as a node
    // compiles a row: a workflow a node would quarantine is refused here, and
    // the run speaks the serving engine's expressions. Unbounded: an offline
    // run has no `[engine]` config to read a budget from.
    let compiler = orion_runtime::runtime::functions::offline_compiler(secrets, plugin_functions)?;
    let engine = orion_engine::offline::single_engine(&compiler, &json)?;
    let layers: Vec<std::sync::Arc<dyn dataflow_rs::function::CallLayer>> =
        vec![std::sync::Arc::new(
            orion_engine::functions::stub::StubLayer::new(stubs, log.clone()),
        )];
    Ok(OfflineRun {
        engine,
        log,
        tasks: std::sync::Arc::new(TaskRecorder::default()),
        layers,
    })
}

/// Dry-run a workflow against an input JSON file.
///
/// Connector-backed tasks are answered from the stub file when one is supplied
/// (see [`orion_engine::functions::stub`]); without one, any such task fails
/// naming the stub that would satisfy it. Nothing reaches a real backend
/// either way — this is the offline counterpart to
/// `POST /definitions/test`, which runs against live connectors.
/// How much of a run `dry-run` records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum DryRunTrace {
    /// Every step, with a snapshot of the message after it and the changes it
    /// made. Each snapshot carries only its own step's audit entry, so the
    /// trace grows with the steps rather than with their square.
    Full,
    /// Every step's id, result, timing and changes, with no snapshots.
    Steps,
    /// No trace: the run as a node runs an untraced message, with no
    /// per-write capture. `tasks` still lists what ran.
    #[value(name = "none")]
    Off,
}

impl DryRunTrace {
    /// The trace to record, or `None` for none.
    fn options(self) -> Option<orion_engine::offline::OfflineTrace> {
        match self {
            Self::Full => Some(orion_engine::offline::OfflineTrace::Changes),
            Self::Steps => Some(orion_engine::offline::OfflineTrace::Steps),
            Self::Off => None,
        }
    }
}

pub(crate) struct DryRunRequest<'a> {
    pub(crate) workflow: &'a str,
    pub(crate) input: &'a str,
    pub(crate) stubs: Option<&'a str>,
    pub(crate) metadata: Option<&'a str>,
    pub(crate) secrets: Option<&'a str>,
    pub(crate) definitions: Option<&'a str>,
    pub(crate) plugin_dirs: &'a [String],
    pub(crate) trace: DryRunTrace,
}

pub(crate) async fn run_dry_run(req: DryRunRequest<'_>) -> Result<(), Box<dyn std::error::Error>> {
    let DryRunRequest {
        workflow: workflow_path,
        input: input_path,
        stubs: stubs_path,
        metadata: metadata_path,
        secrets: secrets_path,
        definitions,
        plugin_dirs,
        trace: trace_mode,
    } = req;
    let input_raw = std::fs::read_to_string(input_path)
        .map_err(|e| format!("Failed to read input '{input_path}': {e}"))?;
    let input: serde_json::Value = serde_json::from_str(&input_raw)
        .map_err(|e| format!("'{input_path}' is not valid JSON: {e}"))?;

    let metadata = match metadata_path {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| format!("Failed to read metadata '{path}': {e}"))?;
            serde_json::from_str(&raw)
                .map_err(|e| format!("'{path}' is not valid JSON: {e}"))
                .and_then(|v| {
                    orion_engine::utils::prepare_offline_metadata(v)
                        .map_err(|e| format!("'{path}': {e}"))
                })?
        }
        None => serde_json::json!({}),
    };

    let secrets = match secrets_path {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| format!("Failed to read secrets '{path}': {e}"))?;
            let value: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|e| format!("'{path}' is not valid JSON: {e}"))?;
            offline_secrets(&value, path)?
        }
        None => orion_engine::ResolvedSecrets::empty(),
    };

    let catalog = Catalog::load_opt(definitions, plugin_dirs)?;
    let run = build_dry_run_engine(workflow_path, stubs_path, catalog.as_ref(), &secrets)?;
    // Per-write capture is what the audit trail's `changes` report; without
    // a trace the run is a node's untraced run. The trace is kept on every
    // status, so a hard failure still prints the steps that ran: a dry run
    // that dies on task three and reports nothing is the least useful
    // possible answer to "what does this workflow do?".
    let trace_options = trace_mode.options();
    let result = run
        .run(&input, &metadata, trace_options, trace_options.is_some())
        .await;
    let message = &result.message;
    let tasks = run.tasks.take();

    // `output` is the data document under its historical name: CI `jq` filters
    // read it. The run's documents go in beside it under the names a case's
    // `expect` roots use, from the same builder the runner reads, so a path
    // lifted off a dry run addresses the same thing in a case.
    let mut output = serde_json::json!({
        "matched": !tasks.is_empty(),
        "tasks": tasks,
        "output": serde_json::Value::from(message.context().data()),
        "errors": serde_json::to_value(message.errors())?,
    });
    if let Some(trace) = &result.trace {
        output["trace"] = serde_json::to_value(trace)?;
    }
    for (name, document) in orion_engine::functions::stub::run_documents(message, &run.log) {
        output[name] = document;
    }
    if let Some(ref e) = result.error {
        output["error"] = serde_json::json!(e.to_string());
    }
    println!("{}", serde_json::to_string_pretty(&output)?);

    // The trace is on stdout either way; the exit status still reports the
    // failure, so `orion-cli dry-run` stays usable as a CI gate.
    match result.error {
        Some(e) => Err(e.into()),
        None => Ok(()),
    }
}

// ============================================================
// `test` — a regression suite for a team's own workflows
// ============================================================

/// One test case, as authored in a `*.json` file.
///
/// `workflow` and `stubs_file` are paths resolved **relative to the case
/// file**, so a suite directory is movable and a case can be read without
/// knowing where it will be run from.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TestCase {
    /// Human name, printed in the report. Defaults to the file stem.
    #[serde(default)]
    name: Option<String>,
    /// Path to the workflow JSON under test.
    workflow: String,
    /// The message payload.
    input: serde_json::Value,
    /// The message metadata, as the HTTP ingress would have built it:
    /// `headers`, `params`, `query`, `cookies`, `auth.claims`, `channel`,
    /// `http_method`, plus any caller-supplied keys.
    ///
    /// Normalized by [`orion_engine::utils::prepare_offline_metadata`] so an
    /// offline pass means the same thing as a production pass — header keys
    /// lowercased, credential headers masked.
    #[serde(default)]
    metadata: serde_json::Value,
    /// Inline connector stubs, in the same shape as `dry-run --stubs`.
    #[serde(default)]
    stubs: Option<serde_json::Value>,
    /// Stand-in values for the `{"secret": "name"}` references the workflow
    /// reads, in the same shape as `dry-run --secrets`.
    ///
    /// An offline run has no `[secrets]` config to resolve, and an engine
    /// built with no store refuses a workflow that names one — so a workflow
    /// that signs anything is untestable without this. The values are used
    /// verbatim; use throwaway ones.
    #[serde(default)]
    secrets: Option<serde_json::Value>,
    /// Path to a stub file, as an alternative to inline `stubs`.
    #[serde(default)]
    stubs_file: Option<String>,
    /// Dotted paths to their expected values, each **rooted** at one of
    /// [`RUN_DOCUMENTS`](orion_engine::functions::stub::RUN_DOCUMENTS):
    /// `data.order.flagged`, `metadata.headers.deviceid`,
    /// `temp_data.user_id`, `calls.http_call[0].input.body.id`,
    /// `audit_trail[1].status`.
    ///
    /// The root is required. Every other path in Orion — a `{"var": ..}` node,
    /// a `map` mapping's `path`, a connector filter — resolves over the same
    /// `{data, metadata, temp_data}` context, and every mapping path in every
    /// shipped workflow spells its root. A bare path was accepted here alone,
    /// and silently meant `data.`, so `metadata.foo` read the data document's
    /// own `metadata` key and reported absent.
    #[serde(default)]
    expect: std::collections::BTreeMap<String, serde_json::Value>,
    /// Expected task-error codes, in order. An empty vec (the default) asserts
    /// the run produced none — which is why it is checked even when the case
    /// does not mention it.
    #[serde(default)]
    expect_errors: Vec<String>,
    /// Expected connector calls per function, in execution order.
    ///
    /// Each entry is matched as a **deep subset** of the recorded call's
    /// resolved `input`, so a case names the fields it cares about and ignores
    /// the rest — but the number of entries must equal the number of recorded
    /// calls for that function, so an unexpected extra write fails. Only the
    /// functions named here are constrained; `"db_write": []` asserts that
    /// nothing was written.
    ///
    /// Unlike `expect`, presence is **strict**: a key in the expected object
    /// must be present in the record, so `"revokedAt": null` asserts *written
    /// as null* rather than *absent*. `expect` reads a data document, where
    /// JSONLogic makes a missing path and a null indistinguishable; a recorded
    /// payload is a literal document, where whether the field was written is
    /// the assertion.
    #[serde(default)]
    expect_calls: std::collections::BTreeMap<String, Vec<serde_json::Value>>,
    /// The ids of the tasks that ran, in order, matched exactly.
    ///
    /// `None` (the default) means unchecked. Unlike `expect_errors`, this
    /// cannot default to empty-and-checked: every workflow runs tasks, so that
    /// default would fail every case ever written.
    #[serde(default)]
    expect_tasks: Option<Vec<String>>,
}

/// What one case did.
struct CaseResult {
    name: String,
    failures: Vec<String>,
}

/// `test` subcommand: run every case under `path` and report.
pub(crate) async fn run_test(
    path: &str,
    definitions: Option<&str>,
    plugin_dirs: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let cases = collect_case_files(path)?;
    if cases.is_empty() {
        return Err(format!(
            "no test cases found under '{path}' (looking for *{CASE_SUFFIX}). \
             Name a case file explicitly to run one that does not follow the convention."
        )
        .into());
    }

    // Loaded once for the whole suite rather than per case: the catalog is
    // the same for all of them, and walking the tree per case was most of a
    // run's work on any set of size — and the plugin components it carries
    // are compiled once for every case that runs them.
    let catalog = Catalog::load_opt(definitions, plugin_dirs)?;

    let mut results = Vec::new();
    for case_path in &cases {
        results.push(run_case(case_path, catalog.as_ref()).await);
    }

    let failed: Vec<&CaseResult> = results.iter().filter(|r| !r.failures.is_empty()).collect();
    for result in &results {
        if result.failures.is_empty() {
            println!("  ok    {}", result.name);
        } else {
            println!("  FAIL  {}", result.name);
            for failure in &result.failures {
                println!("          {failure}");
            }
        }
    }
    println!(
        "\n{} passed, {} failed ({} case(s))",
        results.len() - failed.len(),
        failed.len(),
        results.len()
    );

    if failed.is_empty() {
        Ok(())
    } else {
        // Non-zero so a suite gates a deploy, the same way `lint` and
        // `validate-config` already do.
        Err(format!("{} test case(s) failed", failed.len()).into())
    }
}

/// Suffix that marks a file as a test case when scanning a directory.
pub(crate) const CASE_SUFFIX: &str = ".case.json";

/// Every `*.case.json` under `path`, or `path` itself when it names a file.
///
/// The suffix exists because a suite directory is the natural home for the
/// workflows and fixtures the cases reference. Treating every `*.json` as a
/// case reports the workflow under test as a broken case — noise that grows
/// with the size of the suite. Naming a file explicitly bypasses the
/// convention, since that is already unambiguous.
///
/// Sorted, so a report reads the same on every machine — a suite whose order
/// depends on directory iteration is a suite whose diffs are noise.
fn collect_case_files(path: &str) -> Result<Vec<std::path::PathBuf>, Box<dyn std::error::Error>> {
    let p = std::path::Path::new(path);
    if p.is_file() {
        return Ok(vec![p.to_path_buf()]);
    }
    if !p.is_dir() {
        return Err(format!("'{path}' is neither a file nor a directory").into());
    }
    let mut out: Vec<std::path::PathBuf> = std::fs::read_dir(p)
        .map_err(|e| format!("Failed to read '{path}': {e}"))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(CASE_SUFFIX))
        })
        .collect();
    out.sort();
    Ok(out)
}

/// Run one case file. Every failure is collected rather than returned early, so
/// one run reports everything wrong with a case instead of its first problem.
async fn run_case(case_path: &std::path::Path, definitions: Option<&Catalog>) -> CaseResult {
    let display = case_path.display().to_string();
    // `file_stem` on `orders.case.json` gives `orders.case`; strip the whole
    // convention suffix so the default name reads as the case, not the file.
    let stem = case_path
        .file_name()
        .and_then(|s| s.to_str())
        .map(|n| n.strip_suffix(CASE_SUFFIX).unwrap_or(n).to_string())
        .unwrap_or_else(|| display.clone());

    let fail = |name: &str, message: String| CaseResult {
        name: name.to_string(),
        failures: vec![message],
    };

    let raw = match std::fs::read_to_string(case_path) {
        Ok(raw) => raw,
        Err(e) => return fail(&stem, format!("cannot read case: {e}")),
    };
    let case: TestCase = match serde_json::from_str(&raw) {
        Ok(case) => case,
        Err(e) => return fail(&stem, format!("not a valid test case: {e}")),
    };
    let name = case.name.clone().unwrap_or(stem);

    // Relative to the case file, so a suite directory can be moved or invoked
    // from anywhere.
    let base = case_path.parent().unwrap_or(std::path::Path::new("."));
    let workflow_path = base.join(&case.workflow);
    let workflow_path = workflow_path.to_string_lossy().to_string();

    // Inline `stubs` and `stubs_file` are the same thing written two ways;
    // inline wins so a case can override a shared file. Inline stubs are already
    // a parsed value, so they go straight to the validator rather than through a
    // serialize-and-reparse.
    let stubs = match (&case.stubs, &case.stubs_file) {
        (Some(inline), _) => orion_engine::functions::stub::parse_stub_value(inline, "stubs"),
        (None, Some(file)) => match std::fs::read_to_string(base.join(file)) {
            Ok(raw) => orion_engine::functions::stub::parse_stubs(&raw, file),
            Err(e) => Err(format!("cannot read stubs '{file}': {e}")),
        },
        (None, None) => Ok(orion_engine::functions::stub::StubTable::new()),
    };
    let stubs = match stubs {
        Ok(stubs) => stubs,
        Err(e) => return fail(&name, e),
    };

    // Rooting is checked before the workflow runs: a path naming no root can
    // never match anything, and saying so after a full run — as an `<absent>`
    // diff — buries the one fact the author needs.
    let unrooted: Vec<String> = case
        .expect
        .keys()
        .filter(|path| !orion_engine::functions::stub::is_rooted(path))
        .map(|path| unrooted_message(path))
        .collect();
    if !unrooted.is_empty() {
        return CaseResult {
            name,
            failures: unrooted,
        };
    }

    let metadata = match orion_engine::utils::prepare_offline_metadata(case.metadata.clone()) {
        Ok(metadata) => metadata,
        Err(e) => return fail(&name, e),
    };

    let secrets = match case.secrets.as_ref() {
        Some(value) => match offline_secrets(value, &name) {
            Ok(secrets) => secrets,
            Err(e) => return fail(&name, e),
        },
        None => orion_engine::ResolvedSecrets::empty(),
    };

    let run = match build_dry_run_engine_with_stubs(&workflow_path, stubs, definitions, &secrets) {
        Ok(run) => run,
        Err(e) => return fail(&name, e.to_string()),
    };

    // Run the way a node runs an untraced message: no execution trace, and
    // no per-write capture unless the case reads it. A trace snapshots the
    // whole message at every step, each snapshot carrying the audit trail so
    // far, so a looping workflow's test cost memory quadratic in its steps; a
    // 1000-sweep case needed more than 80 GB (#353). Nothing a case asserts
    // needs the trace: `expect_tasks` reads the recorder. Capture fills
    // `audit_trail[i].changes`, the one document it feeds, and is linear in
    // the writes, so it stays on for a case that roots a path there.
    let capture = case
        .expect
        .keys()
        .any(|path| orion_engine::functions::stub::path_root(path) == "audit_trail");
    let result = run.run(&case.input, &metadata, None, capture).await;
    let message = &result.message;
    let run_error = result.error;

    let mut failures = Vec::new();
    if let Some(e) = run_error {
        // A case that names the codes it expects is asserting the failure, and
        // a task error that halts the workflow is exactly how a refusal looks
        // from here — reporting the run's `Err` on top of that would make the
        // negative path unwritable. The codes are still matched below, so a
        // case cannot pass by declaring the wrong ones (or none at all).
        if case.expect_errors.is_empty() {
            failures.push(format!("workflow failed: {e}"));
        }
    }

    let roots = serde_json::Value::Object(orion_engine::functions::stub::run_documents(
        message, &run.log,
    ));
    for (path, expected) in &case.expect {
        let actual = lookup_path(&roots, path);
        // An expected `null` matches an absent path as well as an explicit
        // null. JSONLogic resolves a missing `var` to null, so that is already
        // what the workflow sees; making a case distinguish the two would be a
        // distinction the runtime does not draw. (`expect_calls` is strict
        // instead — see its doc comment for why the two differ.)
        let matched = match actual {
            None => expected.is_null(),
            Some(ref actual) => actual == expected,
        };
        if !matched {
            // The diff is the whole value of the runner: "expected X, got Y at
            // this path" is what a bare pass/fail makes you go and find.
            failures.push(format!(
                "{path}: expected {expected}, got {}",
                actual.map_or("<absent>".to_string(), |v| v.to_string())
            ));
        }
    }

    failures.extend(check_expected_calls(&case.expect_calls, &run.log));

    if let Some(ref expected) = case.expect_tasks {
        let actual = executed_task_ids(run.tasks.take(), message);
        if &actual != expected {
            failures.push(format!("tasks: expected {expected:?}, ran {actual:?}"));
        }
    }

    let actual_errors: Vec<String> = message.errors().iter().map(|e| e.code.clone()).collect();
    if actual_errors != case.expect_errors {
        failures.push(format!(
            "task errors: expected {:?}, got {:?}",
            case.expect_errors, actual_errors
        ));
    }

    CaseResult { name, failures }
}

/// The ids of the tasks that ran, in dispatch order.
///
/// From the [`TaskRecorder`] rather than the audit trail, which cannot tell
/// "skipped by condition" from "not in the workflow" — and which branch ran
/// is the whole question `expect_tasks` exists to answer.
///
/// The message's errors are a second source, for an error that names a task
/// the engine never dispatched. A task that was dispatched and failed is
/// already recorded, in the position it ran.
fn executed_task_ids(mut ids: Vec<String>, message: &dataflow_rs::Message) -> Vec<String> {
    for id in message.errors().iter().filter_map(|e| e.task_id.as_deref()) {
        if !ids.iter().any(|seen| seen == id) {
            ids.push(id.to_string());
        }
    }
    ids
}

/// The failure text for an `expect` path with no root, suggesting the fix.
///
/// `data.` is suggested because it is what almost every unrooted path
/// intends.
fn unrooted_message(path: &str) -> String {
    format!(
        "expect path '{path}' has no root — did you mean 'data.{path}'? \
         roots: {}",
        orion_engine::functions::stub::RUN_DOCUMENTS.join(", ")
    )
}

/// One segment of an `expect` path.
enum Segment<'a> {
    Key(&'a str),
    Index(usize),
}

/// Split a dotted path into segments, accepting both `calls.http_call[0].input`
/// and `calls.http_call.0.input` for an array position.
fn path_segments(path: &str) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    for part in path.split('.') {
        let (head, mut rest) = match part.find('[') {
            Some(i) => (&part[..i], &part[i..]),
            None => (part, ""),
        };
        if let Ok(index) = head.parse::<usize>() {
            out.push(Segment::Index(index));
        } else if !head.is_empty() {
            out.push(Segment::Key(head));
        }
        // Trailing `[n]` groups, however many.
        while let Some(close) = rest.find(']') {
            if let Ok(index) = rest[1..close].parse::<usize>() {
                out.push(Segment::Index(index));
            }
            rest = &rest[close + 1..];
        }
    }
    out
}

/// Read a rooted, optionally array-indexed path out of the run's documents.
fn lookup_path(roots: &serde_json::Value, path: &str) -> Option<serde_json::Value> {
    path_segments(path)
        .into_iter()
        .try_fold(roots, |acc, segment| match segment {
            Segment::Key(key) => acc.get(key),
            Segment::Index(i) => acc.get(i),
        })
        .cloned()
}

/// Check a case's `expect_calls` against what the run recorded.
fn check_expected_calls(
    expected: &std::collections::BTreeMap<String, Vec<serde_json::Value>>,
    log: &orion_engine::functions::stub::CallLog,
) -> Vec<String> {
    if expected.is_empty() {
        return Vec::new();
    }
    let recorded = log.calls();
    let mut failures = Vec::new();
    for (function, expected_calls) in expected {
        let actual: Vec<&orion_engine::functions::stub::RecordedCall> = recorded
            .iter()
            .filter(|call| call.function == function.as_str())
            .collect();
        if actual.len() != expected_calls.len() {
            failures.push(format!(
                "calls.{function}: expected {} call(s), recorded {}",
                expected_calls.len(),
                actual.len()
            ));
            continue;
        }
        for (i, want) in expected_calls.iter().enumerate() {
            failures.extend(subset_mismatch(
                want,
                &actual[i].input,
                &format!("calls.{function}[{i}].input"),
            ));
        }
    }
    failures
}

/// Every way `actual` fails to contain `expected`, as diff lines rooted at
/// `path`.
///
/// Objects match as subsets — a key `expected` does not mention is not
/// checked — but a key it *does* mention must be **present**, which is what
/// makes `"revokedAt": null` assert that the field was written. Arrays match
/// element for element, including length: "these two documents were inserted"
/// is an assertion a subset rule could not express.
fn subset_mismatch(
    expected: &serde_json::Value,
    actual: &serde_json::Value,
    path: &str,
) -> Vec<String> {
    match (expected, actual) {
        (serde_json::Value::Object(want), serde_json::Value::Object(got)) => want
            .iter()
            .flat_map(|(key, want_value)| match got.get(key) {
                Some(got_value) => subset_mismatch(want_value, got_value, &format!("{path}.{key}")),
                None => vec![format!("{path}.{key}: expected {want_value}, not written")],
            })
            .collect(),
        (serde_json::Value::Array(want), serde_json::Value::Array(got))
            if want.len() == got.len() =>
        {
            want.iter()
                .zip(got)
                .enumerate()
                .flat_map(|(i, (want_value, got_value))| {
                    subset_mismatch(want_value, got_value, &format!("{path}[{i}]"))
                })
                .collect()
        }
        _ if expected == actual => Vec::new(),
        _ => vec![format!("{path}: expected {expected}, got {actual}")],
    }
}
