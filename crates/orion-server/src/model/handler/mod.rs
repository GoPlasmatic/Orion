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
use dataflow_rs::ErrorKind;
use dataflow_rs::datavalue::{OwnedDataTensor, OwnedDataValue};
use dataflow_rs::expr::ExprEngine;
use dataflow_rs::function::{FunctionSpec, Prepare};
use dataflow_rs::{Ctx, Expr, Function, Issue, Outcome};
use dataflow_rs_v3::datalogic_rs as datalogic;
use dataflow_rs_v3::engine::error::DataflowError;
use dataflow_rs_v3::engine::functions::AsyncFunctionHandler;
use dataflow_rs_v3::engine::task_context::TaskContext;
use dataflow_rs_v3::engine::task_outcome::TaskOutcome;
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
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

mod infer;
pub use infer::*;

#[cfg(test)]
mod tests;
