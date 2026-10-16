//! The quarantine differential (I182): dataflow-rs 3's load verdict against
//! dataflow-rs 4's, over every workflow definition in Orion's repository.
//!
//! The building cutover hands the verdict to dataflow-rs 4: every stored row
//! is compiled on it first, and its refusal quarantines the channel. That is
//! a behaviour change exactly where the two engines disagree, so the
//! disagreements are listed here, each with the break that names it. A
//! difference with no row fails the test; so does a row that no longer
//! matches a difference, so the table stays the account of what moved.
//!
//! **The corpus** is every workflow definition under `examples/`,
//! `tests/e2e/`, `crates/orion-server/tests/benchmark/fixtures/` and
//! `crates/orion-server/tests/fixtures/`, each in the canonical form a row is
//! stored in:
//!
//! - a definition set (each example package, each clippy and clippy-fix
//!   fixture directory) loads through `DefinitionSet::from_directory`, which
//!   expands its fragments and shared values with the set's own catalog, as
//!   `orion-server compile` does before a package is applied;
//! - every other file is source form on its own, and goes through
//!   `definitions::compile` with no catalog: a workflow document, each
//!   element of a bulk-import array, and each inline entry of a document's
//!   `workflows` (an e2e case, an artifact). One that still needs a catalog
//!   to compile, or that the create request does not read (no `name`), is
//!   not a row anything can store, and is counted apart.
//!
//! **The verdicts** are each engine's at load, as the generation reaches
//! them. dataflow-rs 3 screens through `engine::build_single`, which screens
//! as boot does, over today's handlers from `OfflineDeps` with the examples'
//! plugins loaded. dataflow-rs 4 compiles with `compile_row` on a `Registry`
//! from `register_functions` over the same `OfflineDeps`, the same plugins
//! registered, after the loader's alias rewrite. Both run on Orion's
//! expressions with every secret the corpus names declared, so neither
//! refuses a row for the environment this test builds.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dataflow_rs::Issue;
use dataflow_rs::program::{CompileOptions, Compiler, IssueCode, Severity};
use orion::definitions::{DefinitionSet, Entity, SharedDefinitions};
use orion::engine::{FunctionRegistry, ResolvedSecrets};
use orion::storage::repositories::workflows::{
    CreateWorkflowRequest, synthetic_workflow, workflow_json, workflow_to_dataflow,
};
use serde_json::Value;

/// The repository root.
const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

/// The trees the corpus is read from.
const TREES: &[&str] = &[
    "examples",
    "tests/e2e",
    "crates/orion-server/tests/benchmark/fixtures",
    "crates/orion-server/tests/fixtures",
];

/// The definition sets among them: each holds its own fragments and shared
/// values, which its workflows resolve against.
const SET_PARENTS: &[(&str, usize)] = &[
    // Each package is a set.
    ("examples/packages", 1),
    // `<rule>/fires`, `<rule>/quiet`.
    ("crates/orion-server/tests/fixtures/clippy", 2),
    // `<case>/before`, `<case>/after`.
    ("crates/orion-server/tests/fixtures/clippy-fix", 2),
];

/// The plugins the corpus calls: the examples' two, and the test fixture the
/// plugin benchmark's workflow names.
const PLUGINS: &[&str] = &[
    "examples/packages/fixed-width-statement/plugin.toml",
    "examples/packages/c4-tournament/plugin.toml",
    "crates/orion-server/tests/fixtures/plugins/fixture.toml",
];

/// The channel every row is loaded into. The verdict does not depend on it.
const CHANNEL: &str = "differential";

// -- The exception rows --

/// Why the two engines disagree about a definition: a break the plan names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
enum Why {
    /// A near miss of a known key, a `LIKELY_TYPO` Defect that quarantines.
    Break2,
    /// An unknown key in a host function's input, Rejected by its config.
    Break2a,
    /// A result that is never an object, written at a literal context root:
    /// I185's Defect.
    Break21,
    /// `name` optional on every node: dataflow-rs 4 loads what dataflow-rs 3
    /// refused.
    Break6,
    /// A field `schema.rs` marks required, missing (I183).
    RequiredField,
    /// An alias and its canonical key together (I186).
    I186,
}

/// One definition the engines disagree about, and why.
struct Exception {
    /// The file, relative to the repository root.
    file: &'static str,
    /// The step the difference sits in: `tasks[1]`, `loop.setup[0]`.
    task: &'static str,
    why: Why,
}

/// Every difference between the two verdicts over the corpus.
const EXCEPTIONS: &[Exception] = &[
    // `db_read`'s `limit`, a key its input does not have: 1.12 ignored it
    // (`deny_unknown` false for the connector functions), which is the
    // finding the clippy rule this fixture fires exists to report.
    Exception {
        file: "crates/orion-server/tests/fixtures/clippy/correctness.unknown_input_key/fires/w.json",
        task: "tasks[1]",
        why: Why::Break2a,
    },
];

// -- The corpus --

/// One workflow definition, as a row would store it.
struct Definition {
    /// The file it is read from, relative to the repository root.
    file: String,
    /// Where in the file: empty for a whole document, `[2]` for an element
    /// of an array, `workflows[0]` for an inline entry.
    at: String,
    doc: Value,
}

impl Definition {
    fn label(&self) -> String {
        if self.at.is_empty() {
            self.file.clone()
        } else {
            format!("{} {}", self.file, self.at)
        }
    }
}

fn root() -> PathBuf {
    std::fs::canonicalize(ROOT).expect("the repository root")
}

fn relative(root: &Path, path: &str) -> String {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
    path.strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

/// The directories of every definition set.
fn set_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for (parent, depth) in SET_PARENTS {
        let mut level = vec![root.join(parent)];
        for _ in 0..*depth {
            level = level
                .iter()
                .flat_map(|dir| {
                    let mut children: Vec<PathBuf> = std::fs::read_dir(dir)
                        .expect("a set parent")
                        .filter_map(Result::ok)
                        .map(|e| e.path())
                        .filter(|p| p.is_dir())
                        .collect();
                    children.sort();
                    children
                })
                .collect();
        }
        dirs.extend(level);
    }
    dirs
}

/// Whether `value` is a workflow definition: an object with a `tasks` list.
fn is_workflow(value: &Value) -> bool {
    value.get("tasks").is_some_and(Value::is_array)
}

/// The workflow definitions a standalone file holds, with where each sits.
fn documents_in(doc: Value) -> Vec<(String, Value)> {
    match doc {
        Value::Object(_) if is_workflow(&doc) => vec![(String::new(), doc)],
        Value::Object(mut object) => match object.remove("workflows") {
            Some(Value::Array(entries)) => entries
                .into_iter()
                .enumerate()
                .filter(|(_, entry)| is_workflow(entry))
                .map(|(i, entry)| (format!("workflows[{i}]"), entry))
                .collect(),
            _ => Vec::new(),
        },
        // A bulk-import array holds workflows; a bare task list holds tasks,
        // and a task names a function.
        Value::Array(items)
            if !items.is_empty()
                && items
                    .iter()
                    .all(|item| is_workflow(item) && item.get("function").is_none()) =>
        {
            items
                .into_iter()
                .enumerate()
                .map(|(i, item)| (format!("[{i}]"), item))
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Every workflow definition in the trees, canonical, and every one that
/// has no canonical form, with why.
fn corpus() -> (Vec<Definition>, Vec<(String, String)>) {
    let root = root();
    let sets = set_dirs(&root);
    let mut out = Vec::new();
    let mut not_canonical = Vec::new();

    for dir in &sets {
        let (set, report) = DefinitionSet::from_directory(dir).expect("a definition set loads");
        let errors: Vec<String> = report
            .findings
            .iter()
            .filter(|f| f.is_error())
            .map(ToString::to_string)
            .collect();
        for def in set.iter(Entity::Workflow) {
            let file = relative(&root, &def.origin);
            if errors.is_empty() {
                out.push(Definition {
                    file,
                    at: String::new(),
                    doc: def.doc.clone(),
                });
            } else {
                not_canonical.push((file, errors.join("; ")));
            }
        }
    }

    let shared = SharedDefinitions::default();
    for tree in TREES {
        for path in orion::definitions::json_files(&root.join(tree)).expect("a tree") {
            if sets.iter().any(|set| path.starts_with(set)) {
                continue;
            }
            let Ok(doc) = serde_json::from_str::<Value>(
                &std::fs::read_to_string(&path).expect("a JSON file reads"),
            ) else {
                // The JSON front end's own fixtures, most of them invalid
                // on purpose.
                continue;
            };
            let file = relative(&root, &path.display().to_string());
            for (at, mut doc) in documents_in(doc) {
                let mut findings = Vec::new();
                let origin = format!("{file} {at}");
                orion::definitions::compile::compile(
                    &mut doc,
                    &orion::definitions::Cx::detached(&shared, &origin),
                    &mut findings,
                );
                let errors: Vec<String> = findings
                    .iter()
                    .filter(|f| f.is_error())
                    .map(ToString::to_string)
                    .collect();
                if errors.is_empty() {
                    out.push(Definition {
                        file: file.clone(),
                        at,
                        doc,
                    });
                } else {
                    not_canonical.push((format!("{file} {at}"), errors.join("; ")));
                }
            }
        }
    }
    (out, not_canonical)
}

/// Every literal secret name the corpus reads, so both engines declare it.
fn secret_names(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            if object.len() == 1
                && let Some(Value::String(name)) = object.get("secret")
            {
                out.insert(name.clone());
            }
            object.values().for_each(|v| secret_names(v, out));
        }
        Value::Array(items) => items.iter().for_each(|v| secret_names(v, out)),
        _ => {}
    }
}

// -- The engines --

/// Both engines' load, over inert dependencies and the corpus's plugins.
struct Engines {
    secrets: ResolvedSecrets,
    offline: orion::engine::kit::OfflineDeps,
    compiler: Compiler,
    functions: FunctionRegistry,
    plugin_handlers: Vec<orion::plugin::PluginFunctionHandler>,
}

impl Engines {
    fn new(secrets: ResolvedSecrets) -> Self {
        let root = root();
        let mut set = DefinitionSet::default();
        let plugin_files: Vec<String> = PLUGINS
            .iter()
            .map(|p| root.join(p).display().to_string())
            .collect();
        let findings = set
            .add_plugin_dirs(&plugin_files)
            .expect("the manifests read");
        assert!(findings.iter().all(|f| !f.is_error()), "{findings:?}");
        assert_eq!(set.plugins.len(), PLUGINS.len());
        let functions = set.function_registry().expect("the plugin registry");

        let config = orion::config::PluginsConfig {
            enabled: true,
            ..orion::config::PluginsConfig::default()
        };
        let runtime = orion::plugin::WasmRuntime::new(&config).expect("the sandbox");
        let mut plugin_handlers = Vec::new();
        let mut plugin_functions = Vec::new();
        for plugin in &set.plugins {
            let path = plugin
                .component_path
                .as_ref()
                .expect("a component beside it");
            let component = runtime
                .load_blocking(&std::fs::read(path).expect("the component reads"))
                .expect("the component loads");
            let limits = orion::plugin::Limits::effective(&config, &plugin.manifest.name);
            for (entry, decl) in plugin.entries().into_iter().zip(&plugin.manifest.functions) {
                let handler = orion::plugin::PluginFunctionHandler::new(
                    Arc::new(entry),
                    component.clone(),
                    runtime.clone(),
                    limits,
                );
                plugin_functions.push(orion::plugin::PluginFunction::new(
                    handler.clone(),
                    decl.runtime_fields(),
                ));
                plugin_handlers.push(handler);
            }
        }

        let offline = orion::engine::kit::OfflineDeps::new();
        let exprs =
            orion::engine::operators::orion_exprs(&secrets, 0).expect("Orion's expressions");
        let mut registry = dataflow_rs::Registry::new(exprs);
        orion::engine::register_functions(&mut registry, offline.deps());
        let refused = orion::engine::register_plugin_functions(&mut registry, plugin_functions);
        assert!(refused.is_empty(), "{refused:?}");
        Self {
            secrets,
            compiler: registry.compiler(CompileOptions::default()),
            offline,
            functions,
            plugin_handlers,
        }
    }

    /// dataflow-rs 3's verdict, as boot screens: `Err` holds its reason.
    fn v3(&self, row: &orion::storage::models::Workflow) -> Result<(), String> {
        let workflow = workflow_to_dataflow(row, CHANNEL).map_err(|e| e.to_string())?;
        let mut handlers = orion::engine::build_custom_functions(self.offline.deps());
        for handler in &self.plugin_handlers {
            handlers.insert(handler.name().to_string(), Box::new(handler.clone()));
        }
        orion::engine::build_single(workflow, handlers, &self.secrets, 0)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// dataflow-rs 4's verdict, as the loader reaches it: `Err` holds every
    /// issue, when one quarantines.
    fn v4(&self, row: &orion::storage::models::Workflow) -> Result<(), Vec<Issue>> {
        let mut json = workflow_json(row, CHANNEL, &row.workflow_id, "active", None)
            .expect("a synthetic row is JSON");
        orion::engine::rewrite_aliases(&mut json, &self.functions);
        orion::engine::compile_row(&self.compiler, &json).map(|_| ())
    }
}

// -- The comparison --

/// Whether `issue` is the kind of refusal `why` names, at `task`.
fn fits(issue: &Issue, task: &str, why: Why) -> bool {
    let path = issue.path.to_string();
    let under = path == task || path.starts_with(&format!("{task}."));
    let in_input = path.starts_with(&format!("{task}.function.input"));
    under
        && match why {
            Why::Break2 => {
                issue.code == IssueCode::LikelyTypo && issue.severity == Severity::Defect
            }
            Why::Break2a | Why::I186 => {
                in_input
                    && matches!(issue.code, IssueCode::UnknownKey | IssueCode::LikelyTypo)
                    && issue.severity == Severity::Rejected
            }
            Why::Break21 => {
                issue.code == IssueCode::InputParse
                    && issue.severity == Severity::Defect
                    && path.ends_with(".output")
            }
            Why::RequiredField => {
                in_input
                    && issue.code == IssueCode::InputParse
                    && issue.severity == Severity::Rejected
            }
            Why::Break6 => false,
        }
}

/// The step at an authored path such as `tasks[1].tasks[0]`.
fn step_at<'a>(doc: &'a Value, task: &str) -> Option<&'a Value> {
    task.split('.').try_fold(doc, |value, seg| {
        let (key, index) = match seg.split_once('[') {
            Some((key, rest)) => (key, rest.trim_end_matches(']').parse::<usize>().ok()),
            None => (seg, None),
        };
        let value = value.get(key)?;
        match index {
            Some(i) => value.get(i),
            None => Some(value),
        }
    })
}

#[test]
fn the_v3_and_v4_quarantine_verdicts_agree_over_the_repository_definitions() {
    let (corpus, not_canonical) = corpus();
    let mut names = BTreeSet::new();
    for definition in &corpus {
        secret_names(&definition.doc, &mut names);
    }
    let secrets = ResolvedSecrets::from_values(
        names
            .into_iter()
            .map(|name| (name, Value::String("differential".to_string())))
            .collect(),
    );
    let engines = Engines::new(secrets);

    let mut definitions = 0;
    let mut not_storable = Vec::new();
    let mut accepted = 0;
    let mut quarantined: Vec<String> = Vec::new();
    let mut excepted: Vec<String> = Vec::new();
    let mut unexplained: Vec<String> = Vec::new();
    let mut used = vec![false; EXCEPTIONS.len()];

    for (n, definition) in corpus.iter().enumerate() {
        let Ok(request) = serde_json::from_value::<CreateWorkflowRequest>(definition.doc.clone())
        else {
            not_storable.push(definition.label());
            continue;
        };
        definitions += 1;
        let id = request
            .workflow_id
            .clone()
            .unwrap_or_else(|| format!("differential-{n}"));
        let row = synthetic_workflow(&request, &id).expect("a request is a row");
        let rows: HashMap<usize, &Exception> = EXCEPTIONS
            .iter()
            .enumerate()
            .filter(|(_, e)| e.file == definition.file)
            .collect();

        match (engines.v3(&row), engines.v4(&row)) {
            (Ok(()), Ok(())) => accepted += 1,
            (Err(reason), Err(issues)) => {
                let codes: Vec<String> = issues
                    .iter()
                    .filter(|i| orion::engine::quarantines(i))
                    .map(|i| format!("{} at {}", i.code.as_str(), i.path))
                    .collect();
                quarantined.push(format!(
                    "{}: v3: {reason}; v4: {}",
                    definition.label(),
                    codes.join(", ")
                ));
            }
            (Ok(()), Err(issues)) => {
                let refusing: Vec<&Issue> = issues
                    .iter()
                    .filter(|i| orion::engine::quarantines(i))
                    .collect();
                let explained = refusing.iter().all(|issue| {
                    rows.iter().any(|(ix, row)| {
                        let fit = fits(issue, row.task, row.why);
                        if fit {
                            used[*ix] = true;
                        }
                        fit
                    })
                });
                let described = refusing
                    .iter()
                    .map(|i| {
                        format!(
                            "{} {} at {}: {}",
                            i.severity,
                            i.code.as_str(),
                            i.path,
                            i.message
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                if explained {
                    excepted.push(format!(
                        "{}: v4 quarantines: {described}",
                        definition.label()
                    ));
                } else {
                    unexplained.push(format!(
                        "{}: v3 loads, v4 quarantines: {described}",
                        definition.label()
                    ));
                }
            }
            (Err(reason), Ok(())) => {
                let explained = rows.iter().any(|(ix, row)| {
                    let fit = row.why == Why::Break6
                        && step_at(&definition.doc, row.task)
                            .is_some_and(|step| step.get("name").is_none());
                    if fit {
                        used[*ix] = true;
                    }
                    fit
                });
                if explained {
                    excepted.push(format!("{}: v3 quarantines: {reason}", definition.label()));
                } else {
                    unexplained.push(format!(
                        "{}: v4 loads, v3 quarantines: {reason}",
                        definition.label()
                    ));
                }
            }
        }
    }

    println!(
        "v4 differential: {definitions} definitions: {accepted} accepted both ways, \
         {} quarantined both ways, {} excepted ({} rows); {} not storable, {} with no \
         canonical form",
        quarantined.len(),
        excepted.len(),
        EXCEPTIONS.len(),
        not_storable.len(),
        not_canonical.len(),
    );
    for line in &quarantined {
        println!("  quarantined both ways: {line}");
    }
    for line in &excepted {
        println!("  excepted: {line}");
    }
    for label in &not_storable {
        println!("  not storable: {label}");
    }
    for (label, why) in &not_canonical {
        println!("  no canonical form: {label}: {why}");
    }

    assert!(
        definitions >= 100,
        "the corpus is every definition in the repository, and found only {definitions}"
    );
    assert!(
        unexplained.is_empty(),
        "verdicts differ with no exception row naming the break:\n{}",
        unexplained.join("\n")
    );
    let stale: Vec<String> = EXCEPTIONS
        .iter()
        .zip(&used)
        .filter(|(_, used)| !**used)
        .map(|(e, _)| format!("{} {} ({:?})", e.file, e.task, e.why))
        .collect();
    assert!(
        stale.is_empty(),
        "exception rows that match no difference:\n{}",
        stale.join("\n")
    );
}
