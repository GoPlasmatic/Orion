//! One workflow, run where nothing serves it: `orion-server dry-run`, the
//! `test` runner, and `POST /definitions/test`.
//!
//! All three compile the workflow on a dataflow-rs 4 registry and run it on
//! the engine a node serves with, from the message a node builds, so their
//! verdict is the serving engine's. What differs is the registry: the test
//! endpoint compiles on the node's own (live connectors), the CLI on
//! [`offline_compiler`]'s, whose connector functions a
//! [`StubLayer`](super::functions::stub::StubLayer) answers instead.

use std::sync::Arc;

use dataflow_rs::function::CallLayer;
use dataflow_rs::observe::{Observer, TraceOptions};
use dataflow_rs::program::{CompileOptions, Compiler};
use dataflow_rs::run::{AuditMode, RunOptions};
use serde_json::Value;

use crate::errors::OrionError;

/// An engine over exactly one workflow: `json` compiled on `compiler` as
/// boot and reload compile a row, so a workflow they would quarantine is
/// refused here, naming every issue that quarantines it.
///
/// # Errors
///
/// A validation error naming the workflow and the refusal.
pub fn single_engine(compiler: &Compiler, json: &Value) -> Result<dataflow_rs::Engine, OrionError> {
    let compiled = super::compile_row(compiler, json).map_err(|issues| {
        let id = json.get("id").and_then(Value::as_str).unwrap_or("workflow");
        OrionError::validation(format!(
            "workflow '{id}' has an unusable task: {}",
            super::loader::refusal(&issues, &crate::plugin::PluginSet::empty(), json)
        ))
    })?;
    Ok(super::serving_engine(compiler.link([compiled.workflow])))
}

/// The registry an offline run compiles on: every Orion function over inert
/// dependencies, the expressions reading `secrets`, and the `plugins` loaded
/// from their components.
///
/// # Errors
///
/// A validation error when the expressions do not build, or a plugin function
/// is refused registration.
pub fn offline_compiler(
    secrets: &super::ResolvedSecrets,
    plugins: Vec<crate::plugin::PluginFunction>,
) -> Result<Compiler, OrionError> {
    let exprs = super::operators::orion_exprs(secrets, 0)
        .map_err(|e| OrionError::validation(format!("the offline expression engine: {e}")))?;
    let offline = super::kit::OfflineDeps::new();
    let mut deps = offline.deps();
    deps.exprs = exprs;
    let mut registry = super::serving_registry(deps);
    let refused = super::register_plugin_functions(&mut registry, plugins);
    if !refused.is_empty() {
        return Err(OrionError::validation(format!(
            "plugin function(s) could not be registered: {}",
            refused.join(", ")
        )));
    }
    Ok(registry.compiler(CompileOptions::default()))
}

/// How much of an offline run is traced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfflineTrace {
    /// Every step with its changes, so the trace replays to the context
    /// after each one.
    Changes,
    /// Every step's id, result, status and timing, without its changes.
    Steps,
}

/// One offline run's options. The run names no channel: the engine holds
/// the one workflow, and a named channel would overwrite the caller's
/// `metadata.channel`.
pub struct OfflineRun<'a> {
    /// The trace to keep, or none.
    pub trace: Option<OfflineTrace>,
    /// A ceiling on what the trace holds; `0` is unbounded.
    pub max_snapshot_bytes: usize,
    /// Whether each audit entry records the writes its task made.
    pub capture_changes: bool,
    /// Layers around every host call: the stubs, for a CLI run.
    pub layers: &'a [Arc<dyn CallLayer>],
    /// An observer of the run, beside the engine's own.
    pub observer: Option<&'a dyn Observer>,
}

/// What one offline run left.
pub struct OfflineResult {
    /// The message as the workflow left it.
    pub message: dataflow_rs::Message,
    /// Whether a workflow was selected and ran.
    pub matched: bool,
    /// The trace, as a node persists one.
    pub trace: Option<dataflow_rs::observe::Trace>,
    /// Why the run stopped, when it stopped short.
    pub error: Option<dataflow_rs::Error>,
}

/// Run `engine` over the message a node builds from `data` and `metadata`.
pub async fn run_single(
    engine: &dataflow_rs::Engine,
    data: &Value,
    metadata: &Value,
    run: OfflineRun<'_>,
) -> OfflineResult {
    let mut message = super::execute::serving_message(data, metadata);
    let mut opts = RunOptions::default()
        .capture_changes(run.capture_changes)
        .audit(AuditMode::Full)
        .layers(run.layers);
    if let Some(observer) = run.observer {
        opts = opts.observer(observer);
    }
    if let Some(mode) = run.trace {
        let mut trace = TraceOptions::default()
            .timings(true)
            .changes(mode == OfflineTrace::Changes);
        if run.max_snapshot_bytes > 0 {
            trace = trace.max_bytes(run.max_snapshot_bytes);
        }
        opts = opts.trace(trace);
    }
    let mut report = engine.run_with(&mut message, &opts).await;
    let matched = report.matched() > 0;
    let trace = report.take_trace();
    OfflineResult {
        message,
        matched,
        trace,
        error: report.into_result().err(),
    }
}
