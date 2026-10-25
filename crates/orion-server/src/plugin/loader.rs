//! From active plugin rows to what a generation carries: registry entries,
//! engine functions, and the reasons anything did not load.
//!
//! A load never aborts. A plugin that fails — no artifact under its digest,
//! a component that will not compile, a self-test that traps, or a node with
//! plugins disabled — becomes a [`PluginLoadIssue`], and every workflow
//! naming one of its functions is quarantined by the engine screen with
//! that reason attached. The node keeps serving everything else, which is
//! the same shape a channel that fails to load already takes.

use std::sync::Arc;
use std::time::Instant;

use dataflow_rs::Issue;
use dataflow_rs::program::{IssueCode, PathSeg};

use crate::config::PluginsConfig;
use crate::engine::{FunctionEntry, PluginBinding};
use crate::storage::models::Plugin;

use super::handler::{PluginFunction, PluginFunctionHandler};
use super::limits::Limits;
use super::manifest::Manifest;
use super::runtime::WasmRuntime;

/// Why a plugin version is not serving on this node.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PluginLoadIssue {
    pub plugin: String,
    pub version: i64,
    pub digest: String,
    /// `disabled`, `manifest`, `signature`, `artifact`, `compile`, `link`,
    /// `size`, `self_test`.
    pub stage: &'static str,
    pub reason: String,
}

/// One plugin version this node loaded.
pub struct LoadedPlugin {
    pub id: String,
    pub version: i64,
    pub digest: String,
    pub functions: Vec<String>,
    pub compile_ms: u64,
    entries: Vec<FunctionEntry>,
    /// The engine functions, one per entry, each over its handler.
    plugin_functions: Vec<PluginFunction>,
}

/// The plugin half of a generation: what loaded, what did not, and the
/// fingerprint of the rows it was built from.
#[derive(Default)]
pub struct PluginSet {
    pub plugins: Vec<LoadedPlugin>,
    pub issues: Vec<PluginLoadIssue>,
    /// Functions of the plugins that did not load, with the reason — so a
    /// workflow naming one is quarantined with a message that names the
    /// plugin rather than "unknown function".
    pub unavailable: Vec<(String, String)>,
    fingerprint: String,
}

impl PluginSet {
    /// The set a node boots on, and the one a node with plugins disabled
    /// keeps when no plugin row exists.
    pub fn empty() -> Self {
        Self::default()
    }

    /// The identity of a set of active rows: sorted `id@version:digest`, so
    /// two reloads over the same rows build nothing twice and any change —
    /// a new version, a digest, an activation — is a different set.
    pub fn fingerprint_of(rows: &[Plugin]) -> String {
        let mut parts: Vec<String> = rows
            .iter()
            .map(|r| format!("{}@{}:{}", r.plugin_id, r.version, r.digest))
            .collect();
        parts.sort();
        parts.join(";")
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Every registry entry the loaded plugins declare.
    pub fn entries(&self) -> Vec<FunctionEntry> {
        self.plugins
            .iter()
            .flat_map(|p| p.entries.iter().cloned())
            .collect()
    }

    /// Every loaded function as an engine function, for one `Registry`.
    /// [`register_plugin_functions`](crate::engine::register_plugin_functions)
    /// registers them.
    pub fn functions(&self) -> impl Iterator<Item = &PluginFunction> {
        self.plugins.iter().flat_map(|p| p.plugin_functions.iter())
    }

    /// Attach the plugin reason to a load issue that names an unavailable
    /// function, so the quarantine message says which plugin and why.
    pub fn annotate(&self, reason: &mut String) {
        for (function, why) in &self.unavailable {
            if reason.contains(function.as_str()) {
                reason.push_str(&format!(" — plugin function '{function}': {why}"));
            }
        }
    }

    /// [`Self::annotate`] for dataflow-rs 4's verdict, by the issue rather
    /// than by its text: when `issue` is the `UNKNOWN_FUNCTION` a task naming
    /// a function of a plugin that did not load is refused with, the plugin
    /// and why are attached to `reason`, in `annotate`'s words.
    ///
    /// The function is the one the task names: the string at the issue's
    /// path (`tasks[i].function.name`, or wherever the step sits, a group's
    /// or a loop's `setup` included) in `workflow`, the definition the issue
    /// was found in, as authored. Nothing is read from the issue's message,
    /// so a function whose name holds another's (`acme.codec.parse` and
    /// `acme.codec.parse-strict`), a task id or a workflow name that spells
    /// one, attaches nothing it should not.
    pub fn annotate_issue(&self, issue: &Issue, workflow: &serde_json::Value, reason: &mut String) {
        if let Some((function, why)) = self.unavailable_named_by(issue, workflow) {
            reason.push_str(&format!(" — plugin function '{function}': {why}"));
        }
    }

    /// The unavailable function `issue` refuses `workflow`'s task for, and
    /// why it is unavailable: see [`Self::annotate_issue`].
    pub fn unavailable_named_by(
        &self,
        issue: &Issue,
        workflow: &serde_json::Value,
    ) -> Option<(&str, &str)> {
        if issue.code != IssueCode::UnknownFunction {
            return None;
        }
        let named = issue
            .path
            .segs()
            .iter()
            .try_fold(workflow, |value, seg| match seg {
                PathSeg::Key(key) => value.get(&**key),
                PathSeg::Index(index) => value.get(*index),
                _ => None,
            })?
            .as_str()?;
        self.unavailable
            .iter()
            .find(|(function, _)| function == named)
            .map(|(function, why)| (function.as_str(), why.as_str()))
    }
}

/// Load every deployed plugin, its component from `components` by digest.
/// `runtime` is `None` on a node with plugins disabled, which makes every
/// row an issue rather than an abort: the estate stays observable and the
/// workflows naming those functions are quarantined with the reason.
pub async fn load_active(
    rows: Vec<Plugin>,
    components: &std::collections::HashMap<String, Vec<u8>>,
    runtime: Option<&Arc<WasmRuntime>>,
    config: &PluginsConfig,
) -> PluginSet {
    let fingerprint = PluginSet::fingerprint_of(&rows);
    let mut set = PluginSet {
        fingerprint,
        ..PluginSet::default()
    };
    for row in rows {
        match load_one(&row, components, runtime, config).await {
            Ok(loaded) => {
                crate::metrics::record_plugin_load(
                    &row.plugin_id,
                    "ok",
                    Some(loaded.compile_ms as f64 / 1000.0),
                );
                tracing::info!(
                    plugin = %row.plugin_id,
                    version = row.version,
                    digest = %row.digest,
                    functions = ?loaded.functions,
                    compile_ms = loaded.compile_ms,
                    "Plugin loaded"
                );
                set.plugins.push(loaded);
            }
            Err((stage, reason)) => {
                crate::metrics::record_plugin_load(&row.plugin_id, "error", None);
                tracing::error!(
                    plugin = %row.plugin_id,
                    version = row.version,
                    digest = %row.digest,
                    stage,
                    reason = %reason,
                    "Plugin not loaded: the workflows naming its functions are quarantined"
                );
                // The functions are known from the manifest even when the
                // component is not, which is what lets the quarantine name
                // the plugin.
                if let Ok(manifest) = serde_json::from_str::<Manifest>(&row.manifest_json) {
                    for f in manifest.function_names() {
                        set.unavailable.push((
                            f.to_string(),
                            format!("{} v{} {stage}: {reason}", row.plugin_id, row.version),
                        ));
                    }
                }
                set.issues.push(PluginLoadIssue {
                    plugin: row.plugin_id.clone(),
                    version: row.version,
                    digest: row.digest.clone(),
                    stage,
                    reason,
                });
            }
        }
    }
    set
}

async fn load_one(
    row: &Plugin,
    components: &std::collections::HashMap<String, Vec<u8>>,
    runtime: Option<&Arc<WasmRuntime>>,
    config: &PluginsConfig,
) -> Result<LoadedPlugin, (&'static str, String)> {
    let manifest: Manifest = serde_json::from_str(&row.manifest_json)
        .map_err(|e| ("manifest", format!("stored manifest does not parse: {e}")))?;
    let Some(runtime) = runtime else {
        return Err((
            "disabled",
            "plugins are disabled on this node (plugins.enabled = false)".to_string(),
        ));
    };
    // The upload's check, repeated by the node that will run the version:
    // a definition can be uploaded through a peer with no keys, and this
    // node's `[plugins.trust]` is the policy that applies here. Checked
    // before the component is compiled — a signature that fails needs no
    // compile to fail.
    super::trust::verify(
        &config.trust.public_keys,
        &row.digest,
        row.signature.as_deref(),
    )
    .map_err(|reason| ("signature", reason))?;
    let loaded = match runtime.cached(&row.digest) {
        Some(loaded) => loaded,
        None => {
            // A deployed plugin's component rides in its definition.
            let bytes = components.get(&row.digest).cloned().ok_or_else(|| {
                (
                    "artifact",
                    format!("no component is deployed under {}", row.digest),
                )
            })?;
            let started = Instant::now();
            let loaded = runtime
                .load(bytes)
                .await
                .map_err(|e| (load_stage(&e), e.to_string()))?;
            let _ = started;
            loaded
        }
    };
    if loaded.digest != row.digest {
        return Err((
            "artifact",
            format!(
                "stored bytes hash to {} but the row names {}",
                loaded.digest, row.digest
            ),
        ));
    }
    let limits = Limits::effective(config, &row.plugin_id);
    let functions: Vec<String> = manifest.function_names().map(str::to_string).collect();
    let names: Vec<&str> = functions.iter().map(String::as_str).collect();
    runtime
        .self_test(&loaded, &limits, &names)
        .await
        .map_err(|e| ("self_test", e.to_string()))?;

    let binding = PluginBinding {
        id: row.plugin_id.clone(),
        version: row.version,
        digest: row.digest.clone(),
        abi: manifest.abi.clone(),
    };
    let entries = manifest.entries(&binding);
    // `entries` holds one entry per declared function, in order, so each
    // handler is its declaration's.
    let plugin_functions = manifest
        .functions
        .iter()
        .zip(&entries)
        .map(|(decl, entry)| {
            let handler = PluginFunctionHandler::new(
                Arc::new(entry.clone()),
                loaded.clone(),
                runtime.clone(),
                limits,
            );
            PluginFunction::new(handler, decl.runtime_fields())
        })
        .collect();
    Ok(LoadedPlugin {
        id: row.plugin_id.clone(),
        version: row.version,
        digest: row.digest.clone(),
        functions,
        compile_ms: loaded.compile_time.as_millis() as u64,
        entries,
        plugin_functions,
    })
}

fn load_stage(e: &super::runtime::LoadError) -> &'static str {
    use super::runtime::LoadError;
    match e {
        LoadError::TooLarge { .. } => "size",
        LoadError::Compile(_) => "compile",
        LoadError::Link(_) => "link",
        LoadError::SelfTest { .. } => "self_test",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dataflow_rs::def::parse;
    use dataflow_rs::program::CompileOptions;
    use serde_json::{Value, json};

    /// Two plugins that did not load, as `load_active` records them.
    fn set() -> PluginSet {
        PluginSet {
            unavailable: vec![
                (
                    "acme.codec.parse".to_string(),
                    "acme.codec v2 self_test: the export trapped".to_string(),
                ),
                (
                    "acme.other.render".to_string(),
                    "acme.other v1 disabled: plugins are disabled on this node \
                     (plugins.enabled = false)"
                        .to_string(),
                ),
            ],
            ..PluginSet::default()
        }
    }

    /// Every issue dataflow-rs 4 refuses `workflow` with, on a registry
    /// holding none of the plugins' functions.
    fn issues(workflow: &Value) -> Vec<Issue> {
        let compiler = dataflow_rs::Registry::new(crate::engine::kit::test_exprs())
            .compiler(CompileOptions::default());
        let def = parse(workflow, crate::engine::parse_options())
            .def
            .expect("the workflow parses");
        compiler.compile_one(&def).expect_err("refused")
    }

    /// A workflow naming a function of a plugin that did not load is
    /// refused `UNKNOWN_FUNCTION` at the task's `function.name`, and the
    /// plugin's reason is attached by what the task names there: in a
    /// group and in a loop's `setup` too, and not to a task naming another
    /// function whose name holds that one, which the text-matching
    /// `annotate` attaches it to.
    #[test]
    fn an_unloaded_plugin_function_is_named_by_the_issues_path_not_its_text() {
        let set = set();
        let workflow = json!({
            "id": "statements",
            "name": "acme.codec.parse every line",
            "loop": {
                "max": 2,
                "setup": [{"id": "warm", "function": {"name": "acme.other.render", "input": {}}}]
            },
            "tasks": [
                {"id": "acme.codec.parse", "function": {"name": "acme.codec.parse-strict", "input": {}}},
                {"id": "g", "tasks": [
                    {"id": "decode", "function": {"name": "acme.codec.parse", "input": {"record": "x"}}}
                ]}
            ]
        });
        let issues = issues(&workflow);
        let reasons: Vec<(String, IssueCode, String)> = issues
            .iter()
            .map(|issue| {
                let mut reason = String::new();
                set.annotate_issue(issue, &workflow, &mut reason);
                (issue.path.to_string(), issue.code, reason)
            })
            .collect();
        assert_eq!(
            reasons,
            [
                (
                    "loop.setup[0].function.name".to_string(),
                    IssueCode::UnknownFunction,
                    " — plugin function 'acme.other.render': acme.other v1 disabled: plugins are \
                     disabled on this node (plugins.enabled = false)"
                        .to_string(),
                ),
                (
                    "tasks[0].function.name".to_string(),
                    IssueCode::UnknownFunction,
                    String::new(),
                ),
                (
                    "tasks[1].tasks[0].function.name".to_string(),
                    IssueCode::UnknownFunction,
                    " — plugin function 'acme.codec.parse': acme.codec v2 self_test: the export \
                     trapped"
                        .to_string(),
                ),
            ]
        );
        assert_eq!(
            set.unavailable_named_by(&issues[2], &workflow),
            Some((
                "acme.codec.parse",
                "acme.codec v2 self_test: the export trapped"
            ))
        );

        // What the text alone gives: the task naming `acme.codec.parse-strict`
        // reads as naming `acme.codec.parse`.
        let mut by_text = issues[1].message.clone();
        set.annotate(&mut by_text);
        assert!(
            by_text.contains("plugin function 'acme.codec.parse'"),
            "{by_text}"
        );

        // Only the unknown function's issue is the plugin's, at its path.
        let elsewhere = Issue::new(
            IssueCode::InputParse,
            "tasks[1].tasks[0].function.name".parse().expect("a path"),
            "acme.codec.parse",
        );
        assert_eq!(set.unavailable_named_by(&elsewhere, &workflow), None);
        let off_the_path = Issue::new(
            IssueCode::UnknownFunction,
            "tasks[9].function.name".parse().expect("a path"),
            "acme.codec.parse",
        );
        assert_eq!(set.unavailable_named_by(&off_the_path, &workflow), None);
    }
}
