//! The engine functions behind every plugin function: an ordinary
//! `AsyncFunctionHandler` for dataflow-rs 3, and its dataflow-rs 4 twin,
//! [`PluginFunction`], whose body is the same sandboxed call.
//!
//! The steps are the design's, in order: validate the task's input against
//! the entry's own table, evaluate its template fields and fold `{"var": …}`
//! in its resolvable ones; refuse anything over `max_request_bytes` before
//! entering WASM; take the function's concurrency permit within the deadline;
//! invoke under a fresh store; validate the result and write it at `output`.
//! A failure at any step writes nothing.
//!
//! One type, one instance per function a manifest declares. The two
//! receiver-taking hooks dataflow-rs 3.12 added — `parse_input_with` and
//! `compile_input_with` — are what let an instance answer for its own
//! registration: the authored input is checked against *this* function's
//! table when the workflow loads, and *this* function's template fields are
//! the ones compiled. Before 3.12 the hooks had no `&self`, so a plugin field
//! could only be folded or literal.
//!
//! On dataflow-rs 4 the table is the function's spec: its manifest's fields
//! as runtime fields ([`FunctionDecl::runtime_fields`]), which the engine
//! compiles into a `Templated` config at load, and which the call reads
//! through [`Templated::get`]. Both engines share everything after the input
//! is resolved: the schema over the resolved values, the size limit, the
//! permit, the invocation and its classification, the metrics and the
//! operator log.
//!
//! [`FunctionDecl::runtime_fields`]: super::manifest::FunctionDecl::runtime_fields

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use dataflow_rs::ErrorKind;
use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{ExprVisitor, FieldSpec as RuntimeField, Interpolated};
use dataflow_rs::function::{FunctionConfig, FunctionSpec, Prepare, Templated};
use dataflow_rs::program::{AuthoredPath, IssueCode, PathSeg};
use dataflow_rs::{Ctx, Issue, Outcome, Path};
use dataflow_rs_v3::TemplateCompiler;
use dataflow_rs_v3::engine::error::DataflowError;
use dataflow_rs_v3::engine::functions::AsyncFunctionHandler;
use dataflow_rs_v3::engine::task_context::TaskContext;
use dataflow_rs_v3::engine::task_outcome::TaskOutcome;
use serde_json::{Map, Value};
use tokio::sync::Semaphore;

use super::error::{Category, Failure};
use super::limits::Limits;
use super::runtime::{LoadedComponent, WasmRuntime};
use crate::engine::functions::connector_helpers::{apply_output, resolve_value};
use crate::engine::functions::templated_input::{TemplatedInput, with_scope};
use crate::engine::{FunctionEntry, HandlerError};
use crate::plugin::manifest::OUTPUT_FIELD;

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
        // host's. A template field is evaluated by the engine (compiled once
        // at load, in `compile_input_with`), a resolvable field is folded
        // against the message, and anything else is the literal it was
        // written as.
        //
        // Every field resolves against one view of the message: a function
        // declaring k expression fields views it once rather than k times.
        let guest = with_scope(ctx, |scope| {
            let mut guest = Map::new();
            for field in self.entry.input_fields.as_deref().unwrap_or(&[]) {
                if field.name == OUTPUT_FIELD {
                    continue;
                }
                let Some(value) = obj.get(&field.name) else {
                    continue;
                };
                let value = match input.template_value(&field.name, scope) {
                    Some(Ok(evaluated)) => evaluated,
                    Some(Err(e)) => {
                        return Err(Failure::host(
                            Category::CallerInput,
                            format!("'{}' did not evaluate: {e}", field.name),
                        ));
                    }
                    None if field.resolvable => resolve_value(value, scope),
                    None => value.clone(),
                };
                guest.insert(field.name.clone(), value);
            }
            Ok(Value::Object(guest))
        })?;

        let value = self.invoke_resolved(guest).await?;
        apply_output(ctx, &output, value);
        Ok(())
    }

    /// Everything after the input is resolved, which both engines share:
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

    /// One invocation's end, on either engine: its metrics, the operator
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

#[async_trait]
impl AsyncFunctionHandler for PluginFunctionHandler {
    type Input = TemplatedInput;

    /// The authored input against this registration's own table, when the
    /// workflow loads. `check_workflow` and `build` both run it, so a
    /// workflow whose input no longer matches the schema its plugin's active
    /// version declares — a field renamed between versions, say — is
    /// quarantined with the mismatch named, rather than every message
    /// failing at the same check below. The same rules the create-time gate
    /// applies, so what create accepted this accepts, and what a later
    /// version's schema refuses the screen refuses too.
    fn parse_input_with(&self, input: &Value) -> dataflow_rs_v3::Result<TemplatedInput> {
        let problems = self.entry.validate_input(input, "task");
        if !problems.is_empty() {
            return Err(DataflowError::Validation(
                problems
                    .iter()
                    .map(|p| format!("{} ({})", p.message, p.code))
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        Ok(TemplatedInput::from(input.clone()))
    }

    /// Compile this function's template fields — the ones its manifest marks
    /// `template_at` — against the engine's compiler, once at load. A
    /// malformed expression fails here and quarantines the workflow, the
    /// same stance every built-in's template fields take.
    fn compile_input_with(
        &self,
        input: &mut TemplatedInput,
        c: &TemplateCompiler,
    ) -> dataflow_rs_v3::Result<()> {
        input.compile_fields(
            &self.entry.name,
            self.entry.input_fields.as_deref().unwrap_or(&[]),
            c,
        )
    }

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &TemplatedInput,
    ) -> dataflow_rs_v3::Result<TaskOutcome> {
        let started = Instant::now();
        let result = self.run(ctx, input).await;
        let workflow = ctx.workflow_id().unwrap_or("-");
        let task = ctx.task_id().unwrap_or("-");
        self.settle(started, result, workflow, task)
            .map(|()| TaskOutcome::Success)
            .map_err(Into::into)
    }
}

/// One plugin function on dataflow-rs 4: its manifest's fields as the
/// runtime fields of a `Templated` config, and a call that is the sandboxed
/// one [`PluginFunctionHandler`] makes. Registered under the name its
/// manifest gives it, `<plugin>.<label>`.
///
/// It wraps the dataflow-rs 3 handler of the same function, so the two share
/// the component, the limits and the concurrency permits: one function holds
/// one limit, whichever engine calls it. Cheap to clone.
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

    /// 1.12's call: the prologue, then the sandboxed invocation shared with
    /// the dataflow-rs 3 handler, then the result written at `output`.
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

/// 1.12's refusal of a template field whose expression failed, on dataflow-rs
/// 4's error: the error worded as dataflow-rs 3's `Display` worded it, an
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};
    use std::time::Duration;

    use dataflow_rs::function::{RetrySafety, testing};
    use dataflow_rs::program::Severity;
    use dataflow_rs::run::Instant as RunInstant;
    use serde_json::json;

    use crate::config::PluginsConfig;
    use crate::engine::functions::twin::{self, Seen};
    use crate::engine::kit::test_exprs;
    use crate::engine::{FunctionRegistry, PluginBinding};
    use crate::plugin::manifest::{ABI, Manifest};

    const COMPONENT: &[u8] = include_bytes!("../../tests/fixtures/plugins/fixture.wasm");
    const MANIFEST: &str = include_str!("../../tests/fixtures/plugins/fixture.toml");

    /// The fixture's `identity` under a manifest of every position and kind
    /// a field can take. The component dispatches on the function's last
    /// label, so any field table serves; `identity` hands back the input it
    /// was given, which shows what reached the guest.
    const RICH: &str = r#"
abi = "orion:plugin@1.0.0"
name = "test.fixture"
version = "0.0.0"

[[functions]]
name = "test.fixture.identity"
description = "Return the input unchanged"

[[functions.input_fields]]
name = "message"
kind = "any"
resolvable = true

[[functions.input_fields]]
name = "label"
kind = "string"

[[functions.input_fields]]
name = "count"
kind = "number"
template_at = true

[[functions.input_fields]]
name = "greeting"
kind = "string"
template_at = true

[[functions.input_fields]]
name = "mode"
kind = "string"

[[functions.input_fields]]
name = "text"
kind = "string"
resolvable = true
"#;

    fn config() -> PluginsConfig {
        PluginsConfig {
            enabled: true,
            max_memory_bytes: 16 * 1024 * 1024,
            max_timeout_ms: 300,
            max_live_instances: 64,
            max_concurrency_per_function: 64,
            // High enough that the deadline, not fuel, stops a spinner; the
            // fuel category has a case of its own.
            fuel_backstop: 1_000_000_000_000,
            ..PluginsConfig::default()
        }
    }

    /// The fixture component, compiled once for the binary on a sandbox
    /// sized by [`config`]. Each test's functions run on it under the limits
    /// its own config gives them (`Limits::effective`).
    static SANDBOX: LazyLock<(Arc<WasmRuntime>, Arc<LoadedComponent>)> = LazyLock::new(|| {
        let runtime = WasmRuntime::new(&config()).expect("the sandbox builds");
        let loaded = runtime.load_blocking(COMPONENT).expect("the fixture loads");
        (runtime, loaded)
    });

    /// One function on both engines: its dataflow-rs 3 handler, and the
    /// dataflow-rs 4 function over it.
    #[derive(Clone)]
    struct Both {
        v3: PluginFunctionHandler,
        v4: PluginFunction,
    }

    /// Every function `manifest` declares, over the fixture component, under
    /// `config`'s limits, by name: built as the loader builds them.
    fn functions_of(manifest: &str, config: &PluginsConfig) -> HashMap<String, Both> {
        let (runtime, loaded) = &*SANDBOX;
        let manifest = Manifest::parse(manifest).expect("the manifest parses");
        let binding = PluginBinding {
            id: manifest.name.clone(),
            version: 1,
            digest: loaded.digest.clone(),
            abi: ABI.to_string(),
        };
        let limits = Limits::effective(config, &manifest.name);
        manifest
            .entries(&binding)
            .into_iter()
            .zip(&manifest.functions)
            .map(|(entry, decl)| {
                let v3 = PluginFunctionHandler::new(
                    Arc::new(entry),
                    loaded.clone(),
                    runtime.clone(),
                    limits,
                );
                let v4 = PluginFunction::new(v3.clone(), decl.runtime_fields());
                (v4.name().to_string(), Both { v3, v4 })
            })
            .collect()
    }

    fn fixture() -> HashMap<String, Both> {
        functions_of(MANIFEST, &config())
    }

    /// `f` called with `input` over a message whose `data` is `data`, on
    /// both engines.
    async fn twins(f: &Both, input: Value, data: Value) -> (Seen, Seen) {
        let v3 = twin::v3(
            f.v3.name(),
            Box::new(f.v3.clone()),
            input.clone(),
            data.clone(),
        )
        .await;
        let v4 = twin::v4(f.v4.name(), f.v4.clone(), input, data).await;
        (v3, v4)
    }

    /// `f` called on dataflow-rs 4 alone, its issues and writes kept.
    async fn v4_call(f: &PluginFunction, input: Value, data: Value) -> testing::Called {
        testing::call(f.clone(), input)
            .exprs(test_exprs())
            .name(f.name())
            .message(dataflow_rs::Message::from_parts(
                OwnedDataValue::Null,
                OwnedDataValue::from(&data),
                OwnedDataValue::Null,
            ))
            .await
    }

    /// The text dataflow-rs 3 refuses `input` with when it builds an engine
    /// over `f`, its load refusal; `None` when the engine builds.
    fn v3_refusal(f: &PluginFunctionHandler, input: &Value) -> Option<String> {
        let mut fns: HashMap<String, dataflow_rs_v3::BoxedFunctionHandler> = HashMap::new();
        fns.insert(f.name().to_string(), Box::new(f.clone()));
        let workflow: dataflow_rs_v3::Workflow = serde_json::from_value(json!({
            "id": "w", "name": "w", "condition": true,
            "tasks": [{"id": "t", "name": "t", "function": {"name": f.name(), "input": input}}]
        }))
        .expect("a workflow");
        dataflow_rs_v3::Engine::new(vec![workflow], fns)
            .err()
            .map(|e| e.to_string())
    }

    /// The Rejected issues dataflow-rs 4 refuses `input` with at load, as
    /// (path, code); the call must not have run.
    async fn v4_refusal(f: &PluginFunction, input: Value) -> Vec<(String, IssueCode)> {
        let called = v4_call(f, input, json!({})).await;
        assert!(
            called
                .outcome
                .as_ref()
                .is_err_and(|e| *e.kind() == ErrorKind::Compile),
            "refused at load: {:?} {:?}",
            called.outcome,
            called.issues
        );
        called
            .issues
            .iter()
            .filter(|i| i.severity == Severity::Rejected)
            .map(|i| (i.path.to_string(), i.code))
            .collect()
    }

    /// One runtime field as a test compares it: name, kind (`FieldKind` has
    /// no `PartialEq`, so its `Debug`), required, default, recorded.
    fn shape(fields: &[RuntimeField]) -> Vec<(String, String, bool, Option<String>, bool)> {
        fields
            .iter()
            .map(|f| {
                (
                    f.name.to_string(),
                    format!("{:?}", f.kind),
                    f.required,
                    f.default.as_deref().map(str::to_string),
                    f.records,
                )
            })
            .collect()
    }

    /// The fixture component's manifest, field by field (I183): each
    /// function's runtime fields are its declared fields in the positions
    /// 1.12 executed them in, then the implicit `output`, defaulting to the
    /// declared root and required where there is none. The spec says what
    /// the registry entry says, and every function registers beside the
    /// built-ins, its catalogue fields the runtime fields.
    #[test]
    fn the_fixture_manifest_maps_field_by_field() {
        let f = fixture();
        let row = |name: &str, kind: &str, required: bool, default: Option<&str>, records: bool| {
            (
                name.to_string(),
                kind.to_string(),
                required,
                default.map(str::to_string),
                records,
            )
        };
        let at_root = [
            row("message", "Interpolated", true, None, false),
            row("output", "Output", false, Some("\"data\""), true),
        ];
        let output_only = [row("output", "Output", true, None, true)];
        let expected: Vec<(&str, Vec<_>)> = vec![
            ("test.fixture.identity", at_root.to_vec()),
            ("test.fixture.wrap", at_root.to_vec()),
            ("test.fixture.trap", output_only.to_vec()),
            ("test.fixture.alloc-forever", output_only.to_vec()),
            ("test.fixture.spin", output_only.to_vec()),
            (
                "test.fixture.upper",
                vec![
                    row("text", "Expr(String)", true, None, true),
                    row("output", "Output", true, None, true),
                ],
            ),
            (
                "test.fixture.big-output",
                vec![
                    row("size", "Expr(Number)", false, None, true),
                    row("output", "Output", true, None, true),
                ],
            ),
            ("test.fixture.bad-json", output_only.to_vec()),
            ("test.fixture.bad-code", output_only.to_vec()),
            ("test.fixture.caller-input-error", output_only.to_vec()),
            ("test.fixture.internal-error", output_only.to_vec()),
        ];
        assert_eq!(f.len(), expected.len());
        for (name, fields) in expected {
            let both = &f[name];
            assert_eq!(shape(both.v4.fields()), fields, "{name}");
            let spec = dataflow_rs::Function::spec(&both.v4);
            assert_eq!(spec.description, both.v3.entry.description, "{name}");
            assert_eq!(spec.category, "transform", "{name}");
            assert_eq!(spec.retry, RetrySafety::Idempotent, "{name}: pure");
            assert!(spec.references.is_empty(), "{name}");
            assert_eq!(
                shape(spec.runtime_fields.as_deref().expect("runtime fields")),
                fields,
                "{name}"
            );
        }

        let mut registry = dataflow_rs::Registry::new(test_exprs());
        let refused = crate::engine::register_plugin_functions(
            &mut registry,
            f.values().map(|both| both.v4.clone()),
        );
        assert!(refused.is_empty(), "{refused:?}");
        let hosts: Vec<_> = registry.catalogue().filter(|info| !info.builtin).collect();
        assert_eq!(hosts.len(), 11);
        assert!(
            registry
                .catalogue()
                .any(|info| info.builtin && info.name == "map")
        );
        for info in hosts {
            assert_eq!(
                shape(info.fields),
                shape(f[info.name].v4.fields()),
                "{}",
                info.name
            );
        }
    }

    /// What a call of `inner` reads: each field `read_fields` gave it, and
    /// whether the value was lent from the config.
    struct Lending {
        inner: PluginFunction,
        lent: Arc<Mutex<Vec<(String, bool)>>>,
    }

    impl dataflow_rs::Function for Lending {
        type Config = Templated;

        fn spec(&self) -> FunctionSpec {
            dataflow_rs::Function::spec(&self.inner)
        }

        fn prepare(&self, cfg: &mut Templated, p: &Prepare<'_>) -> Result<(), Vec<Issue>> {
            dataflow_rs::Function::prepare(&self.inner, cfg, p)
        }

        async fn call(&self, ctx: &mut Ctx<'_>, cfg: &Templated) -> dataflow_rs::Result<Outcome> {
            {
                let read = self.inner.read_fields(ctx, cfg).expect("every field reads");
                let mut lent = self.lent.lock().expect("not poisoned");
                for (name, value) in read {
                    lent.push((name.to_string(), matches!(value, Cow::Borrowed(_))));
                }
            }
            dataflow_rs::Function::call(&self.inner, ctx, cfg).await
        }
    }

    /// I131: the guest's fields are read through `Templated::get`, so a
    /// literal and a template that folded to a constant are lent from the
    /// config, and a computed template and an interpolated field are
    /// resolved per call. `identity` hands back what it received, which is
    /// what dataflow-rs 3 handed it.
    #[tokio::test]
    async fn identity_reads_its_fields_through_templated_get_lending_literals_and_constants() {
        let rich = functions_of(RICH, &config());
        let identity = &rich["test.fixture.identity"];
        let input = json!({
            "message": {"var": "data.raw"},
            "label": "plain",
            "count": {"+": [1, 2]},
            "greeting": {"cat": ["hello ", {"var": "data.who"}]},
            "output": "data.echoed",
        });
        let data = json!({
            "raw": {"mti": "0200", "fields": [1, 2, 3], "ok": true, "n": 1.5},
            "who": "world",
        });
        let lent = Arc::new(Mutex::new(Vec::new()));
        let called = testing::call(
            Lending {
                inner: identity.v4.clone(),
                lent: Arc::clone(&lent),
            },
            input.clone(),
        )
        .exprs(test_exprs())
        .name(identity.v4.name())
        .message(dataflow_rs::Message::from_parts(
            OwnedDataValue::Null,
            OwnedDataValue::from(&data),
            OwnedDataValue::Null,
        ))
        .await;
        assert!(called.outcome.is_ok(), "{:?}", called.outcome);
        assert_eq!(
            *lent.lock().expect("not poisoned"),
            [
                ("message".to_string(), false),
                ("label".to_string(), true),
                ("count".to_string(), true),
                ("greeting".to_string(), false),
            ]
        );
        let echoed = json!({
            "message": {"mti": "0200", "fields": [1, 2, 3], "ok": true, "n": 1.5},
            "label": "plain",
            "count": 3,
            "greeting": "hello world",
        });
        assert_eq!(
            Value::from(called.message.context().data())["echoed"],
            echoed
        );
        let (v3, v4) = twins(identity, input, data).await;
        assert_eq!(v4, v3);
        assert_eq!(v4.expect("identity succeeds")["echoed"], echoed);
    }

    /// Each call the integration suite makes of a well-behaved fixture
    /// function writes on dataflow-rs 4 what it writes on dataflow-rs 3:
    /// the round trip, the default root, an evaluated and a literal
    /// template, and a result under the response limit.
    #[tokio::test]
    async fn a_call_writes_what_dataflow_rs_3_writes() {
        let f = fixture();
        for (name, input, data) in [
            (
                "test.fixture.identity",
                json!({"message": {"var": "data.raw"}, "output": "data.echoed"}),
                json!({"raw": {"mti": "0200", "fields": [1, 2, 3], "ok": true, "n": 1.5}}),
            ),
            (
                "test.fixture.identity",
                json!({
                    "message": [{"var": "data.n"}, {"$oid": "x"}, {"var": ["data.gone", 7]}],
                    "output": "data.out.echo"
                }),
                json!({"n": -2, "keep": "this"}),
            ),
            (
                "test.fixture.wrap",
                json!({"message": "hello"}),
                json!({"before": 1}),
            ),
            (
                "test.fixture.upper",
                json!({"text": {"cat": ["hello ", {"var": "data.who"}]}, "output": "data.up"}),
                json!({"who": "world"}),
            ),
            (
                "test.fixture.upper",
                json!({"text": "plain", "output": "data.up"}),
                json!({}),
            ),
            (
                "test.fixture.big-output",
                json!({"size": {"var": "data.n"}, "output": "data.x"}),
                json!({"n": 12}),
            ),
            (
                "test.fixture.big-output",
                json!({"size": 1000, "output": "data.x"}),
                json!({}),
            ),
        ] {
            let (v3, v4) = twins(&f[name], input.clone(), data).await;
            assert!(v3.is_ok(), "{name} {input}: {v3:?}");
            assert_eq!(v4, v3, "{name} {input}");
        }
    }

    /// Each refusal the integration suite pins at call time, on both
    /// engines with one code, text, retryability and detail: a value that
    /// resolved to the wrong kind (the schema run again over the resolved
    /// input), a template that did not evaluate, the guest's own errors,
    /// and the sandbox's.
    #[tokio::test]
    async fn a_refusal_at_call_is_dataflow_rs_3s() {
        let f = fixture();
        let out = json!({"output": "data.x"});
        for (name, input, data, needle) in [
            (
                "test.fixture.big-output",
                json!({"size": {"var": "data.size"}, "output": "data.x"}),
                json!({"size": "not a number"}),
                "expected number for 'size' (TYPE_MISMATCH)",
            ),
            (
                "test.fixture.upper",
                json!({"text": {"var": "data.missing"}, "output": "data.x"}),
                json!({}),
                "expected string for 'text' (TYPE_MISMATCH)",
            ),
            (
                "test.fixture.upper",
                json!({"text": {"throw": {"var": "data.why"}}, "output": "data.x"}),
                json!({"why": "boom"}),
                "'text' did not evaluate: ",
            ),
            // An object in a template position may be an operator call, so
            // both engines admit it at load; this one is not, so it evaluates
            // to itself, an object where the table wants a string.
            (
                "test.fixture.upper",
                json!({"text": {"no_such_operator": [{"var": "data.n"}]}, "output": "data.x"}),
                json!({"n": 1}),
                "expected string for 'text' (TYPE_MISMATCH)",
            ),
            (
                "test.fixture.caller-input-error",
                out.clone(),
                json!({}),
                "BAD_MESSAGE: the message is not ISO 8583",
            ),
            (
                "test.fixture.internal-error",
                out.clone(),
                json!({}),
                "BOOM: the guest failed with a newline",
            ),
            (
                "test.fixture.bad-code",
                out.clone(),
                json!({}),
                "ABI grammar",
            ),
            (
                "test.fixture.trap",
                out.clone(),
                json!({"keep": true}),
                "the plugin trapped",
            ),
            (
                "test.fixture.alloc-forever",
                out.clone(),
                json!({}),
                "exceeded its memory limit",
            ),
            (
                "test.fixture.spin",
                out.clone(),
                json!({}),
                "exceeded its deadline",
            ),
            ("test.fixture.bad-json", out.clone(), json!({}), "not JSON"),
        ] {
            let (v3, v4) = twins(&f[name], input.clone(), data).await;
            let failure = v3.as_ref().expect_err(name);
            assert!(
                failure.message.contains(needle),
                "{name} {input}: {failure:?}"
            );
            assert!(
                failure.message.contains(name),
                "the error names the function: {failure:?}"
            );
            assert_eq!(v4, v3, "{name} {input}");
        }
    }

    /// A template crossing `engine.ops_budget` is refused as 1.12 refused
    /// it: the caller's class, so `VALIDATION_ERROR` and not
    /// `BUDGET_EXCEEDED`, with datalogic's text under the field's name.
    #[tokio::test]
    async fn a_template_over_the_ops_budget_is_refused_as_it_was() {
        let f = fixture();
        let upper = &f["test.fixture.upper"];
        let input = json!({
            "text": {"cat": ["hello ", {"var": "data.who"}, " and ", {"var": "data.who"}]},
            "output": "data.up"
        });
        let data = json!({"who": "world"});
        let secrets = crate::engine::ResolvedSecrets::empty();

        let mut fns: HashMap<String, dataflow_rs_v3::BoxedFunctionHandler> = HashMap::new();
        fns.insert(upper.v3.name().to_string(), Box::new(upper.v3.clone()));
        let workflow: dataflow_rs_v3::Workflow = serde_json::from_value(json!({
            "id": "w", "name": "w", "condition": true,
            "tasks": [{"id": "t", "name": "t", "function": {"name": upper.v3.name(), "input": input}}]
        }))
        .expect("a workflow");
        let engine = crate::engine::operators::with_ops_budget(
            crate::engine::operators::with_orion_engine_defaults(
                dataflow_rs_v3::Engine::builder(),
                &secrets,
            ),
            2,
        )
        .with_handlers(fns)
        .with_workflows(vec![workflow])
        .build()
        .expect("the engine builds");
        let mut message = dataflow_rs_v3::Message::from_value(&json!({}));
        dataflow_rs_v3::engine::utils::set_nested_value(
            &mut message.context,
            "data",
            OwnedDataValue::from(&data),
        );
        let e = engine
            .process_message(&mut message)
            .await
            .expect_err("over the budget");
        let v3 = twin::Failure {
            code: message.errors()[0].code.clone(),
            message: e.to_string(),
            retryable: e.retryable(),
            detail: message.errors()[0].detail.clone(),
        };
        assert_eq!(v3.code, "VALIDATION_ERROR");
        assert!(v3.message.contains("'text' did not evaluate: "), "{v3:?}");
        assert!(v3.message.contains("budget exceeded"), "{v3:?}");

        let called = testing::call(upper.v4.clone(), input)
            .exprs(crate::engine::operators::orion_exprs(&secrets, 2).expect("the engine builds"))
            .name(upper.v4.name())
            .message(dataflow_rs::Message::from_parts(
                OwnedDataValue::Null,
                OwnedDataValue::from(&data),
                OwnedDataValue::Null,
            ))
            .await;
        assert_eq!(twin::seen(&called), Err(v3));
    }

    /// The size and fuel ceilings, under a plugin's own limits, refuse as
    /// they did.
    #[tokio::test]
    async fn the_limits_refuse_as_they_did() {
        let f = functions_of(
            MANIFEST,
            &PluginsConfig {
                max_response_bytes: 64 * 1024,
                max_request_bytes: 64,
                fuel_backstop: 10_000_000,
                max_timeout_ms: 5_000,
                ..config()
            },
        );
        for (name, input, needle) in [
            (
                "test.fixture.big-output",
                json!({"size": 200_000, "output": "data.x"}),
                "over the 65536 byte limit",
            ),
            (
                "test.fixture.identity",
                json!({"message": "x".repeat(200), "output": "data.x"}),
                "over the 64 byte limit",
            ),
            (
                "test.fixture.spin",
                json!({"output": "data.x"}),
                "exhausted its fuel backstop",
            ),
        ] {
            let (v3, v4) = twins(&f[name], input, json!({})).await;
            let failure = v3.as_ref().expect_err(name);
            assert!(failure.message.contains(needle), "{name}: {failure:?}");
            assert_eq!(v4, v3, "{name}");
        }
    }

    /// The permits are the function's, not an engine's: the dataflow-rs 4
    /// function holds its dataflow-rs 3 handler's semaphore, so a call on
    /// either engine waits for a permit the other holds, and is refused with
    /// 1.12's text when none frees within the ceiling.
    #[tokio::test]
    async fn the_function_and_its_dataflow_rs_3_handler_share_one_permit() {
        let f = functions_of(
            MANIFEST,
            &PluginsConfig {
                max_concurrency_per_function: 1,
                max_timeout_ms: 100,
                ..config()
            },
        );
        let identity = &f["test.fixture.identity"];
        assert!(Arc::ptr_eq(
            &identity.v3.permits,
            &identity.v4.handler.permits
        ));
        let input = json!({"message": 1, "output": "data.x"});
        let held = Arc::clone(&identity.v3.permits)
            .acquire_owned()
            .await
            .expect("the permit is free");
        let (v3, v4) = twins(identity, input.clone(), json!({})).await;
        let failure = v3.as_ref().expect_err("no permit frees");
        assert!(
            failure
                .message
                .contains("no concurrency permit within 100ms (1 already running)"),
            "{failure:?}"
        );
        assert_eq!(v4, v3);
        drop(held);
        let (v3, v4) = twins(identity, input, json!({})).await;
        assert!(v3.is_ok(), "{v3:?}");
        assert_eq!(v4, v3);
    }

    /// A run deadline before the plugin's own ceiling ends the call
    /// `TimedOut`, the engine's race, with nothing written; the ceiling's
    /// own refusal is 1.12's (above).
    #[tokio::test]
    async fn a_run_deadline_first_ends_the_call_timed_out_with_nothing_written() {
        let f = fixture();
        let called = testing::call(
            f["test.fixture.spin"].v4.clone(),
            json!({"output": "data.x"}),
        )
        .exprs(test_exprs())
        .name("test.fixture.spin")
        .deadline(RunInstant::now() + Duration::from_millis(50))
        .await;
        // The run's own `TimedOut`, not the ceiling's `TIMEOUT_ERROR`.
        let e = called.outcome.expect_err("the run ends first");
        assert_eq!(*e.kind(), ErrorKind::TimedOut);
        assert!(called.writes.is_empty());
    }

    /// What dataflow-rs 3 refused when its engine was built
    /// (`parse_input_with` over the entry's table), dataflow-rs 4 refuses at
    /// load: a required field missing, an unknown one, a literal or a
    /// template scalar of the wrong kind, an `output` that is not written as
    /// a string, a `resolvable` field that is neither its kind nor one `var`
    /// node, and an input that is not an object.
    #[tokio::test]
    async fn what_dataflow_rs_3_refused_at_load_is_refused_at_load() {
        let f = fixture();
        let rich = functions_of(RICH, &config());
        let identity = &f["test.fixture.identity"];
        let upper = &f["test.fixture.upper"];
        let any = &rich["test.fixture.identity"];
        for (both, input, v3_code, v4_at) in [
            (identity, json!({"output": "data.x"}), "REQUIRED", "message"),
            (
                upper,
                json!({"txt": "x", "text": "y", "output": "data.x"}),
                "UNKNOWN_FIELD",
                "txt",
            ),
            (
                upper,
                json!({"text": 5, "output": "data.x"}),
                "TYPE_MISMATCH",
                "text",
            ),
            (
                upper,
                json!({"text": null, "output": "data.x"}),
                "TYPE_MISMATCH",
                "text",
            ),
            (
                identity,
                json!({"message": 1, "output": {"var": "data.where"}}),
                "TYPE_MISMATCH",
                "output",
            ),
            (
                identity,
                json!({"message": 1, "output": {"cat": ["data.", "x"]}}),
                "TYPE_MISMATCH",
                "output",
            ),
            (
                identity,
                json!({"message": 1, "output": 5}),
                "TYPE_MISMATCH",
                "output",
            ),
            (
                any,
                json!({"label": 5, "output": "data.x"}),
                "TYPE_MISMATCH",
                "label",
            ),
            (
                any,
                json!({"text": 5, "output": "data.x"}),
                "TYPE_MISMATCH",
                "text",
            ),
            (
                any,
                json!({"text": null, "output": "data.x"}),
                "TYPE_MISMATCH",
                "text",
            ),
            (
                any,
                json!({"text": {"var": "data.t", "or": "x"}, "output": "data.x"}),
                "TYPE_MISMATCH",
                "text",
            ),
            (any, json!(["not", "an", "object"]), "TYPE_MISMATCH", ""),
        ] {
            let refusal = v3_refusal(&both.v3, &input).expect("dataflow-rs 3 refuses it");
            assert!(refusal.contains(v3_code), "{input}: {refusal}");
            let issues = v4_refusal(&both.v4, input.clone()).await;
            assert!(
                issues.iter().any(|(at, _)| at == v4_at),
                "{input}: {issues:?}"
            );
        }
        // And the kind 1.12 admitted, a `var` node in a `resolvable` field,
        // loads.
        let called = v4_call(
            &any.v4,
            json!({"text": {"var": "data.t"}, "output": "data.x"}),
            json!({"t": "x"}),
        )
        .await;
        assert!(called.outcome.is_ok(), "{:?}", called.issues);
    }

    /// The named breaks (I183): what dataflow-rs 3 loaded and then failed on
    /// every call is refused at load. A task naming no `output` where the
    /// function declares no default root (the field is required); an empty
    /// or blank `output`; a template whose constant is not of its kind,
    /// which 1.12 admitted as a possible operator call and refused at every
    /// call once it evaluated.
    #[tokio::test]
    async fn what_dataflow_rs_3_failed_on_every_call_is_refused_at_load() {
        let f = fixture();
        let identity = &f["test.fixture.identity"];
        let upper = &f["test.fixture.upper"];
        for (both, input, v3_needle, v4_at) in [
            (
                &f["test.fixture.trap"],
                json!({}),
                "names no 'output'",
                "output",
            ),
            (
                identity,
                json!({"message": 1, "output": ""}),
                "'output' must be a non-empty dotted context path",
                "output",
            ),
            (
                identity,
                json!({"message": 1, "output": "  "}),
                "'output' must be a non-empty dotted context path",
                "output",
            ),
            (
                upper,
                json!({"text": ["a"], "output": "data.x"}),
                "expected string for 'text' (TYPE_MISMATCH)",
                "text",
            ),
        ] {
            assert_eq!(v3_refusal(&both.v3, &input), None, "{input}: 1.12 loads it");
            let v3 = twin::v3(
                both.v3.name(),
                Box::new(both.v3.clone()),
                input.clone(),
                json!({}),
            )
            .await
            .expect_err("and refuses every call");
            assert!(v3.message.contains(v3_needle), "{input}: {v3:?}");
            let issues = v4_refusal(&both.v4, input.clone()).await;
            assert!(
                issues.iter().any(|(at, _)| at == v4_at),
                "{input}: {issues:?}"
            );
        }
        // O3's ruled class: a string naming no context root, which 1.12
        // wrote where no response shows, is no path.
        let input = json!({"message": 1, "output": "result"});
        let v3 = twin::v3(
            identity.v3.name(),
            Box::new(identity.v3.clone()),
            input.clone(),
            json!({}),
        )
        .await;
        assert_eq!(v3, Ok(json!({})), "1.12 wrote it where no response shows");
        assert_eq!(
            v4_refusal(&identity.v4, input).await,
            [("output".to_string(), IssueCode::InvalidPath)]
        );
    }

    /// A plugin never sees key material. A `{"secret": …}` read in a
    /// template field, which 1.12 refused at authoring and at load evaluated
    /// into the guest's input, is refused at load, recorded as the guest's
    /// result may be (`SECRET_IN_MESSAGE_WRITE`); in an interpolated field
    /// it is data, refused as such (`SECRET_IN_DOCUMENT`).
    #[tokio::test]
    async fn a_secret_read_in_a_plugins_input_is_refused_at_load_as_at_authoring() {
        let rich = functions_of(RICH, &config());
        let any = &rich["test.fixture.identity"];
        let registry = FunctionRegistry::builtin()
            .with_entries(vec![(*any.v3.entry).clone()])
            .expect("the entry registers");
        for (input, code) in [
            (
                json!({"greeting": {"secret": "api_key"}, "output": "data.x"}),
                IssueCode::SecretInMessageWrite,
            ),
            (
                json!({"greeting": {"cat": ["k=", {"secret": "api_key"}]}, "output": "data.x"}),
                IssueCode::SecretInMessageWrite,
            ),
            (
                json!({"message": {"k": {"secret": "api_key"}}, "output": "data.x"}),
                IssueCode::SecretInDocument,
            ),
        ] {
            let tasks = json!([{"id": "t", "function": {"name": any.v4.name(), "input": input}}]);
            assert!(
                !crate::validation::secret_reference_errors(&tasks, None, &registry).is_empty(),
                "1.12's authoring refused {input}"
            );
            let issues = v4_refusal(&any.v4, input.clone()).await;
            assert!(
                issues.iter().any(|(_, c)| *c == code),
                "{input}: {issues:?}"
            );
        }
    }

    /// Two values 1.12 refused at load that load on dataflow-rs 4, pinned
    /// for the report's concern: a `null` in an optional literal field reads
    /// as unset (I53), so the guest receives no such field; and a `null`
    /// output keeps its default root.
    #[tokio::test]
    async fn a_null_that_dataflow_rs_3_refused_at_load_loads_as_unset() {
        let rich = functions_of(RICH, &config());
        let any = &rich["test.fixture.identity"];
        let input = json!({"mode": null, "label": "x", "output": "data.echoed"});
        assert!(v3_refusal(&any.v3, &input).is_some_and(|e| e.contains("TYPE_MISMATCH")));
        let called = v4_call(&any.v4, input, json!({})).await;
        assert!(called.outcome.is_ok(), "{:?}", called.issues);
        assert_eq!(
            Value::from(called.message.context().data())["echoed"],
            json!({"label": "x"})
        );

        let identity = &fixture()["test.fixture.identity"];
        let input = json!({"message": 1, "output": null});
        assert!(v3_refusal(&identity.v3, &input).is_some_and(|e| e.contains("TYPE_MISMATCH")));
        let called = v4_call(&identity.v4, input, json!({"before": 1})).await;
        assert!(called.outcome.is_ok(), "{:?}", called.issues);
        assert_eq!(
            Value::from(called.message.context().data()),
            json!({"message": 1})
        );
    }

    /// I185 for a result that may be an object and may not: an `output` at
    /// a literal context root, the default root included, draws an Advisory
    /// at load, and a result that is not an object fails the call there
    /// (break 21), where 1.12 replaced the root with it. Below a root,
    /// nothing is said.
    #[tokio::test]
    async fn a_root_output_is_advised_at_load_and_refuses_a_result_that_is_not_an_object() {
        let f = fixture();
        let advisories = |called: &testing::Called| -> Vec<(String, IssueCode, Severity)> {
            called
                .issues
                .iter()
                .map(|i| (i.path.to_string(), i.code, i.severity))
                .collect()
        };
        let advised = [(
            "output".to_string(),
            IssueCode::InputParse,
            Severity::Advisory,
        )];

        let big = &f["test.fixture.big-output"];
        let input = json!({"size": 3, "output": "data"});
        let (v3, _) = twins(big, input.clone(), json!({"before": 1})).await;
        assert_eq!(v3, Ok(json!("xxx")), "1.12 replaced the root");
        let called = v4_call(&big.v4, input, json!({"before": 1})).await;
        assert_eq!(advisories(&called), advised);
        let e = called.outcome.expect_err("a root takes only an object");
        assert_eq!(*e.kind(), ErrorKind::Config);
        assert!(
            e.message().contains("is a root and takes only an object"),
            "{e}"
        );
        assert!(called.writes.is_empty());

        // The default root: advised, and an object written there as before.
        let wrap = &f["test.fixture.wrap"];
        let called = v4_call(&wrap.v4, json!({"message": "hello"}), json!({})).await;
        assert_eq!(advisories(&called), advised);
        assert!(called.outcome.is_ok());

        let called = v4_call(
            &wrap.v4,
            json!({"message": "hello", "output": "temp_data.wrapped"}),
            json!({}),
        )
        .await;
        assert!(called.issues.is_empty(), "{:?}", called.issues);
    }

    /// I43: a name reserved for a built-in is refused where
    /// `Registry::register` would panic on it, and the built-in keeps it;
    /// every other name registers.
    #[test]
    fn registration_refuses_a_reserved_name_where_register_would_panic() {
        let f = fixture();
        let identity = &f["test.fixture.identity"];
        let renamed = |name: &str| {
            let mut entry = (*identity.v3.entry).clone();
            entry.name = name.to_string();
            let handler = PluginFunctionHandler::new(
                Arc::new(entry),
                identity.v3.loaded.clone(),
                identity.v3.runtime.clone(),
                identity.v3.limits,
            );
            PluginFunction::new(handler, identity.v4.fields().to_vec())
        };
        let mut registry = dataflow_rs::Registry::new(test_exprs());
        let refused = crate::engine::register_plugin_functions(
            &mut registry,
            [
                renamed("map"),
                identity.v4.clone(),
                renamed("validate"),
                renamed("parse_json"),
            ],
        );
        assert_eq!(refused, ["map", "validate", "parse_json"]);
        let hosts: Vec<&str> = registry
            .catalogue()
            .filter(|info| !info.builtin)
            .map(|info| info.name)
            .collect();
        assert_eq!(hosts, ["test.fixture.identity"]);
        assert!(
            registry
                .catalogue()
                .any(|info| info.builtin && info.name == "map")
        );

        for name in dataflow_rs::def::BUILTIN_FUNCTIONS {
            assert!(crate::engine::is_reserved_function_name(name), "{name}");
        }
        assert!(crate::engine::is_reserved_function_name("validate"));
        assert!(!crate::engine::is_reserved_function_name(
            "test.fixture.identity"
        ));
        // What the refusal stands in front of.
        let mut bare = dataflow_rs::Registry::new(test_exprs());
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            bare.register("map", renamed("map"));
        }));
        assert!(panicked.is_err(), "register panics on a built-in's name");
    }
}
