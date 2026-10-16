//! `model_infer` — one inference of an admitted model, from the message to
//! the result the task writes.
//!
//! The handler holds the node's [`RuntimeHandle`] and loads the generation
//! **once per call**, as `channel_call` does: the entry it resolves — the
//! adapters, the result expression, the limits — comes off one load. The
//! adapters were compiled on the process's expression engine (I188), which
//! the model set holds and the call evaluates them on, so a reload
//! mid-workflow can never pair adapters with an engine that did not compile
//! them. The loaded session comes from the process-wide [`LoadedCache`] and
//! outlives generations.
//!
//! Two functions run it: [`ModelInferHandler`] on dataflow-rs 3, which
//! serves, and [`ModelInferFunction`] on dataflow-rs 4. Each reads the
//! task's fields as its engine compiled them and hands them to one core
//! (`infer`), so the sequence, the refusals and what is written are one
//! code path.
//!
//! The sequence, and what each step refuses as:
//!
//! 1. the node has the runtime and the generation serves the model —
//!    else `unavailable`, with the load issue's reason when there is one;
//! 2. the runtime: the task's `runtime` or `[models.default_runtime]`'s row
//!    for the manifest's format, checked on this node — `runtime_unavailable`;
//! 3. the adapters, evaluated on `input` — an evaluation error is `adapter`
//!    (an `engine.ops_budget` refusal keeps its `BUDGET_EXCEEDED` code), a
//!    value that is not the declared tensor is `caller_input`, and more
//!    elements than `max_input_elements` is `input_size`. Before the load,
//!    so a message that does not marshal never pays a cold load;
//! 4. under the deadline — the shorter of `timeout_ms` and the model's
//!    ceiling — the load if the session is cold (`unavailable` on failure,
//!    the cold load charged to the deadline), then a global and a per-model
//!    permit (`permit` when none frees up in time), then the run on the
//!    blocking pool (`run`; the deadline elapsing anywhere is `timeout`).
//!    On dataflow-rs 4, a run whose own deadline comes first arms no timer
//!    of the model's: the engine ends the call `TimedOut` at the run's
//!    deadline, as 1.12's channel timeout ended the run;
//! 5. the outputs, checked against the manifest (`run` on a mismatch,
//!    `output_size` over `max_output_elements`), then written raw as
//!    `{name: tensor}`, reduced by the task's own `select`, or passed
//!    through the manifest's result expression (`adapter` when either
//!    expression fails).
//!
//! Nothing is written on failure. Every category is one of
//! [`Category`]'s, which is what the failure metric counts by, and the
//! `model` and `runtime` labels are the entry's id and the runtime's own
//! name — never a string the message chose.
//!
//! **Offline**, `dry-run` and `orion-server test` run the same handler over
//! a [`ModelSource::Offline`]: the set is built from the manifests a
//! `--model-dir` holds ([`super::offline::OfflineModels`]), compiled lazily
//! on the offline run's expression engine, and a cold load reads the file
//! beside the manifest rather than fetching through a storage connector.
//! That is the one seam between the two: [`ArtifactSource`] is how a load
//! gets its bytes, and [`InferenceHost`] carries it beside the runtimes, the
//! resident sessions and the inference slots. Nothing about the adapters,
//! the limits or the refusals differs, so what a dry run reports is what a
//! node would do with the same bytes — admission excepted: no digest is
//! claimed and no probe runs offline, because the bytes on disk are the
//! author's own and are what the run exists to try. Without a `--model-dir`
//! the function is stubbed like a connector function, keyed by its name.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dataflow_rs::datavalue::{OwnedDataTensor, OwnedDataValue};
use dataflow_rs::expr::ExprEngine;
use dataflow_rs::function::{FunctionSpec, Prepare};
use dataflow_rs::run::ErrorKind;
use dataflow_rs::{Ctx, Expr, Function, Issue, Outcome};
use dataflow_rs_v3::datalogic_rs as datalogic;
use dataflow_rs_v3::engine::error::DataflowError;
use dataflow_rs_v3::engine::functions::AsyncFunctionHandler;
use dataflow_rs_v3::engine::task_context::TaskContext;
use dataflow_rs_v3::engine::task_outcome::TaskOutcome;
use serde_json::{Value, json};
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio::time::Instant;

use super::artifact::ArtifactStore;
use super::cache::{CacheKey, LoadedCache};
use super::error::{Category, Failure};
use super::loader::{ModelEntry, ModelSet};
use super::node::ModelsRuntime;
use super::offline::OfflineModels;
use super::runtimes::{LoadError, LoadedModel, ModelRuntime, ModelRuntimes};
use crate::config::ModelsConfig;
use crate::connector::{ConnectorConfig, ConnectorRegistry};
use crate::engine::HandlerError;
use crate::engine::functions::model_infer::ModelInferConfig;
use crate::engine::functions::templated_input::{TemplatedInput, with_scope};
use crate::engine::kit;
use crate::runtime::RuntimeHandle;

/// This handler's name in metrics, profiles and error messages.
pub const NAME: &str = "model_infer";

/// A `model_infer` task's input: the fields as authored, with the template
/// fields compiled, and `select` compiled once as an expression over the
/// outputs rather than evaluated against the message.
#[derive(Debug, Default)]
pub struct InferInput {
    fields: TemplatedInput,
    select: Option<Arc<datalogic::Logic>>,
}

impl<'de> serde::Deserialize<'de> for InferInput {
    /// Any JSON, as [`TemplatedInput`] takes: the field table validates it.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self {
            fields: TemplatedInput::deserialize(deserializer)?,
            select: None,
        })
    }
}

/// The key the call's `input` root sits under in `select`'s context, beside
/// the outputs.
const SELECT_INPUT_KEY: &str = "input";

/// Where the result lands when the task names no `output`.
pub const DEFAULT_OUTPUT: &str = "temp_data.inference";

/// How a cold load gets a model's bytes.
///
/// On a node the bytes come out of the digest-keyed disk cache, fetched
/// through the entry's storage connector when the cache does not hold them
/// ([`NodeArtifacts`]); offline they are the file beside the manifest
/// ([`super::offline::LocalArtifacts`]). Everything after the bytes — the
/// parse into a runtime, the cache, the permits — is shared.
#[async_trait]
pub trait ArtifactSource: Send + Sync {
    /// The bytes of `entry`'s artifact, or the stage that could not produce
    /// them.
    async fn bytes(&self, entry: &ModelEntry) -> Result<Vec<u8>, LoadError>;
}

/// A node's artifacts: the store every admission filled, the connector
/// registry the entry's reference resolves through, and the fetch ceilings.
pub struct NodeArtifacts {
    pub store: Arc<ArtifactStore>,
    pub registry: Arc<ConnectorRegistry>,
    pub client: reqwest::Client,
    pub config: Arc<ModelsConfig>,
}

#[async_trait]
impl ArtifactSource for NodeArtifacts {
    async fn bytes(&self, entry: &ModelEntry) -> Result<Vec<u8>, LoadError> {
        let name = entry.artifact.connector.as_str();
        let storage = match self.registry.get(name).await {
            Some(connector) => match connector.as_ref() {
                ConnectorConfig::Storage(storage) => storage.clone(),
                other => {
                    return Err(LoadError::new(
                        "gate",
                        format!(
                            "connector '{name}' is a {} connector, and a model artifact is read \
                             through a storage connector",
                            other.connector_type().as_str()
                        ),
                    ));
                }
            },
            None => {
                return Err(LoadError::new(
                    "gate",
                    format!(
                        "connector '{name}' is not loaded on this node — it does not exist, is \
                         disabled, or failed to load (see /health)"
                    ),
                ));
            }
        };
        let path = self
            .store
            .fetch(
                &storage,
                &self.client,
                &entry.artifact,
                self.config.max_artifact_bytes,
                Duration::from_secs(self.config.fetch_timeout_secs),
            )
            .await
            .map_err(|e| LoadError::new(e.stage(), e.to_string()))?;
        tokio::fs::read(&path).await.map_err(|e| {
            LoadError::new(
                "cache",
                format!("the cached artifact could not be read: {e}"),
            )
        })
    }
}

/// What an inference runs on, wherever it runs: the runtimes to load into,
/// the sessions already resident, the process-wide slots, and where a cold
/// load gets its bytes. A node builds one over its [`ModelsRuntime`]; an
/// offline run builds one over the files a `--model-dir` holds.
pub struct InferenceHost {
    pub runtimes: Arc<ModelRuntimes>,
    pub loaded: Arc<LoadedCache>,
    pub inference_permits: Arc<Semaphore>,
    pub inference_slots: usize,
    /// Cold-load slots, `None` when `models.max_concurrent_loads` is unbounded.
    /// Held across the fetch and parse of a cold load, so a burst of distinct
    /// cold models cannot hold more than this many artifacts resident at once.
    pub load_permits: Option<Arc<Semaphore>>,
    pub artifacts: Arc<dyn ArtifactSource>,
}

impl InferenceHost {
    /// The node's: its runtimes, cache and slots shared by `Arc`, so the
    /// handler, the preload and the health view all see one set of resident
    /// sessions.
    pub fn node(
        models: &ModelsRuntime,
        config: Arc<ModelsConfig>,
        registry: Arc<ConnectorRegistry>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            runtimes: models.runtimes.clone(),
            loaded: models.loaded.clone(),
            inference_permits: models.inference_permits.clone(),
            inference_slots: models.inference_slots,
            load_permits: models.load_permits.clone(),
            artifacts: Arc::new(NodeArtifacts {
                store: models.store.clone(),
                registry,
                client,
                config,
            }),
        }
    }

    /// An offline host: the runtimes `config` enables, a fresh session
    /// cache bounded by `config.max_loaded_bytes`, slots as the config says,
    /// and `artifacts` for the bytes. No disk cache and no admission queue —
    /// nothing is fetched and nothing is admitted.
    pub fn offline(config: &ModelsConfig, artifacts: Arc<dyn ArtifactSource>) -> Self {
        let inference_slots = match config.max_concurrent_inferences {
            0 => std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
            n => n as usize,
        };
        Self {
            runtimes: Arc::new(ModelRuntimes::builtin(config)),
            loaded: Arc::new(LoadedCache::new(config.max_loaded_bytes)),
            inference_permits: Arc::new(Semaphore::new(inference_slots)),
            inference_slots,
            load_permits: super::node::load_permits(config),
            artifacts,
        }
    }
}

/// Where a call finds the model set — the entries and the expression engine
/// their adapters were compiled on, which the set holds as one pair.
pub enum ModelSource {
    /// A node: the serving generation, loaded once per call.
    Node(Arc<RuntimeHandle>),
    /// An offline run: a fixed set of manifests, compiled the first time a
    /// task asks and reused for the rest of the run.
    Offline(Arc<OfflineModels>),
}

impl ModelSource {
    /// The set this call resolves against: one load of the serving
    /// generation's, or the offline run's.
    fn set(&self) -> Arc<ModelSet> {
        match self {
            Self::Node(runtime) => runtime.load().models.clone(),
            Self::Offline(offline) => offline.set(),
        }
    }
}

/// The `model_infer` task function on dataflow-rs 3.
pub struct ModelInferHandler {
    pub source: ModelSource,
    /// What the call runs on — `None` with `models.enabled = false`, which
    /// makes every call an `unavailable` failure rather than a build error.
    pub host: Option<Arc<InferenceHost>>,
    pub config: Arc<ModelsConfig>,
}

#[async_trait]
impl AsyncFunctionHandler for ModelInferHandler {
    type Input = InferInput;

    /// Compile the expression fields the table declares (`model`, `input`,
    /// `timeout_ms`), and `select` as an expression of its own: it reads the
    /// outputs, which do not exist until the model has run, so it is
    /// compiled here and evaluated after the run, never templated against
    /// the message.
    fn compile_input(
        input: &mut Self::Input,
        c: &dataflow_rs_v3::engine::functions::TemplateCompiler,
    ) -> dataflow_rs_v3::Result<()> {
        input.fields.compile(NAME, c)?;
        input.select = match input.fields.get("select") {
            None | Some(Value::Null) => None,
            Some(select) => Some(Arc::new(c.engine().compile(select).map_err(|e| {
                DataflowError::Validation(format!("{NAME}: 'select' does not compile: {e}"))
            })?)),
        };
        Ok(())
    }

    /// The shell around the core, then the writes: the result at `output`,
    /// the stats at `stats_output` when it is set.
    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &InferInput,
    ) -> dataflow_rs_v3::Result<TaskOutcome> {
        let started = Instant::now();
        let result = self.run(ctx, input, started).await;
        let Inferred {
            value,
            stats,
            writes: (output, stats_output),
            ..
        } = settle(started, result)?;
        // Written last, together: nothing lands on a failure above.
        ctx.set(output, value);
        if let (Some(path), Some(stats)) = (stats_output, stats) {
            ctx.set_json(path, &stats);
        }
        Ok(TaskOutcome::Success)
    }
}

impl ModelInferHandler {
    /// The task's fields as dataflow-rs 3 compiled them, read for this
    /// message, and the core over them. The fields that read the message
    /// are resolved together against one view of it; the rest are read as
    /// written, each refused in 1.12's words when it is not what the field
    /// takes.
    async fn run<'i>(
        &self,
        ctx: &mut TaskContext<'_>,
        infer_input: &'i InferInput,
        started: Instant,
    ) -> Result<Inferred<(&'i str, Option<&'i str>)>, Refused> {
        let input = &infer_input.fields;
        let (model_field, timeout_field, root_field) = with_scope(ctx, |scope| {
            (
                input.resolve_owned("model", scope),
                input.value_of("timeout_ms", NAME, scope),
                input.resolve_owned("input", scope),
            )
        });
        let raw = match input.get("raw") {
            None | Some(Value::Null) => Ok(false),
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(caller_input(None, "'raw' must be a boolean".to_string())),
        };
        let writes = if matches!(raw, Ok(true)) && infer_input.select.is_some() {
            Err(caller_input(
                None,
                "'raw' and 'select' are exclusive: 'raw' writes the outputs whole, 'select' \
                 reduces them first"
                    .to_string(),
            ))
        } else {
            v3_path(input, "output", Some(DEFAULT_OUTPUT)).and_then(|output| {
                v3_path(input, "stats_output", None)
                    .map(|stats_output| (output.unwrap_or(DEFAULT_OUTPUT), stats_output))
            })
        };
        // `select` evaluates on the engine running this task, the one that
        // compiled it.
        let datalogic = ctx.datalogic().clone();
        let request = Request {
            started,
            model: match model_field {
                None => Err(caller_input(None, "requires 'model' (string)".to_string())),
                Some(Err(e)) => Err(field_refused_v3("model", e)),
                Some(Ok(value)) => Ok(value),
            },
            runtime: match input.get("runtime") {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(name)) => Ok(Some(name.as_str())),
                Some(_) => Err(caller_input(None, "'runtime' must be a string".to_string())),
            },
            timeout_ms: match timeout_field {
                None | Some(Ok(Value::Null)) => Ok(None),
                Some(Err(e)) => Err(field_refused_v3("timeout_ms", e)),
                Some(Ok(v)) => Ok(Some(v)),
            },
            raw,
            stats: matches!(writes, Ok((_, Some(_)))),
            writes,
            root: match root_field {
                None => Err(caller_input(
                    None,
                    "requires 'input': the JSON root the manifest's adapters read".to_string(),
                )),
                Some(Err(e)) => Err(field_refused_v3("input", e)),
                Some(Ok(root)) => Ok(root),
            },
            select: infer_input
                .select
                .as_deref()
                .map(|logic| Select::V3(&datalogic, logic)),
            run_deadline: None,
        };
        infer(&self.source, self.host.as_ref(), &self.config, request).await
    }
}

/// A path field read as 1.12 read it: unset or `null` is `default`, a
/// non-empty string is the path, and anything else is refused.
fn v3_path<'i>(
    input: &'i TemplatedInput,
    field: &str,
    default: Option<&'static str>,
) -> Result<Option<&'i str>, Refused> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::String(path)) if !path.is_empty() => Ok(Some(path.as_str())),
        Some(_) => Err(caller_input(
            None,
            format!("'{field}' must be a dotted path (string)"),
        )),
    }
}

/// The `model_infer` task function on dataflow-rs 4: the inference
/// [`ModelInferHandler`] runs, over the same model set and host, its fields
/// read in the same order. `output` is the primary output, beside
/// `stats_output`.
pub struct ModelInferFunction {
    pub source: ModelSource,
    /// What the call runs on — `None` with `models.enabled = false`, which
    /// makes every call an `unavailable` failure rather than a build error.
    pub host: Option<Arc<InferenceHost>>,
    pub config: Arc<ModelsConfig>,
}

impl Function for ModelInferFunction {
    type Config = ModelInferConfig;

    fn spec(&self) -> FunctionSpec {
        kit::spec(NAME)
    }

    /// I185: a literal context root at `output` is an Advisory unless the
    /// call writes the outputs `raw`, an object.
    fn prepare(&self, cfg: &mut ModelInferConfig, _p: &Prepare<'_>) -> Result<(), Vec<Issue>> {
        cfg.root_output()
    }

    /// The three fields that read the message, evaluated in 1.12's order;
    /// the core over them and the config's literals; then the writes, the
    /// result through the primary output and the stats beside it.
    async fn call(
        &self,
        ctx: &mut Ctx<'_>,
        cfg: &ModelInferConfig,
    ) -> dataflow_rs::Result<Outcome> {
        let started = Instant::now();
        let model = ctx
            .eval(&cfg.model)
            .map(Cow::into_owned)
            .map_err(|e| field_refused("model", e));
        let timeout_ms = match &cfg.timeout_ms {
            None => Ok(None),
            Some(expr) => match ctx.eval(expr) {
                Ok(value) if value.is_null() => Ok(None),
                Ok(value) => Ok(Some(Value::from(&*value))),
                Err(e) => Err(field_refused("timeout_ms", e)),
            },
        };
        let root = ctx
            .eval(&cfg.input)
            .map(Cow::into_owned)
            .map_err(|e| field_refused("input", e));
        // `select` evaluates on the engine its config compiled on.
        let exprs = ctx.exprs().clone();
        let request = Request {
            started,
            model,
            runtime: Ok(cfg.runtime.as_deref()),
            timeout_ms,
            raw: Ok(cfg.raw.unwrap_or(false)),
            writes: Ok(()),
            root,
            select: cfg
                .select
                .as_ref()
                .map(|select| Select::V4(&exprs, select.expr())),
            stats: cfg.stats_output.is_some(),
            run_deadline: ctx.deadline(),
        };
        let inferred = settle(
            started,
            infer(&self.source, self.host.as_ref(), &self.config, request).await,
        )?;
        ctx.emit_default(inferred.value)?;
        if let (Some(out), Some(stats)) = (&cfg.stats_output, inferred.stats) {
            ctx.emit(out, OwnedDataValue::from(&stats))?;
        }
        Ok(Outcome::Ok)
    }
}

/// One call's fields, as the engine running it read them for this message:
/// each its value, or the refusal 1.12 gave for it. [`infer`] consumes them
/// in 1.12's order, so the first refusal a call reports is the one it
/// always reported. `W` is where the call writes, which only the engine
/// running it knows how to name.
struct Request<'s, W> {
    /// When the call started: the model's own deadline counts from here.
    started: Instant,
    model: Result<OwnedDataValue, Refused>,
    /// `None` for `[models.default_runtime]`'s row for the format.
    runtime: Result<Option<&'s str>, Refused>,
    /// The value `timeout_ms` gave, as the JSON 1.12 judged; `None` when it
    /// is unset or gave `null`.
    timeout_ms: Result<Option<Value>, Refused>,
    raw: Result<bool, Refused>,
    /// Where the result goes, and whatever 1.12 refused between `raw` and
    /// the root: `raw` beside `select`, an output that is not a path.
    writes: Result<W, Refused>,
    /// The `input` field's value: the root every adapter reads.
    root: Result<OwnedDataValue, Refused>,
    select: Option<Select<'s>>,
    /// Whether `stats_output` is set, and so whether the stats are built.
    stats: bool,
    /// The deadline of the run the call belongs to (`Ctx::deadline`); none
    /// on dataflow-rs 3, whose channel timeout ends the run instead.
    run_deadline: Option<dataflow_rs::run::Instant>,
}

/// The task's own `select`, with the engine that compiled it.
enum Select<'s> {
    /// dataflow-rs 3's, compiled by its `TemplateCompiler`.
    V3(&'s datalogic::Engine, &'s datalogic::Logic),
    /// dataflow-rs 4's, a [`kit::DetachedExpr`] compiled on the program's
    /// expression engine.
    V4(&'s ExprEngine, &'s Expr<OwnedDataValue>),
}

impl Select<'_> {
    /// `select` over `root`, the outputs by name and the call's input, and
    /// what it charged; `adapter` when it fails.
    fn eval(
        &self,
        root: &OwnedDataValue,
        labels: &Labels,
    ) -> Result<(OwnedDataValue, u64), Refused> {
        const CONTEXT: &str = "the select expression failed";
        match self {
            Self::V3(engine, logic) => evaluate(engine, logic, root)
                .map_err(|e| datalogic_refused(Some(labels), Category::Adapter, CONTEXT, &e)),
            Self::V4(exprs, expr) => exprs
                .eval(expr, root)
                .map(|evaluated| (evaluated.value.into_owned(), evaluated.ops.unwrap_or(0)))
                .map_err(|e| evaluation_refused(Some(labels), Category::Adapter, CONTEXT, e)),
        }
    }
}

/// What a call produced, for the engine running it to write: the result,
/// the stats when `stats_output` is set, and where they go.
struct Inferred<W> {
    labels: Labels,
    value: OwnedDataValue,
    stats: Option<Value>,
    writes: W,
}

/// The shell around one call, on either engine: the inference counted
/// under its outcome, a failure's operator-only detail logged, and its
/// error named once with the handler's name.
fn settle<W>(
    started: Instant,
    result: Result<Inferred<W>, Refused>,
) -> Result<Inferred<W>, HandlerError> {
    match result {
        Ok(inferred) => {
            crate::metrics::record_model_inference(
                &inferred.labels.model,
                inferred.labels.runtime,
                "ok",
                started.elapsed().as_secs_f64(),
            );
            Ok(inferred)
        }
        Err(refused) => {
            if let Some(labels) = &refused.labels {
                crate::metrics::record_model_inference(
                    &labels.model,
                    labels.runtime,
                    "error",
                    started.elapsed().as_secs_f64(),
                );
                crate::metrics::record_model_failure(
                    &labels.model,
                    labels.runtime,
                    refused.category.as_str(),
                );
            }
            if let Some(detail) = &refused.error.detail {
                tracing::warn!(
                    model = refused.labels.as_ref().map(|l| l.model.as_str()),
                    category = refused.category.as_str(),
                    detail = %detail,
                    "model_infer failed"
                );
            }
            Err(refused.error.prefixed(NAME))
        }
    }
}

/// The two metric labels, known once the model and the runtime resolve.
#[derive(Clone)]
struct Labels {
    model: String,
    runtime: &'static str,
}

/// A refused call: the category it counts under, the error the task sees,
/// and the labels when the call got far enough to have them.
struct Refused {
    labels: Option<Labels>,
    category: Category,
    error: HandlerError,
}

impl Refused {
    /// The refusal, counted under `labels`: a field's, read before the
    /// labels were known and consumed after.
    fn labelled(mut self, labels: &Labels) -> Self {
        self.labels = Some(labels.clone());
        self
    }
}

fn refuse(labels: Option<&Labels>, failure: Failure) -> Refused {
    Refused {
        labels: labels.cloned(),
        category: failure.category,
        error: failure.into_handler_error(),
    }
}

/// An evaluation error under `category`, with the engine's own text kept
/// so the caller sees what the expression did — and an `engine.ops_budget`
/// refusal, `ErrorKind::Budget`, carried through as itself under `context`,
/// so its `BUDGET_EXCEEDED` code survives the trip through the handler
/// shell.
fn evaluation_refused(
    labels: Option<&Labels>,
    category: Category,
    context: &str,
    e: dataflow_rs::Error,
) -> Refused {
    if *e.kind() == ErrorKind::Budget {
        return Refused {
            labels: labels.cloned(),
            category,
            error: HandlerError::from(e.wrap(context)),
        };
    }
    refuse(
        labels,
        Failure::new(category, format!("{context}: {}", e.message())),
    )
}

/// [`evaluation_refused`] for dataflow-rs 3's `select`, compiled on its own
/// datalogic engine, whose budget refusal is told by its tag.
fn datalogic_refused(
    labels: Option<&Labels>,
    category: Category,
    context: &str,
    e: &datalogic::Error,
) -> Refused {
    if e.tag() == "BudgetExceeded" {
        return Refused {
            labels: labels.cloned(),
            category,
            error: HandlerError::from(DataflowError::BudgetExceeded(format!("{context}: {e}"))),
        };
    }
    refuse(labels, Failure::new(category, format!("{context}: {e}")))
}

/// A field of the task's own that did not evaluate, on dataflow-rs 4: the
/// error under the field's name, its kind kept, so a budget refusal in
/// `model` or `input` keeps its code.
fn field_refused(field: &str, e: dataflow_rs::Error) -> Refused {
    Refused {
        labels: None,
        category: Category::CallerInput,
        error: HandlerError::from(e.wrap(format!("'{field}'"))),
    }
}

/// [`field_refused`] on dataflow-rs 3: the `DataflowError` is kept as the
/// original, so a budget refusal keeps its code.
fn field_refused_v3(field: &str, e: DataflowError) -> Refused {
    let mut error = HandlerError::from(e);
    error.msg = format!("'{field}': {}", error.msg);
    Refused {
        labels: None,
        category: Category::CallerInput,
        error,
    }
}

fn caller_input(labels: Option<&Labels>, message: String) -> Refused {
    refuse(labels, Failure::new(Category::CallerInput, message))
}

/// `[1,2,6,7]` as `1x2x6x7`-style text for a message. Takes a declared
/// shape as readily as an actual one, so a refusal can put `1,2,H,7`
/// beside the `1,2,5,7` that did not fit it.
fn dims<D: std::fmt::Display>(shape: &[D]) -> String {
    shape
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// What kind of value an adapter produced, for the message that says it
/// was not a tensor.
fn kind_of(value: &OwnedDataValue) -> &'static str {
    match value {
        OwnedDataValue::Null => "null",
        OwnedDataValue::Bool(_) => "a boolean",
        OwnedDataValue::Number(_) => "a number",
        OwnedDataValue::String(_) => "a string",
        OwnedDataValue::Array(_) => "a list",
        OwnedDataValue::Object(_) => "an object",
        OwnedDataValue::Tensor(_) => "a tensor",
        _ => "another value",
    }
}

/// Evaluate dataflow-rs 3's compiled `select` over `root` on `engine`, the
/// one that compiled it, and take the result out of the arena.
fn evaluate(
    engine: &datalogic::Engine,
    logic: &datalogic::Logic,
    root: &OwnedDataValue,
) -> Result<(OwnedDataValue, u64), datalogic::Error> {
    let arena = datalogic::bumpalo::Bump::new();
    engine
        .evaluate_metered(logic, root, &arena, engine.resolve_ops_budget(None))
        .map(|metered| (metered.value.to_owned(), metered.ops))
}

/// What one call's expressions charged: every input adapter and, unless the
/// task asked for `raw`, the task's `select` or the manifest's result.
///
/// Two numbers because they answer different questions. `total` is what the
/// message cost. `peak` is what a ceiling has to clear, because
/// `engine.ops_budget` bounds one *evaluation* and a call is several: a
/// manifest with four adapters at 300 operations each is refused by a
/// budget of 250 and admitted by one of 400, whatever the total says.
#[derive(Default)]
struct Charged {
    total: u64,
    peak: u64,
}

impl Charged {
    fn add(&mut self, ops: u64) {
        self.total = self.total.saturating_add(ops);
        self.peak = Ord::max(self.peak, ops);
    }
}

/// A permit from `permits` before `deadline`, or a `permit` refusal naming
/// the ceiling that was full. With no deadline of the call's own, the wait
/// is the run's to end.
async fn acquire<'s>(
    permits: &'s Semaphore,
    deadline: Option<Instant>,
    ceiling: &str,
    labels: &Labels,
) -> Result<SemaphorePermit<'s>, Refused> {
    let acquired = match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, permits.acquire()).await,
        None => Ok(permits.acquire().await),
    };
    match acquired {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_closed)) => Err(refuse(
            Some(labels),
            Failure::new(Category::Permit, "the inference slots are closed"),
        )),
        Err(_elapsed) => Err(refuse(
            Some(labels),
            Failure::new(
                Category::Permit,
                format!("no inference slot freed up before the deadline ({ceiling})"),
            ),
        )),
    }
}

/// The call, on either engine, from the fields read to the value written.
async fn infer<W>(
    source: &ModelSource,
    host: Option<&Arc<InferenceHost>>,
    config: &ModelsConfig,
    request: Request<'_, W>,
) -> Result<Inferred<W>, Refused> {
    // One set for this call: the entry, its adapters and the engine they
    // were compiled on, off one load of the generation on a node, or the
    // offline set.
    let set = source.set();
    let exprs = set.exprs();
    let Some(host) = host else {
        return Err(refuse(
            None,
            Failure::new(
                Category::Unavailable,
                "models are disabled on this node (models.enabled = false)",
            ),
        ));
    };

    // 1. The model.
    let value = request.model?;
    let model_id = match value.as_str() {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => {
            return Err(caller_input(
                None,
                format!(
                    "'model' must evaluate to a model id (string), not {}",
                    kind_of(&value)
                ),
            ));
        }
    };
    let Some(entry) = set.get(&model_id).cloned() else {
        let reason = set
            .issue_for(&model_id)
            .map(|issue| format!("{}: {}", issue.stage, issue.reason))
            .unwrap_or_else(|| "it is unknown or not active".to_string());
        return Err(refuse(
            None,
            Failure::new(
                Category::Unavailable,
                format!("model '{model_id}' is unavailable on this node: {reason}"),
            ),
        ));
    };

    // 2. The runtime, and with it the labels.
    let format = entry.manifest.format.as_str();
    let runtime_name = match request.runtime? {
        None => match config.default_runtime_for(format) {
            Some(name) => name,
            None => {
                return Err(refuse(
                    None,
                    Failure::new(
                        Category::RuntimeUnavailable,
                        super::runtimes::RuntimeSelection::NoDefault {
                            format: format.to_string(),
                        }
                        .to_string(),
                    ),
                ));
            }
        },
        Some(name) => name,
    };
    let (runtime, device) = host
        .runtimes
        .for_format(config, runtime_name, format)
        .map_err(|selection| {
            refuse(
                None,
                Failure::new(Category::RuntimeUnavailable, selection.to_string()),
            )
        })?;
    let labels = Labels {
        model: entry.id.clone(),
        runtime: runtime.name(),
    };

    // The deadline evaluates, like `channel_call`'s and `http_call`'s: a
    // workflow running several inferences under one wall-clock budget
    // has to divide it per message, so each call spends its own share
    // and times itself out rather than starving the ones behind it.
    let timeout_ms = match request.timeout_ms.map_err(|r| r.labelled(&labels))? {
        None => None,
        Some(v) => match v.as_u64() {
            Some(ms) if ms > 0 => Some(ms),
            _ => {
                return Err(caller_input(
                    Some(&labels),
                    "'timeout_ms' must be a positive integer".to_string(),
                ));
            }
        },
    };
    let budget = timeout_ms
        .map(Duration::from_millis)
        .map_or(entry.limits.timeout, |t| t.min(entry.limits.timeout));
    // The earlier bound wins. When the model's comes first, the load, the
    // permits and the run are timed against it, and fail as they always
    // have. When the run's comes first, or with it, the call arms no timer
    // of its own: dataflow-rs 4 races every host call against the run's
    // deadline and ends the call `TimedOut` when it passes, as 1.12's
    // channel timeout ended the run. A timer of the model's at the same
    // instant would win that race (the engine polls the call first) and
    // turn the run's timeout into the task's error.
    let own = request.started + budget;
    let deadline = match request.run_deadline.map(Instant::from_std) {
        Some(run) if run <= own => None,
        _ => Some(own),
    };
    let raw = request.raw.map_err(|r| r.labelled(&labels))?;
    let writes = request.writes.map_err(|r| r.labelled(&labels))?;

    // 3. The adapters, on the set's engine, before anything is loaded: a
    // message that does not marshal costs no cold load.
    let root = request.root.map_err(|r| r.labelled(&labels))?;
    let mut tensors: Vec<OwnedDataTensor> = Vec::with_capacity(entry.adapters.inputs.len());
    let mut input_elements = 0usize;
    // What the manifest's own half of the call spends. The adapters run
    // here, before the load and the permits, so their cost is in
    // neither `queued_ms` nor `inference_ms` — and without this it was
    // in nothing at all.
    let mut charged = Charged::default();
    // What this call binds each named dimension to, filled by the
    // inputs in order and then held against the outputs.
    let mut bindings = crate::model::manifest::Bindings::default();
    for ((name, adapter), decl) in entry.adapters.inputs.iter().zip(&entry.manifest.inputs) {
        let evaluated = exprs.eval(adapter, &root).map_err(|e| {
            evaluation_refused(
                Some(&labels),
                Category::Adapter,
                &format!("adapter for input '{name}' failed"),
                e,
            )
        })?;
        charged.add(evaluated.ops.unwrap_or(0));
        let tensor = match evaluated.value.into_owned() {
            OwnedDataValue::Tensor(tensor) => {
                Arc::try_unwrap(tensor).unwrap_or_else(|shared| (*shared).clone())
            }
            other => {
                return Err(caller_input(
                    Some(&labels),
                    format!(
                        "input '{name}': the adapter produced {}, not a tensor",
                        kind_of(&other)
                    ),
                ));
            }
        };
        // The declared shape may name a dimension, and a name binds on
        // its first occurrence in the call: every later one — in
        // another input, or in an output below — is held to it.
        let fits = tensor.dtype().name() == decl.dtype
            && bindings.check(&decl.shape, tensor.shape()).is_ok();
        if !fits {
            return Err(caller_input(
                Some(&labels),
                format!(
                    "input '{name}': expected {}[{}], got {}[{}]",
                    decl.dtype,
                    dims(&decl.shape),
                    tensor.dtype(),
                    dims(tensor.shape())
                ),
            ));
        }
        input_elements = input_elements.saturating_add(tensor.numel());
        if input_elements > entry.limits.max_input_elements {
            return Err(refuse(
                Some(&labels),
                Failure::new(
                    Category::InputSize,
                    format!(
                        "the inputs total more than {} elements (models.max_input_elements)",
                        entry.limits.max_input_elements
                    ),
                ),
            ));
        }
        tensors.push(tensor);
    }

    // 4. Under the deadline: the session, the permits, the run. A
    // blocking parse or inference cannot be cancelled; the deadline
    // bounds how long this task waits for it, not the thread it runs on.
    let budget_ms = budget.as_millis() as u64;
    let work = async {
        let (loaded, cold_load) =
            load_model(host, entry.clone(), runtime.clone(), device, "demand")
                .await
                .map_err(|e| {
                    refuse(
                        Some(&labels),
                        Failure::new(
                            Category::Unavailable,
                            format!(
                                "model '{}' is not loaded on this node: the {} step failed",
                                entry.id, e.stage
                            ),
                        )
                        .with_detail(e.message),
                    )
                })?;

        let queued = Instant::now();
        let _global = acquire(
            &host.inference_permits,
            deadline,
            "models.max_concurrent_inferences",
            &labels,
        )
        .await?;
        let _own = acquire(
            &entry.permits,
            deadline,
            "models.max_concurrency_per_model",
            &labels,
        )
        .await?;
        let queued_for = queued.elapsed();
        crate::metrics::record_model_queue_time(
            &labels.model,
            labels.runtime,
            queued_for.as_secs_f64(),
        );
        crate::metrics::set_model_live_inferences(
            host.inference_slots
                .saturating_sub(host.inference_permits.available_permits()) as u64,
        );

        let run_started = Instant::now();
        let outputs = tokio::task::spawn_blocking(move || loaded.run(tensors))
            .await
            .map_err(|e| {
                refuse(
                    Some(&labels),
                    Failure::new(Category::Run, "the inference did not complete").with_detail(e),
                )
            })?
            .map_err(|e| {
                refuse(
                    Some(&labels),
                    Failure::new(
                        Category::Run,
                        format!("the model failed to run ({} stage)", e.stage),
                    )
                    .with_detail(e.message),
                )
            })?;
        Ok::<_, Refused>((outputs, cold_load, queued_for, run_started.elapsed()))
    };
    let timed = match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, work).await,
        None => Ok(work.await),
    };
    let (outputs, cold_load, queued_for, inference_took) = match timed {
        Ok(result) => result?,
        Err(_elapsed) => {
            return Err(refuse(
                Some(&labels),
                Failure::new(
                    Category::Timeout,
                    format!("the inference exceeded its deadline of {budget_ms}ms"),
                ),
            ));
        }
    };

    // 5. The outputs, against the manifest.
    if outputs.len() != entry.manifest.outputs.len() {
        return Err(refuse(
            Some(&labels),
            Failure::new(
                Category::Run,
                format!(
                    "the model produced {} outputs; the manifest declares {}",
                    outputs.len(),
                    entry.manifest.outputs.len()
                ),
            ),
        ));
    }
    let mut output_elements = 0usize;
    for (decl, tensor) in entry.manifest.outputs.iter().zip(&outputs) {
        // Against the same bindings the inputs filled, so `[…, "H"]` on
        // an output means the H the call actually brought.
        let fits = tensor.dtype().name() == decl.dtype
            && bindings.check(&decl.shape, tensor.shape()).is_ok();
        if !fits {
            return Err(refuse(
                Some(&labels),
                Failure::new(
                    Category::Run,
                    format!(
                        "output '{}': expected {}[{}], got {}[{}]",
                        decl.name,
                        decl.dtype,
                        dims(&decl.shape),
                        tensor.dtype(),
                        dims(tensor.shape())
                    ),
                ),
            ));
        }
        output_elements = output_elements.saturating_add(tensor.numel());
    }
    if output_elements > entry.limits.max_output_elements {
        return Err(refuse(
            Some(&labels),
            Failure::new(
                Category::OutputSize,
                format!(
                    "the outputs total more than {} elements (models.max_output_elements)",
                    entry.limits.max_output_elements
                ),
            ),
        ));
    }
    let mut members: Vec<(String, OwnedDataValue)> = entry
        .manifest
        .outputs
        .iter()
        .zip(outputs)
        .map(|(decl, tensor)| (decl.name.clone(), OwnedDataValue::Tensor(Arc::new(tensor))))
        .collect();
    let value = if raw {
        OwnedDataValue::Object(members)
    } else if let Some(select) = &request.select {
        // The task's own reduction, over the outputs and the root the
        // adapters read — which is how it reaches values from the
        // message, such as the indices to gather — so only what it
        // selects is written, and a whole output tensor never rides the
        // rest of the workflow. Evaluated natively on the engine that
        // compiled it.
        if members.iter().any(|(name, _)| name == SELECT_INPUT_KEY) {
            return Err(caller_input(
                Some(&labels),
                format!(
                    "'select' reads the call's input as '{SELECT_INPUT_KEY}', and this \
                     model has an output of that name; use the manifest's result or 'raw'"
                ),
            ));
        }
        members.push((SELECT_INPUT_KEY.to_string(), root));
        let (value, ops) = select.eval(&OwnedDataValue::Object(members), &labels)?;
        charged.add(ops);
        value
    } else {
        let object = OwnedDataValue::Object(members);
        let evaluated = exprs.eval(&entry.adapters.result, &object).map_err(|e| {
            evaluation_refused(
                Some(&labels),
                Category::Adapter,
                "the result expression failed",
                e,
            )
        })?;
        charged.add(evaluated.ops.unwrap_or(0));
        evaluated.value.into_owned()
    };

    let stats = request.stats.then(|| {
        json!({
            "id": entry.id,
            "version": entry.version,
            "digest": entry.digest,
            "runtime": labels.runtime,
            "device": device,
            "parameters": entry.stats.as_ref().map(|s| s.parameters),
            "artifact_bytes": entry.stats.as_ref().map(|s| s.artifact_bytes),
            "ops": charged.total,
            "peak_ops": charged.peak,
            "queued_ms": queued_for.as_secs_f64() * 1000.0,
            "inference_ms": inference_took.as_secs_f64() * 1000.0,
            "cold_load": cold_load,
        })
    });
    Ok(Inferred {
        labels,
        value,
        stats,
        writes,
    })
}

/// The session for `entry` on `runtime`/`device`, from the cache or loaded
/// into it: the bytes from the host's [`ArtifactSource`] — the disk cache
/// through the storage connector on a node, the file beside the manifest
/// offline — into the runtime on the blocking pool. The second value says
/// whether this call waited for a load. `source` is the metric's account of
/// why — `demand` from a task, `preload` from a publish.
///
/// Shared by the handler and the preload so the two cannot load differently.
pub async fn load_model(
    host: &InferenceHost,
    entry: Arc<ModelEntry>,
    runtime: Arc<dyn ModelRuntime>,
    device: &str,
    source: &'static str,
) -> Result<(Arc<dyn LoadedModel>, bool), LoadError> {
    let key = CacheKey {
        digest: entry.digest.clone(),
        runtime: runtime.name(),
        device: device.to_string(),
        binding: entry.binding.fingerprint().to_string(),
    };
    host.loaded
        .get_or_load(key, || async {
            let started = Instant::now();
            let result = fetch_and_load(host, &entry, runtime.clone(), device).await;
            let secs = started.elapsed().as_secs_f64();
            match &result {
                Ok(loaded) => {
                    crate::metrics::record_model_load(
                        &entry.id,
                        runtime.name(),
                        "ok",
                        source,
                        secs,
                    );
                    tracing::info!(
                        model = %entry.id,
                        version = entry.version,
                        digest = %entry.digest,
                        runtime = runtime.name(),
                        device,
                        resident_bytes = loaded.resident_bytes(),
                        load_ms = (secs * 1000.0) as u64,
                        source,
                        "Model loaded"
                    );
                }
                Err(e) => {
                    crate::metrics::record_model_load(
                        &entry.id,
                        runtime.name(),
                        "error",
                        source,
                        secs,
                    );
                    tracing::warn!(
                        model = %entry.id,
                        version = entry.version,
                        digest = %entry.digest,
                        runtime = runtime.name(),
                        device,
                        stage = e.stage,
                        reason = %e.message,
                        source,
                        "Model not loaded"
                    );
                }
            }
            result
        })
        .await
}

async fn fetch_and_load(
    host: &InferenceHost,
    entry: &Arc<ModelEntry>,
    runtime: Arc<dyn ModelRuntime>,
    device: &str,
) -> Result<Arc<dyn LoadedModel>, LoadError> {
    // One cold-load slot for the fetch and the parse together: both hold the
    // artifact in memory, so the permit is what keeps K distinct cold models
    // from holding K artifacts at once. Held until this function returns.
    // `acquire` only errors if the semaphore is closed, which it never is —
    // the host holds it for the life of the process.
    let _load_slot = match &host.load_permits {
        Some(permits) => Some(
            permits
                .acquire()
                .await
                .map_err(|e| LoadError::new("load", format!("the load slot is gone: {e}")))?,
        ),
        None => None,
    };
    let bytes = host.artifacts.bytes(entry).await?;
    let entry = entry.clone();
    let device = device.to_string();
    tokio::task::spawn_blocking(move || runtime.load(&bytes, &entry.binding, &device))
        .await
        .map_err(|e| LoadError::new("load", format!("the load did not complete: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::functions::run_test_task;
    use crate::model::ModelSet;
    use crate::model::fixture;
    use serde_json::json;

    fn models_runtime(config: &ModelsConfig) -> Arc<ModelsRuntime> {
        Arc::new(ModelsRuntime::new(config, "n".to_string()).expect("creates the dir"))
    }

    fn config() -> ModelsConfig {
        ModelsConfig {
            enabled: true,
            cache_dir: std::env::temp_dir()
                .join(format!("orion-model-handler-{}", uuid::Uuid::new_v4()))
                .to_string_lossy()
                .into_owned(),
            ..ModelsConfig::default()
        }
    }

    fn handler(models: Option<Arc<ModelsRuntime>>, config: ModelsConfig) -> ModelInferHandler {
        let config = Arc::new(config);
        ModelInferHandler {
            source: ModelSource::Node(crate::runtime::generation::test_handle()),
            host: models.map(|m| {
                Arc::new(InferenceHost::node(
                    &m,
                    config.clone(),
                    Arc::new(ConnectorRegistry::new(Default::default())),
                    reqwest::Client::new(),
                ))
            }),
            config,
        }
    }

    async fn refused(handler: ModelInferHandler, input: Value) -> String {
        run_test_task(NAME, Box::new(handler), input, json!({"board": [1]}))
            .await
            .expect_err("the task must fail")
    }

    /// Every refusal before a model runs names the handler and what the
    /// caller can act on: the node without the runtime, an id the
    /// generation does not serve, the id's own load issue, the fields the
    /// task must carry.
    #[tokio::test]
    async fn refusals_name_the_handler_and_the_reason() {
        let config = config();
        let good = json!({"model": "ada.c4-tiny", "input": {"var": ""}});

        let err = refused(handler(None, config.clone()), good.clone()).await;
        assert!(
            err.contains("model_infer: models are disabled on this node"),
            "{err}"
        );

        let models = models_runtime(&config);
        let err = refused(handler(Some(models.clone()), config.clone()), good.clone()).await;
        assert!(
            err.contains("model 'ada.c4-tiny' is unavailable on this node")
                && err.contains("unknown or not active"),
            "{err}"
        );

        let err = refused(
            handler(Some(models.clone()), config.clone()),
            json!({"input": {"var": ""}}),
        )
        .await;
        assert!(err.contains("requires 'model'"), "{err}");
        let err = refused(
            handler(Some(models.clone()), config.clone()),
            json!({"model": 7, "input": {"var": ""}}),
        )
        .await;
        assert!(err.contains("'model' must evaluate to a model id"), "{err}");

        // A generation carrying the model's load issue hands the reason on.
        let h = handler(Some(models.clone()), config.clone());
        let ModelSource::Node(runtime) = &h.source else {
            unreachable!("built on a node source")
        };
        let runtime = runtime.clone();
        let generation = runtime.load();
        let now = chrono::Utc::now().naive_utc();
        let row = crate::storage::models::Model {
            model_id: "ada.c4-tiny".to_string(),
            version: 1,
            status: "active".to_string(),
            digest: "sha256:abc".to_string(),
            manifest_json: fixture::MANIFEST.to_string(),
            artifact_json: json!({"connector": "bucket", "key": "k", "digest": "sha256:abc"})
                .to_string(),
            admission_json: json!({"state": "failed", "stage": "digest", "reason": "mismatch"})
                .to_string(),
            stats_json: None,
            tags_json: "[]".to_string(),
            signature: None,
            created_at: now,
            updated_at: now,
        };
        let set = ModelSet::load_active(&[row], &config, true, &crate::engine::kit::test_exprs());
        runtime.publish(
            generation.engine.clone(),
            generation.channels.clone(),
            generation.functions.clone(),
            generation.plugins.clone(),
            Arc::new(set),
        );
        let err = refused(h, good).await;
        assert!(
            err.contains(
                "unavailable on this node: admission: admission is 'failed' at stage \
                          'digest': mismatch"
            ),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&config.cache_dir);
    }

    /// Cold loads across distinct callers are bounded by
    /// `models.max_concurrent_loads`: with one slot the fetch-and-parse that
    /// holds an artifact in memory never overlaps; unbounded, it does.
    #[tokio::test]
    async fn cold_loads_are_bounded_by_the_load_semaphore() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Records how many loads hold the fetch-and-parse slot at once.
        struct Counting {
            live: AtomicUsize,
            peak: AtomicUsize,
        }
        #[async_trait]
        impl ArtifactSource for Counting {
            async fn bytes(&self, _entry: &ModelEntry) -> Result<Vec<u8>, LoadError> {
                let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(live, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                self.live.fetch_sub(1, Ordering::SeqCst);
                Ok(vec![0u8; 4])
            }
        }

        struct Loaded;
        impl LoadedModel for Loaded {
            fn digest(&self) -> &str {
                "sha256:fake"
            }
            fn resident_bytes(&self) -> usize {
                1
            }
            fn run(
                &self,
                _inputs: Vec<OwnedDataTensor>,
            ) -> Result<Vec<OwnedDataTensor>, super::super::runtimes::RunError> {
                Ok(Vec::new())
            }
        }
        struct Stub;
        impl ModelRuntime for Stub {
            fn name(&self) -> &'static str {
                "tract"
            }
            fn devices(&self) -> &'static [&'static str] {
                &["cpu"]
            }
            fn formats(&self) -> &'static [&'static str] {
                &["onnx"]
            }
            fn load(
                &self,
                _bytes: &[u8],
                _binding: &super::super::runtimes::LoadBinding,
                _device: &str,
            ) -> Result<Arc<dyn LoadedModel>, LoadError> {
                Ok(Arc::new(Loaded))
            }
        }

        // One compiled entry, reused: fetch_and_load does not dedup (the cache
        // single-flight does), so N concurrent calls each take a load slot —
        // which is what we are measuring.
        let cfg = config();
        let set = ModelSet::from_manifests(
            [super::super::loader::ManifestEntry {
                manifest: fixture::manifest(),
                artifact_path: std::path::PathBuf::from("x.onnx"),
                digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                    .to_string(),
                stats: None,
            }],
            &cfg,
            &crate::engine::kit::test_exprs(),
        );
        let id = set.ids().next().expect("one entry compiled").to_string();
        let entry = set.get(&id).expect("the entry").clone();

        let peak_with = |slots: Option<usize>| {
            let entry = entry.clone();
            let cfg = cfg.clone();
            async move {
                let counting = Arc::new(Counting {
                    live: AtomicUsize::new(0),
                    peak: AtomicUsize::new(0),
                });
                let host = Arc::new(InferenceHost {
                    runtimes: Arc::new(ModelRuntimes::builtin(&cfg)),
                    loaded: Arc::new(LoadedCache::new(1 << 30)),
                    inference_permits: Arc::new(Semaphore::new(1)),
                    inference_slots: 1,
                    load_permits: slots.map(|n| Arc::new(Semaphore::new(n))),
                    artifacts: counting.clone(),
                });
                let runtime: Arc<dyn ModelRuntime> = Arc::new(Stub);
                let mut tasks = Vec::new();
                for _ in 0..4 {
                    let host = host.clone();
                    let entry = entry.clone();
                    let runtime = runtime.clone();
                    tasks.push(tokio::spawn(async move {
                        fetch_and_load(&host, &entry, runtime, "cpu").await
                    }));
                }
                for task in tasks {
                    task.await.expect("join").expect("the load succeeds");
                }
                counting.peak.load(Ordering::SeqCst)
            }
        };

        assert_eq!(
            peak_with(Some(1)).await,
            1,
            "one slot serialises cold loads"
        );
        assert!(
            peak_with(None).await >= 2,
            "unbounded, distinct cold loads overlap"
        );
    }

    /// The pieces of a message: a shape as `dtype[dims]`, a value's kind.
    #[test]
    fn messages_spell_shapes_and_kinds() {
        assert_eq!(dims(&[1, 2, 6, 7]), "1,2,6,7");
        assert_eq!(dims::<usize>(&[]), "");
        // A declared shape prints the same way, a named axis by its name,
        // so a refusal can put the declaration beside what arrived.
        assert_eq!(dims(&crate::model::manifest::fixed_shape(&[1, 7])), "1,7");
        assert_eq!(
            dims(&[
                crate::model::manifest::Dim::Fixed(1),
                crate::model::manifest::Dim::Named("H".to_string()),
            ]),
            "1,H"
        );
        assert_eq!(kind_of(&OwnedDataValue::Null), "null");
        assert_eq!(kind_of(&OwnedDataValue::from(&json!([1]))), "a list");
        assert_eq!(
            kind_of(&OwnedDataValue::from(&json!({"a": 1}))),
            "an object"
        );
        assert_eq!(kind_of(&OwnedDataValue::from(&json!("s"))), "a string");
    }

    // -- dataflow-rs 4: `ModelInferFunction`, twinned against the handler --

    use crate::engine::ResolvedSecrets;
    use crate::engine::functions::twin::{self, Seen};
    use crate::model::loader::ManifestEntry;
    use dataflow_rs::function::testing;
    use dataflow_rs::program::{IssueCode, Severity};

    /// The fixture's graph, after a pause: a cold load as slow as a test
    /// needs.
    struct Graph(Duration);

    #[async_trait]
    impl ArtifactSource for Graph {
        async fn bytes(&self, _entry: &ModelEntry) -> Result<Vec<u8>, LoadError> {
            tokio::time::sleep(self.0).await;
            Ok(fixture::ONNX.to_vec())
        }
    }

    /// What one call runs over: the `c4-tiny` fixture's set, compiled on
    /// `exprs`, and a host of the call's own, whose cold load takes `delay`
    /// and which holds `slots` inference slots.
    struct Rig {
        config: Arc<ModelsConfig>,
        exprs: ExprEngine,
        delay: Duration,
        slots: usize,
    }

    impl Rig {
        fn new() -> Self {
            Self {
                config: Arc::new(ModelsConfig {
                    enabled: true,
                    ..ModelsConfig::default()
                }),
                exprs: kit::test_exprs(),
                delay: Duration::ZERO,
                slots: 1,
            }
        }

        fn source(&self) -> ModelSource {
            ModelSource::Offline(Arc::new(OfflineModels::new(
                vec![ManifestEntry {
                    manifest: fixture::manifest(),
                    artifact_path: std::path::PathBuf::from("c4-tiny.onnx"),
                    digest: crate::crypto::sha256_digest(fixture::ONNX),
                    stats: None,
                }],
                self.config.clone(),
                self.exprs.clone(),
            )))
        }

        fn host(&self) -> Arc<InferenceHost> {
            Arc::new(InferenceHost {
                runtimes: Arc::new(ModelRuntimes::builtin(&self.config)),
                loaded: Arc::new(LoadedCache::new(self.config.max_loaded_bytes)),
                inference_permits: Arc::new(Semaphore::new(self.slots)),
                inference_slots: self.slots,
                load_permits: None,
                artifacts: Arc::new(Graph(self.delay)),
            })
        }

        fn handler(&self) -> ModelInferHandler {
            ModelInferHandler {
                source: self.source(),
                host: Some(self.host()),
                config: self.config.clone(),
            }
        }

        fn function(&self) -> ModelInferFunction {
            ModelInferFunction {
                source: self.source(),
                host: Some(self.host()),
                config: self.config.clone(),
            }
        }
    }

    /// A `[1, 2, 6, 7]` board, a few stones placed, as the fixture's
    /// adapter reads it at `data.board`; the id a computed `model` reads;
    /// the columns a `select` gathers.
    fn data() -> Value {
        let mut planes = vec![vec![vec![0.0f32; 7]; 6]; 2];
        planes[0][5][3] = 1.0;
        planes[1][5][2] = 1.0;
        planes[0][4][3] = 1.0;
        json!({"board": [planes], "which": "ada.c4-tiny", "cols": [3, 5]})
    }

    /// A dataflow-rs 4 message whose `data` is `data`.
    fn message(data: &Value) -> dataflow_rs::Message {
        dataflow_rs::Message::from_parts(
            OwnedDataValue::Null,
            OwnedDataValue::from(data),
            OwnedDataValue::Null,
        )
    }

    /// `input` with `extra`'s members set on it.
    fn with(mut input: Value, extra: Value) -> Value {
        if let (Some(into), Some(from)) = (input.as_object_mut(), extra.as_object()) {
            into.extend(from.clone());
        }
        input
    }

    /// The fixture, over the whole context, its result at `data.result`.
    fn infer(extra: Value) -> Value {
        with(
            json!({"model": "ada.c4-tiny", "input": {"var": ""}, "output": "data.result"}),
            extra,
        )
    }

    /// `seen` with the stats' two timings, which no two calls share, read
    /// as numbers and set aside.
    fn steady(seen: Seen) -> Seen {
        seen.map(|mut data| {
            if let Some(stats) = data.get_mut("stats").and_then(Value::as_object_mut) {
                for timing in ["queued_ms", "inference_ms"] {
                    assert!(stats.get(timing).is_some_and(Value::is_number), "{timing}");
                    stats.insert(timing.to_string(), Value::Null);
                }
            }
            data
        })
    }

    /// One task through each engine, each over a rig of its own: a fresh
    /// host, so each call's load is cold.
    async fn both(rig: &Rig, input: Value) -> (Seen, Seen) {
        let v3 = twin::v3(NAME, Box::new(rig.handler()), input.clone(), data()).await;
        let v4 = twin::v4(NAME, rig.function(), input, data()).await;
        (steady(v3), steady(v4))
    }

    /// One dataflow-rs 4 call of `rig`'s function over [`data`], on Orion's
    /// expressions, under the run deadline `deadline` from now.
    async fn call_v4(rig: &Rig, input: Value, deadline: Option<Duration>) -> testing::Called {
        let call = testing::call(rig.function(), input)
            .exprs(kit::test_exprs())
            .name(NAME)
            .message(message(&data()));
        match deadline {
            Some(after) => {
                call.deadline(dataflow_rs::run::Instant::now() + after)
                    .await
            }
            None => call.await,
        }
    }

    /// The fixture runs for real on both engines and writes the same thing:
    /// the manifest's result, the outputs `raw`, a `select`'s reduction with
    /// operands from the message, and the stats beside the result.
    #[tokio::test]
    async fn an_inference_writes_what_1_12_wrote() {
        let rig = Rig::new();
        for input in [
            infer(json!({"stats_output": "data.stats"})),
            // A computed model, a runtime named and a computed deadline.
            infer(json!({
                "model": {"var": "data.which"},
                "runtime": "tract",
                "timeout_ms": {"+": [4000, 1000]}
            })),
            infer(json!({"raw": true})),
            infer(json!({"select": {"to_list": [{"gather": [
                {"reshape": [{"var": "policy"}, [7]]},
                {"var": "input.data.cols"},
                0
            ]}]}})),
            infer(json!({
                "select": {"argmax": [{"var": "policy"}, 1]},
                "stats_output": "data.stats"
            })),
        ] {
            let (v3, v4) = both(&rig, input.clone()).await;
            assert_eq!(v4, v3, "{input}");
            let data = v4.expect("the inference ran");
            assert!(
                data.get("result").is_some_and(|r| !r.is_null()),
                "{input}: {data}"
            );
        }
    }

    /// `stats_output` is written beside `output`, the primary, after it, as
    /// 1.12 wrote it: the call's account, `ops` and `peak_ops` included.
    #[tokio::test]
    async fn stats_output_is_written_beside_output() {
        let called = call_v4(
            &Rig::new(),
            json!({"model": "ada.c4-tiny", "input": {"var": ""}, "stats_output": "data.stats"}),
            None,
        )
        .await;
        assert_eq!(called.outcome.as_ref().ok(), Some(&Outcome::Ok));
        let paths: Vec<String> = called.writes.iter().map(|(p, _)| p.to_string()).collect();
        assert_eq!(paths, ["temp_data.inference", "data.stats"]);
        let result = Value::from(called.message.context().temp_data());
        assert_eq!(
            result["inference"]["policy"][0].as_array().map(Vec::len),
            Some(7),
            "{result}"
        );
        let stats = &Value::from(called.message.context().data())["stats"];
        let keys: Vec<&str> = stats
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "id",
                "version",
                "digest",
                "runtime",
                "device",
                "parameters",
                "artifact_bytes",
                "ops",
                "peak_ops",
                "queued_ms",
                "inference_ms",
                "cold_load"
            ]
        );
        assert_eq!(stats["id"], "ada.c4-tiny");
        assert_eq!(stats["runtime"], "tract");
        assert_eq!(stats["cold_load"], true);
        let (ops, peak) = (stats["ops"].as_u64(), stats["peak_ops"].as_u64());
        assert!(
            matches!((ops, peak), (Some(ops), Some(peak)) if peak > 0 && ops >= peak),
            "{stats}"
        );
    }

    /// `refusals_name_the_handler_and_the_reason` on both engines: every
    /// refusal before and after a model runs, with the same code, text and
    /// retryability, the generation's load issue included. A missing
    /// `model` is refused at load on dataflow-rs 4 (I183), where 1.12
    /// refused each call.
    #[tokio::test]
    async fn refusals_are_1_12s_on_v4() {
        let rig = Rig::new();
        let disabled = (
            ModelInferHandler {
                source: rig.source(),
                host: None,
                config: rig.config.clone(),
            },
            ModelInferFunction {
                source: rig.source(),
                host: None,
                config: rig.config.clone(),
            },
        );
        let v3 = twin::v3(NAME, Box::new(disabled.0), infer(json!({})), data()).await;
        let v4 = twin::v4(NAME, disabled.1, infer(json!({})), data()).await;
        assert_eq!(v4, v3);
        assert!(
            v4.expect_err("disabled")
                .message
                .contains("model_infer: models are disabled on this node")
        );

        for input in [
            infer(json!({"model": 7})),
            infer(json!({"model": "ada.nope"})),
            infer(json!({"model": {"var": "data.nothing"}})),
            infer(json!({"input": {"var": "data.nothing"}})),
            infer(json!({"input": {"data": {"board": [[1, 2]]}}})),
            infer(json!({"timeout_ms": {"-": [5, 5]}})),
            infer(json!({"select": {"reshape": [{"var": "policy"}, [5]]}})),
        ] {
            let (v3, v4) = both(&rig, input.clone()).await;
            assert_eq!(v4, v3, "{input}");
            assert!(v4.is_err(), "{input}");
        }

        // A generation carrying the model's load issue hands the reason on.
        let config = config();
        let runtime = crate::runtime::generation::test_handle();
        let generation = runtime.load();
        let now = chrono::Utc::now().naive_utc();
        let row = crate::storage::models::Model {
            model_id: "ada.c4-tiny".to_string(),
            version: 1,
            status: "active".to_string(),
            digest: "sha256:abc".to_string(),
            manifest_json: fixture::MANIFEST.to_string(),
            artifact_json: json!({"connector": "bucket", "key": "k", "digest": "sha256:abc"})
                .to_string(),
            admission_json: json!({"state": "failed", "stage": "digest", "reason": "mismatch"})
                .to_string(),
            stats_json: None,
            tags_json: "[]".to_string(),
            signature: None,
            created_at: now,
            updated_at: now,
        };
        runtime.publish(
            generation.engine.clone(),
            generation.channels.clone(),
            generation.functions.clone(),
            generation.plugins.clone(),
            Arc::new(ModelSet::load_active(
                &[row],
                &config,
                true,
                &kit::test_exprs(),
            )),
        );
        let models = models_runtime(&config);
        let node = |runtime: &Arc<RuntimeHandle>| {
            let config = Arc::new(config.clone());
            let host = Some(Arc::new(InferenceHost::node(
                &models,
                config.clone(),
                Arc::new(ConnectorRegistry::new(Default::default())),
                reqwest::Client::new(),
            )));
            (
                ModelInferHandler {
                    source: ModelSource::Node(runtime.clone()),
                    host: host.clone(),
                    config: config.clone(),
                },
                ModelInferFunction {
                    source: ModelSource::Node(runtime.clone()),
                    host,
                    config,
                },
            )
        };
        let (handler, function) = node(&runtime);
        let v3 = twin::v3(NAME, Box::new(handler), infer(json!({})), data()).await;
        let v4 = twin::v4(NAME, function, infer(json!({})), data()).await;
        assert_eq!(v4, v3);
        assert!(v4.expect_err("not served").message.contains(
            "unavailable on this node: admission: admission is 'failed' at stage 'digest': \
                 mismatch"
        ));
        let _ = std::fs::remove_dir_all(&config.cache_dir);

        // A task with no `model` never loads on dataflow-rs 4.
        let called = call_v4(&rig, json!({"input": {"var": ""}}), None).await;
        assert_eq!(
            *called.outcome.expect_err("refused at load").kind(),
            ErrorKind::Compile
        );
        assert!(
            called
                .issues
                .iter()
                .any(|i| i.path.to_string() == "model" && i.severity == Severity::Rejected),
            "{:?}",
            called.issues
        );
    }

    /// An `engine.ops_budget` crossed in an adapter is `ErrorKind::Budget`,
    /// where 1.12 matched the error's `BudgetExceeded` tag: the same code,
    /// text and retryability on both engines, under the adapter's name.
    #[tokio::test]
    async fn a_budget_crossed_in_an_adapter_is_errorkind_budget() {
        let mut rig = Rig::new();
        rig.exprs =
            crate::engine::operators::orion_exprs(&ResolvedSecrets::empty(), 20).expect("builds");
        let (v3, v4) = both(&rig, infer(json!({}))).await;
        assert_eq!(v4, v3);
        let failure = v4.expect_err("over the ceiling");
        assert_eq!(failure.code, "BUDGET_EXCEEDED");
        assert!(
            failure
                .message
                .starts_with("model_infer: adapter for input 'board' failed: ")
                && failure.message.contains("budget of 20"),
            "{}",
            failure.message
        );
        assert!(!failure.retryable);
        let called = call_v4(&rig, infer(json!({})), None).await;
        assert_eq!(
            *called.outcome.expect_err("over the ceiling").kind(),
            ErrorKind::Budget
        );
        assert!(called.writes.is_empty());
    }

    /// dataflow-rs 3's engine with an operation ceiling, running `handler`
    /// as `twin::v3` runs one.
    async fn v3_budgeted(handler: ModelInferHandler, input: Value, budget: u64) -> Seen {
        let mut fns: std::collections::HashMap<String, dataflow_rs_v3::BoxedFunctionHandler> =
            Default::default();
        fns.insert(NAME.to_string(), Box::new(handler));
        let workflow: dataflow_rs_v3::Workflow = serde_json::from_value(json!({
            "id": "w", "name": "w", "condition": true,
            "tasks": [{"id": "t", "name": "t", "function": {"name": NAME, "input": input}}]
        }))
        .expect("a workflow");
        let engine =
            crate::engine::operators::with_ops_budget(dataflow_rs_v3::Engine::builder(), budget)
                .with_handlers(fns)
                .with_workflow(workflow)
                .build()
                .expect("the engine builds");
        let mut message = dataflow_rs_v3::Message::from_value(&json!({}));
        dataflow_rs_v3::engine::utils::set_nested_value(
            &mut message.context,
            "data",
            OwnedDataValue::from(&data()),
        );
        match engine.process_message(&mut message).await {
            Ok(()) => Ok(message.data().into()),
            Err(e) => {
                let record = message.errors().first().expect("the failure is recorded");
                Err(twin::Failure {
                    code: record.code.clone(),
                    message: e.to_string(),
                    retryable: e.retryable(),
                    detail: record.detail.clone(),
                })
            }
        }
    }

    /// A ceiling crossed in a field of the task's own keeps its kind under
    /// the field's name, on both engines.
    #[tokio::test]
    async fn a_budget_crossed_in_a_field_keeps_its_kind() {
        let rig = Rig::new();
        let input = infer(json!({
            "input": {"+": [{"+": [{"+": [{"+": [1, {"var": "data.cols.0"}]}, 1]}, 1]}, 1]}
        }));
        let v3 = v3_budgeted(rig.handler(), input.clone(), 3).await;
        let called = testing::call(rig.function(), input)
            .exprs(
                crate::engine::operators::orion_exprs(&ResolvedSecrets::empty(), 3)
                    .expect("builds"),
            )
            .name(NAME)
            .message(message(&data()))
            .await;
        let v4 = twin::seen(&called);
        assert_eq!(v4, v3);
        let failure = v4.expect_err("over the ceiling");
        assert_eq!(failure.code, "BUDGET_EXCEEDED");
        assert!(
            failure.message.starts_with("model_infer: 'input': "),
            "{}",
            failure.message
        );
        assert_eq!(
            *called.outcome.expect_err("over the ceiling").kind(),
            ErrorKind::Budget
        );
    }

    /// The model's own deadline first — `timeout_ms`, or the node's ceiling
    /// when it is unset — is 1.12's, whatever the run's deadline after it:
    /// a cold load cut short, `TIMEOUT_ERROR`, retryable, in 1.12's words,
    /// and nothing written.
    #[tokio::test]
    async fn the_models_own_deadline_first_fails_as_1_12_did() {
        let mut slow = Rig::new();
        slow.delay = Duration::from_secs(5);
        let mut capped = Rig::new();
        capped.delay = Duration::from_secs(5);
        capped.config = Arc::new(ModelsConfig {
            enabled: true,
            max_timeout_ms: 100,
            ..ModelsConfig::default()
        });
        for (rig, input) in [
            (&slow, infer(json!({"timeout_ms": 100}))),
            (&capped, infer(json!({}))),
        ] {
            let v3 = twin::v3(NAME, Box::new(rig.handler()), input.clone(), data()).await;
            let called = call_v4(rig, input.clone(), Some(Duration::from_secs(60))).await;
            let v4 = twin::seen(&called);
            assert_eq!(v4, v3, "{input}");
            let failure = v4.expect_err("timed out");
            assert_eq!(failure.code, "TIMEOUT_ERROR");
            assert_eq!(
                failure.message,
                "Timeout error: model_infer: the inference exceeded its deadline of 100ms"
            );
            assert!(failure.retryable);
            assert!(called.writes.is_empty());
        }
    }

    /// The run's deadline first: the call arms no timer of the model's,
    /// and the engine ends it `TimedOut` at the run's deadline, writing
    /// nothing, as 1.12's channel timeout ended the run.
    #[tokio::test]
    async fn the_runs_deadline_first_ends_the_call_timed_out() {
        let mut rig = Rig::new();
        rig.delay = Duration::from_secs(5);
        let started = std::time::Instant::now();
        let called = call_v4(
            &rig,
            infer(json!({"timeout_ms": 10_000})),
            Some(Duration::from_millis(150)),
        )
        .await;
        let error = called.outcome.expect_err("the run's deadline passed");
        assert_eq!(*error.kind(), ErrorKind::TimedOut);
        assert_eq!(
            error.message(),
            "the run's deadline passed during `model_infer`"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert!(called.writes.is_empty());
    }

    /// A call waiting for an inference slot waits under the same rule: the
    /// model's deadline first is 1.12's `permit` refusal; the run's first
    /// ends the call `TimedOut`.
    #[tokio::test]
    async fn a_full_slot_waits_under_the_earlier_deadline() {
        let mut rig = Rig::new();
        rig.slots = 0;
        let input = infer(json!({"timeout_ms": 150}));
        let v3 = twin::v3(NAME, Box::new(rig.handler()), input.clone(), data()).await;
        let called = call_v4(&rig, input, Some(Duration::from_secs(60))).await;
        assert_eq!(twin::seen(&called), v3);
        let failure = v3.expect_err("no slot");
        assert_eq!(failure.code, "VALIDATION_ERROR");
        assert_eq!(
            failure.message,
            "Validation error: model_infer: no inference slot freed up before the deadline \
             (models.max_concurrent_inferences)"
        );

        let called = call_v4(
            &rig,
            infer(json!({"timeout_ms": 10_000})),
            Some(Duration::from_millis(150)),
        )
        .await;
        assert_eq!(
            *called.outcome.expect_err("the run's deadline").kind(),
            ErrorKind::TimedOut
        );
        assert!(called.writes.is_empty());
    }

    /// I185: a literal context root at `output`, the default excepted, is an
    /// Advisory, since the result may not be an object: the manifest's
    /// object result is written there, and a `select`'s number fails the
    /// call (break 21). `raw` writes an object, so it draws none.
    #[tokio::test]
    async fn a_literal_root_output_is_an_advisory_unless_raw() {
        let rig = Rig::new();
        let argmax = json!({"argmax": [{"var": "policy"}, 1]});
        for (input, advisory, written) in [
            (infer(json!({"output": "data"})), true, true),
            (
                infer(json!({"output": "temp_data", "select": argmax})),
                true,
                false,
            ),
            (infer(json!({"output": "data", "raw": true})), false, true),
            (
                json!({"model": "ada.c4-tiny", "input": {"var": ""}}),
                false,
                true,
            ),
            (infer(json!({"select": argmax})), false, true),
        ] {
            let called = call_v4(&rig, input.clone(), None).await;
            let advisories: Vec<(String, IssueCode)> = called
                .issues
                .iter()
                .filter(|i| i.severity == Severity::Advisory)
                .map(|i| (i.path.to_string(), i.code))
                .collect();
            if advisory {
                assert_eq!(
                    advisories,
                    [("output".to_string(), IssueCode::InputParse)],
                    "{input}"
                );
            } else {
                assert!(advisories.is_empty(), "{input}: {advisories:?}");
            }
            assert_eq!(
                called.outcome.is_ok(),
                written,
                "{input}: {:?}",
                called.outcome
            );
        }
    }
}
