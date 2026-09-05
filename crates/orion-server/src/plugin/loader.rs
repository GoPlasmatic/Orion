//! From active plugin rows to what a generation carries: registry entries,
//! engine handlers, and the reasons anything did not load.
//!
//! A load never aborts. A plugin that fails — no artifact under its digest,
//! a component that will not compile, a self-test that traps, or a node with
//! plugins disabled — becomes a [`PluginLoadIssue`], and every workflow
//! naming one of its functions is quarantined by the engine screen with
//! that reason attached. The node keeps serving everything else, which is
//! the same shape a channel that fails to load already takes.

use std::sync::Arc;
use std::time::Instant;

use crate::config::PluginsConfig;
use crate::engine::{FunctionEntry, PluginBinding};
use crate::storage::models::{Plugin, PluginHealth};
use crate::storage::repositories::plugins::PluginRepository;

use super::handler::PluginFunctionHandler;
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
    handlers: Vec<PluginFunctionHandler>,
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

    /// A fresh boxed handler per function, for one engine build.
    pub fn handlers(&self) -> Vec<(String, dataflow_rs::BoxedFunctionHandler)> {
        self.plugins
            .iter()
            .flat_map(|p| p.handlers.iter())
            .map(|h| {
                (
                    h.name().to_string(),
                    Box::new(h.clone()) as dataflow_rs::BoxedFunctionHandler,
                )
            })
            .collect()
    }

    /// This node's account of one plugin version.
    pub fn health_of(&self, plugin_id: &str, version: i64, enabled: bool) -> PluginHealth {
        if let Some(p) = self
            .plugins
            .iter()
            .find(|p| p.id == plugin_id && p.version == version)
        {
            return PluginHealth {
                state: "loaded".to_string(),
                compile_ms: Some(p.compile_ms),
                reason: None,
            };
        }
        if let Some(issue) = self
            .issues
            .iter()
            .find(|i| i.plugin == plugin_id && i.version == version)
        {
            return PluginHealth {
                state: if issue.stage == "disabled" {
                    "disabled".to_string()
                } else {
                    "failed".to_string()
                },
                compile_ms: None,
                reason: Some(format!("{}: {}", issue.stage, issue.reason)),
            };
        }
        PluginHealth {
            state: if enabled { "inactive" } else { "disabled" }.to_string(),
            compile_ms: None,
            reason: None,
        }
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
}

/// Load every active row. `runtime` is `None` on a node with plugins
/// disabled, which makes every row an issue rather than an abort: the
/// estate stays observable and the workflows naming those functions are
/// quarantined with the reason.
pub async fn load_active(
    rows: Vec<Plugin>,
    repo: &dyn PluginRepository,
    runtime: Option<&Arc<WasmRuntime>>,
    config: &PluginsConfig,
) -> PluginSet {
    let fingerprint = PluginSet::fingerprint_of(&rows);
    let mut set = PluginSet {
        fingerprint,
        ..PluginSet::default()
    };
    for row in rows {
        match load_one(&row, repo, runtime, config).await {
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
    repo: &dyn PluginRepository,
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
    // a row can reach the database through an import on a node with no
    // keys, or a peer's activation, and this node's `[plugins.trust]` is the
    // policy that applies here. Checked before the bytes are fetched — a
    // signature that fails needs no compile to fail.
    super::trust::verify(
        &config.trust.public_keys,
        &row.digest,
        row.signature.as_deref(),
    )
    .map_err(|reason| ("signature", reason))?;
    let loaded = match runtime.cached(&row.digest) {
        Some(loaded) => loaded,
        None => {
            let bytes = repo
                .get_artifact(&row.digest)
                .await
                .map_err(|e| ("artifact", e.to_string()))?
                .ok_or_else(|| {
                    (
                        "artifact",
                        format!("no component is stored under {}", row.digest),
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
    let handlers = entries
        .iter()
        .map(|e| {
            PluginFunctionHandler::new(Arc::new(e.clone()), loaded.clone(), runtime.clone(), limits)
        })
        .collect();
    Ok(LoadedPlugin {
        id: row.plugin_id.clone(),
        version: row.version,
        digest: row.digest.clone(),
        functions,
        compile_ms: loaded.compile_time.as_millis() as u64,
        entries,
        handlers,
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
