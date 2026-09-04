//! The engine handler behind every plugin function: an ordinary
//! `AsyncFunctionHandler` whose body is a sandboxed call.
//!
//! The steps are the design's, in order: validate the task's input against
//! the entry's own table and fold `{"var": …}` in its resolvable fields;
//! refuse anything over `max_request_bytes` before entering WASM; take the
//! function's concurrency permit within the deadline; invoke under a fresh
//! store; validate the result and write it at `output`. A failure at any step
//! writes nothing.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use serde_json::{Map, Value};
use tokio::sync::Semaphore;

use super::error::{Category, Failure};
use super::limits::Limits;
use super::runtime::{LoadedComponent, WasmRuntime};
use crate::engine::FunctionEntry;
use crate::engine::functions::connector_helpers::{apply_output, resolve_value};
use crate::engine::functions::templated_input::TemplatedInput;
use crate::plugin::manifest::OUTPUT_FIELD;

/// One registered plugin function. Cheap to clone — every field is shared —
/// so a generation boxes a clone per engine it builds.
#[derive(Clone)]
pub struct PluginFunctionHandler {
    pub entry: Arc<FunctionEntry>,
    pub loaded: Arc<LoadedComponent>,
    pub runtime: Arc<WasmRuntime>,
    pub limits: Limits,
    /// The function's concurrency permits, shared by every clone.
    permits: Arc<Semaphore>,
    /// The plugin id, for metric labels: a registered name, never a guest
    /// string.
    plugin_id: String,
}

impl PluginFunctionHandler {
    pub fn new(
        entry: Arc<FunctionEntry>,
        loaded: Arc<LoadedComponent>,
        runtime: Arc<WasmRuntime>,
        limits: Limits,
    ) -> Self {
        let plugin_id = entry
            .plugin
            .as_ref()
            .map(|p| p.id.clone())
            .unwrap_or_default();
        Self {
            entry,
            loaded,
            runtime,
            limits,
            permits: Arc::new(Semaphore::new(limits.max_concurrency as usize)),
            plugin_id,
        }
    }

    /// The function's registered name.
    pub fn name(&self) -> &str {
        &self.entry.name
    }

    async fn run(&self, ctx: &mut TaskContext<'_>, input: &TemplatedInput) -> Result<(), Failure> {
        let raw = input.raw();
        let obj = raw
            .as_object()
            .ok_or_else(|| Failure::host(Category::CallerInput, "input must be a JSON object"))?;

        // Where the result goes: the task's `output`, else the function's
        // declared default root, else nowhere — which is a refusal, because a
        // pure function whose result is dropped did nothing.
        let output = match obj.get(OUTPUT_FIELD) {
            Some(Value::String(path)) if !path.trim().is_empty() => path.clone(),
            Some(_) => {
                return Err(Failure::host(
                    Category::CallerInput,
                    "'output' must be a non-empty dotted context path",
                ));
            }
            None => match self.entry.writes {
                crate::engine::functions::schema::WriteShape::OutputPath {
                    default_root: Some(root),
                } => root.to_string(),
                _ => {
                    return Err(Failure::host(
                        Category::CallerInput,
                        "the task names no 'output' and the function declares no default root",
                    ));
                }
            },
        };

        // Only declared fields reach the guest, `output` excluded — it is the
        // host's — and a resolvable field is folded against the message.
        let mut guest = Map::new();
        for field in self.entry.input_fields.as_deref().unwrap_or(&[]) {
            if field.name == OUTPUT_FIELD {
                continue;
            }
            if let Some(value) = obj.get(&field.name) {
                let value = if field.resolvable {
                    resolve_value(value, ctx)
                } else {
                    value.clone()
                };
                guest.insert(field.name.clone(), value);
            }
        }
        let guest = Value::Object(guest);

        // The schema again, over the resolved values: a `{"var": …}` that
        // resolved to the wrong kind, or to nothing where a field is
        // required, is the caller's error and is refused before WASM.
        let problems = self.entry.validate_input(&guest, "task");
        if !problems.is_empty() {
            let text = problems
                .iter()
                .map(|p| format!("{} ({})", p.message, p.code))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(Failure::host(Category::CallerInput, text));
        }

        let text = serde_json::to_string(&guest).map_err(|e| {
            Failure::host(Category::CallerInput, "input is not serialisable").with_detail(e)
        })?;
        if text.len() > self.limits.max_request_bytes {
            return Err(Failure::host(
                Category::RequestSize,
                format!(
                    "the input is {} bytes, over the {} byte limit",
                    text.len(),
                    self.limits.max_request_bytes
                ),
            ));
        }

        let queued = Instant::now();
        let permit = tokio::time::timeout(self.limits.timeout, self.permits.acquire())
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or_else(|| {
                Failure::host(
                    Category::Permit,
                    format!(
                        "no concurrency permit within {:?} ({} already running)",
                        self.limits.timeout, self.limits.max_concurrency
                    ),
                )
            })?;
        crate::metrics::record_plugin_queue_time(
            &self.plugin_id,
            self.entry.label(),
            queued.elapsed().as_secs_f64(),
        );

        let json = self
            .runtime
            .invoke(&self.loaded, &self.limits, &self.entry.name, &text)
            .await
            .map_err(super::runtime::Invocation::into_failure)?;
        drop(permit);

        let value: Value = serde_json::from_str(&json).map_err(|e| {
            Failure::host(
                Category::BadResult,
                "the plugin returned something that is not JSON",
            )
            .with_detail(e)
        })?;
        apply_output(ctx, &output, value);
        Ok(())
    }
}

#[async_trait]
impl AsyncFunctionHandler for PluginFunctionHandler {
    type Input = TemplatedInput;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &TemplatedInput,
    ) -> dataflow_rs::Result<TaskOutcome> {
        let started = Instant::now();
        let result = self.run(ctx, input).await;
        let secs = started.elapsed().as_secs_f64();
        match result {
            Ok(()) => {
                crate::metrics::record_plugin_invocation(
                    &self.plugin_id,
                    self.entry.label(),
                    "ok",
                    secs,
                );
                Ok(TaskOutcome::Success)
            }
            Err(failure) => {
                crate::metrics::record_plugin_invocation(
                    &self.plugin_id,
                    self.entry.label(),
                    "error",
                    secs,
                );
                crate::metrics::record_plugin_failure(
                    &self.plugin_id,
                    self.entry.label(),
                    failure.category.as_str(),
                );
                if let Some(detail) = &failure.detail {
                    // The host's account of a guest failure goes to the
                    // operator, with everything needed to find the plugin —
                    // and never into the task error a client may read.
                    tracing::warn!(
                        plugin = %self.plugin_id,
                        function = %self.entry.name,
                        digest = %self.loaded.digest,
                        workflow = ctx.workflow_id().unwrap_or("-"),
                        task = ctx.task_id().unwrap_or("-"),
                        category = failure.category.as_str(),
                        detail = %detail,
                        "Plugin invocation failed"
                    );
                }
                Err(failure
                    .into_handler_error()
                    .prefixed(&self.entry.name)
                    .into())
            }
        }
    }
}
