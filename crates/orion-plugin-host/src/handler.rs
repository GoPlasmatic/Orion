//! The engine function behind every plugin function: [`PluginFunction`],
//! whose body is the sandboxed call [`PluginFunctionHandler`] makes.
//!
//! The steps are the design's, in order: validate the task's input against
//! the entry's own table, evaluate its template fields and fold `{"var": …}`
//! in its resolvable ones; refuse anything over `max_request_bytes` before
//! entering WASM; take the function's concurrency permit within the deadline;
//! invoke under a fresh store; validate the result and write it at `output`.
//! A failure at any step writes nothing.
//!
//! One type, one instance per function a manifest declares, so an instance
//! answers for its own registration: the authored input is checked against
//! *this* function's table when the workflow loads, and *this* function's
//! template fields are the ones compiled. The table is the function's spec:
//! its manifest's fields as runtime fields ([`FunctionDecl::runtime_fields`]),
//! which the engine compiles into a `Templated` config at load, and which the
//! call reads through [`Templated::get`]. After the input is resolved the
//! handler does the rest: the schema over the resolved values, the size
//! limit, the permit, the invocation and its classification, the metrics and
//! the operator log.
//!
//! [`FunctionDecl::runtime_fields`]: super::manifest::FunctionDecl::runtime_fields

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Instant;

use dataflow_rs::ErrorKind;
use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{ExprVisitor, FieldSpec as RuntimeField, Interpolated};
use dataflow_rs::function::{FunctionConfig, FunctionSpec, Prepare, Templated};
use dataflow_rs::program::{AuthoredPath, IssueCode, PathSeg};
use dataflow_rs::{Ctx, Issue, Outcome, Path};
use serde_json::{Map, Value};
use tokio::sync::Semaphore;

use super::error::{Category, Failure};
use super::limits::Limits;
use super::runtime::{LoadedComponent, WasmRuntime};

use crate::engine::{FunctionEntry, HandlerError};
use crate::manifest::OUTPUT_FIELD;

/// One registered plugin function. Cheap to clone — every field is shared —
/// so a generation boxes a clone per engine it builds.
#[derive(Clone)]
pub struct PluginFunctionHandler {
    pub entry: Arc<FunctionEntry>,
    pub loaded: Arc<LoadedComponent>,
    pub runtime: Arc<WasmRuntime>,
    pub limits: Limits,
    /// The entry as the guest sees it: every field literal. The authored
    /// input is validated against `entry`, whose template and resolvable
    /// fields may hold an expression in place of a value of their kind; the
    /// *resolved* input is validated against this one, where an expression
    /// that produced the wrong kind has nothing left to hide behind.
    resolved: Arc<FunctionEntry>,
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
        let mut resolved = (*entry).clone();
        if let Some(fields) = resolved.input_fields.as_mut() {
            for field in fields {
                field.resolvable = false;
                field.template_at = &[];
            }
        }
        Self {
            entry,
            loaded,
            runtime,
            limits,
            resolved: Arc::new(resolved),
            permits: Arc::new(Semaphore::new(limits.max_concurrency as usize)),
            plugin_id,
        }
    }

    /// The function's registered name.
    pub fn name(&self) -> &str {
        &self.entry.name
    }

    /// Everything after the input is resolved:
    /// the schema over the resolved values, the size limit, the permit, the
    /// invocation, and its result read as JSON. `guest` is the input the
    /// guest receives, every declared field but `output` that has a value.
    async fn invoke_resolved(&self, guest: Value) -> Result<Value, Failure> {
        // The schema again, over the resolved values and with every field
        // read as literal: an expression that evaluated to the wrong kind,
        // or to nothing where a field is required, is the caller's error and
        // is refused before WASM.
        let problems = self.resolved.validate_input(&guest, "task");
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

        serde_json::from_str(&json).map_err(|e| {
            Failure::host(
                Category::BadResult,
                "the plugin returned something that is not JSON",
            )
            .with_detail(e)
        })
    }

    /// One invocation's end: its metrics, the operator
    /// log of a failure's detail, and the failure as the error a client
    /// sees, prefixed with the function's name. `started` is when the call
    /// began, its prologue included; `workflow` and `task` name the call
    /// site for the log.
    ///
    /// The detail goes to the log and nowhere else: the error carries none,
    /// as 1.12's did not, so a guest's own strings and Wasmtime's text never
    /// ride on an error, its source included.
    fn settle<T>(
        &self,
        started: Instant,
        result: Result<T, Failure>,
        workflow: &str,
        task: &str,
    ) -> Result<T, HandlerError> {
        let secs = started.elapsed().as_secs_f64();
        match result {
            Ok(value) => {
                crate::metrics::record_plugin_invocation(
                    &self.plugin_id,
                    self.entry.label(),
                    "ok",
                    secs,
                );
                Ok(value)
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
                        workflow,
                        task,
                        category = failure.category.as_str(),
                        detail = %detail,
                        "Plugin invocation failed"
                    );
                }
                Err(HandlerError::new(failure.category.class(), failure.message)
                    .prefixed(&self.entry.name))
            }
        }
    }
}

/// One plugin function: its manifest's fields as the
/// runtime fields of a `Templated` config, and a call that is the sandboxed
/// one [`PluginFunctionHandler`] makes. Registered under the name its
/// manifest gives it, `<plugin>.<label>`.
///
/// It wraps the function's [`PluginFunctionHandler`], so every clone shares
/// the component, the limits and the concurrency permits: one function holds
/// one limit. Cheap to clone.
#[derive(Clone)]
pub struct PluginFunction {
    handler: PluginFunctionHandler,
    fields: Arc<[RuntimeField]>,
}

impl PluginFunction {
    /// `handler`'s function, its config compiled from `fields`: the
    /// manifest's, from
    /// [`FunctionDecl::runtime_fields`](super::manifest::FunctionDecl::runtime_fields).
    pub fn new(handler: PluginFunctionHandler, fields: Vec<RuntimeField>) -> Self {
        Self {
            handler,
            fields: fields.into(),
        }
    }

    /// The function's registered name.
    pub fn name(&self) -> &str {
        self.handler.name()
    }

    /// The runtime fields the config compiles.
    pub fn fields(&self) -> &[RuntimeField] {
        &self.fields
    }

    /// Each field the guest receives, with its value for this call, in
    /// declaration order: every declared field but `output` that has one,
    /// through [`Templated::get`]. A literal, and a template that folded to
    /// a constant, are lent from the config (I131); a template is evaluated,
    /// and an interpolated field rendered, owned. A field the task left
    /// unset is absent, as 1.12 passed the guest only the fields a task set.
    ///
    /// # Errors
    ///
    /// 1.12's refusal of a template that did not evaluate, the first in
    /// declaration order.
    pub(crate) fn read_fields<'s, 'c>(
        &'s self,
        ctx: &Ctx<'_>,
        cfg: &'c Templated,
    ) -> Result<Vec<(&'s str, Cow<'c, OwnedDataValue>)>, Failure> {
        let mut read = Vec::with_capacity(self.fields.len());
        for field in self.fields.iter().filter(|f| f.name != OUTPUT_FIELD) {
            match cfg.get(ctx, &field.name) {
                Ok(Some(value)) => read.push((&*field.name, value)),
                Ok(None) => {}
                Err(e) => return Err(did_not_evaluate(&field.name, &e)),
            }
        }
        Ok(read)
    }

    /// 1.12's prologue, before anything enters the sandbox: where the result
    /// goes, and the guest's input as JSON.
    ///
    /// The destination is the task's `output`, else the function's declared
    /// default root. Both are literal paths by here: the config refused a
    /// task naming none where there is no default (the field is required),
    /// and `prepare` an output not written as a string.
    fn prologue<'c>(
        &self,
        ctx: &Ctx<'_>,
        cfg: &'c Templated,
    ) -> Result<(&'c Path, Value), Failure> {
        let Some(out) = cfg.primary_output() else {
            return Err(Failure::host(
                Category::CallerInput,
                "the task names no 'output' and the function declares no default root",
            ));
        };
        let Some(path) = out.constant() else {
            return Err(Failure::host(
                Category::CallerInput,
                "'output' must be a non-empty dotted context path",
            ));
        };
        let guest: Map<String, Value> = self
            .read_fields(ctx, cfg)?
            .into_iter()
            .map(|(name, value)| (name.to_owned(), Value::from(&*value)))
            .collect();
        Ok((path, Value::Object(guest)))
    }
}

impl dataflow_rs::Function for PluginFunction {
    type Config = Templated;

    /// The registry entry's description and category, its retry safety on
    /// dataflow-rs 4's answers (`Pure`, so `Idempotent`), and the manifest's
    /// fields, which the engine compiles every call site's input against.
    fn spec(&self) -> FunctionSpec {
        let entry = &self.handler.entry;
        FunctionSpec::default()
            .with_description(entry.description.clone())
            .with_category(entry.category.clone())
            .with_retry(crate::engine::kit::retry(entry.retry_safety))
            .with_runtime_fields(self.fields.to_vec())
    }

    /// The load rules 1.12 applied (`parse_input_with`, over the entry's
    /// table) that a `Templated` config does not apply itself, read off the
    /// authored values the config's visit reports, so a value refused here
    /// quarantined its channel on 1.12 too:
    ///
    /// - **`output`** is a literal path, as 1.12 read it: a string, or the
    ///   refusal of every call. An expression, which a dataflow-rs 4
    ///   `Output` would evaluate, is refused as 1.12 refused it
    ///   (`TYPE_MISMATCH`: the field is a literal string).
    /// - **A `resolvable` field** holds a value of its kind or one `var`
    ///   node, as 1.12 checked it. An interpolated document takes any JSON,
    ///   and a value of another kind renders to that kind, so it would fail
    ///   every call; 1.12 refused it at load, and so does this.
    ///
    /// A literal of the wrong kind, and a template that folds to one, are
    /// the config's own refusals. Then, for an output that is a literal
    /// context root, the default root included, I185's Advisory: a guest's
    /// result may be an object or may not, and a root takes only an object
    /// (break 21).
    fn prepare(&self, cfg: &mut Templated, _p: &Prepare<'_>) -> Result<(), Vec<Issue>> {
        let mut authored = Authored {
            entry: &self.handler.entry,
            issues: Vec::new(),
        };
        FunctionConfig::visit(&*cfg, &mut authored);
        let mut issues = authored.issues;
        if issues.is_empty()
            && let Some(out) = cfg.primary_output()
            && let Err(advisory) =
                crate::engine::kit::root_output_advisory(out, &at_root(self.name()))
        {
            issues.extend(advisory);
        }
        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
    }

    /// 1.12's call: the prologue, then the handler's sandboxed invocation,
    /// then the result written at `output`.
    ///
    /// The run's deadline and cancellation race the call in the engine, so
    /// a run that ends first ends it `TimedOut` or `Cancelled` with nothing
    /// written; the function's own ceiling (`timeout_ms`: the permit wait,
    /// the epoch and the wall clock) gives 1.12's refusals as before.
    async fn call(&self, ctx: &mut Ctx<'_>, cfg: &Templated) -> dataflow_rs::Result<Outcome> {
        let started = Instant::now();
        let result = match self.prologue(ctx, cfg) {
            Ok((path, guest)) => self
                .handler
                .invoke_resolved(guest)
                .await
                .map(|value| (path, value)),
            Err(failure) => Err(failure),
        };
        let info = ctx.info();
        let (workflow, task) = (info.workflow_id, info.task_id);
        let (path, value) = self.handler.settle(started, result, workflow, task)?;
        // After the invocation is counted, as the success it was: a write a
        // root refuses (a result that is not an object, break 21) is the
        // engine's error, not the plugin's.
        ctx.set(path, OwnedDataValue::from(value))?;
        Ok(Outcome::Ok)
    }
}

/// 1.12's refusal of a template field whose expression failed, from the
/// engine's error: the error worded as 1.12 worded it, an
/// evaluation's under "Logic evaluation error: " and a budget refusal's as
/// datalogic does, and the caller's class, so its code is `VALIDATION_ERROR`
/// for a budget refusal too, as 1.12's was.
fn did_not_evaluate(field: &str, e: &dataflow_rs::Error) -> Failure {
    let label = match e.kind() {
        ErrorKind::Eval => "Logic evaluation error: ",
        _ => "",
    };
    Failure::host(
        Category::CallerInput,
        format!("'{field}' did not evaluate: {label}{}", e.message()),
    )
}

/// I185's Advisory for a plugin function whose output is a literal root.
fn at_root(function: &str) -> String {
    format!(
        "`{function}` writes its result at `output`, and a context root takes only an object \
         (break 21): a result that is not an object fails the call; set `output` to a path \
         below it, such as `data.result`"
    )
}

/// The visitor behind [`PluginFunction`]'s `prepare`: 1.12's load rules
/// over a plugin task's authored input, its issues collected.
struct Authored<'e> {
    entry: &'e FunctionEntry,
    issues: Vec<Issue>,
}

impl<'e> Authored<'e> {
    /// The entry's field a visit is at: the path's one key.
    fn field(&self, at: &AuthoredPath) -> Option<&'e crate::engine::FieldSpec> {
        let [PathSeg::Key(name)] = at.segs() else {
            return None;
        };
        self.entry
            .input_fields
            .as_deref()?
            .iter()
            .find(|field| field.name == **name)
    }

    /// 1.12's `TYPE_MISMATCH` for `field`, at `at`, in its words.
    fn mismatch(&mut self, at: &AuthoredPath, field: &crate::engine::FieldSpec) {
        self.issues.push(Issue::new(
            IssueCode::InputParse,
            at.clone(),
            format!("expected {} for '{}'", field.kind.as_str(), field.name),
        ));
    }
}

impl ExprVisitor for Authored<'_> {
    fn expr(&mut self, at: &AuthoredPath, raw: &Value, _records: bool) {
        if let Some(field) = self.field(at)
            && field.name == OUTPUT_FIELD
            && !raw.is_string()
        {
            self.mismatch(at, field);
        }
    }

    fn interpolated(&mut self, at: &AuthoredPath, doc: &Interpolated) {
        if let Some(field) = self.field(at)
            && !field.admits(doc.raw())
        {
            self.mismatch(at, field);
        }
    }
}

impl crate::engine::NamedFunction for PluginFunction {
    fn name(&self) -> &str {
        PluginFunction::name(self)
    }
}

#[cfg(test)]
#[path = "handler_tests.rs"]
mod tests;
