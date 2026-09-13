//! `model_infer` — one inference of an admitted model, from the message to
//! the result the task writes.
//!
//! The handler holds the node's [`RuntimeHandle`] and loads the generation
//! **once per call**, as `channel_call` does: the entry it resolves — the
//! adapters, the result expression, the limits — and the expression engine
//! it evaluates them on are the same generation's, so a reload mid-workflow
//! can never pair one generation's adapters with another's engine. The
//! loaded session comes from the process-wide
//! [`LoadedCache`] and outlives generations.
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
//!    blocking pool (`run`; the deadline elapsing anywhere is `timeout`);
//! 5. the outputs, checked against the manifest (`run` on a mismatch,
//!    `output_size` over `max_output_elements`), then either written raw as
//!    `{name: tensor}` or passed through the result expression (`adapter`).
//!
//! Nothing is written on failure. Every category is one of
//! [`Category`]'s, which is what the failure metric counts by, and the
//! `model` and `runtime` labels are the entry's id and the runtime's own
//! name — never a string the message chose.
//!
//! **Offline**, `dry-run` and `orion-server test` run the same handler over
//! a [`ModelSource::Offline`]: the set is built from the manifests a
//! `--model-dir` holds ([`super::offline::OfflineModels`]), compiled lazily
//! on the calling engine — the only engine a dry run has — and a cold load
//! reads the file beside the manifest rather than fetching through a storage
//! connector. That is the one seam between the two: [`ArtifactSource`] is
//! how a load gets its bytes, and [`InferenceHost`] carries it beside the
//! runtimes, the resident sessions and the inference slots. Nothing about
//! the adapters, the limits or the refusals differs, so what a dry run
//! reports is what a node would do with the same bytes — admission excepted:
//! no digest is claimed and no probe runs offline, because the bytes on
//! disk are the author's own and are what the run exists to try. Without a
//! `--model-dir` the function is stubbed like a connector function, keyed by
//! its name.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dataflow_rs::datalogic_rs as datalogic;
use dataflow_rs::datavalue::{OwnedDataTensor, OwnedDataValue};
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
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
use crate::engine::functions::templated_input::TemplatedInput;
use crate::runtime::RuntimeHandle;

/// This handler's name in metrics, profiles and error messages.
pub const NAME: &str = "model_infer";

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
            artifacts,
        }
    }
}

/// Where a call finds the model set and the expression engine its adapters
/// were compiled on — the two must be one pair.
pub enum ModelSource {
    /// A node: the serving generation, loaded once per call, so a reload
    /// mid-workflow can never pair one generation's adapters with another's
    /// engine.
    Node(Arc<RuntimeHandle>),
    /// An offline run: a fixed set of manifests, compiled on the calling
    /// engine the first time a task asks and reused for the rest of the run.
    Offline(Arc<OfflineModels>),
}

/// The `model_infer` task function.
pub struct ModelInferHandler {
    pub source: ModelSource,
    /// What the call runs on — `None` with `models.enabled = false`, which
    /// makes every call an `unavailable` failure rather than a build error.
    pub host: Option<Arc<InferenceHost>>,
    pub config: Arc<ModelsConfig>,
}

#[async_trait]
impl AsyncFunctionHandler for ModelInferHandler {
    type Input = TemplatedInput;

    /// Compile the expression fields the table declares (`model`, `input`).
    fn compile_input(
        input: &mut Self::Input,
        c: &dataflow_rs::engine::functions::TemplateCompiler,
    ) -> dataflow_rs::Result<()> {
        input.compile(NAME, c)
    }

    /// The shell: run the body, count the outcome, name the handler once.
    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &TemplatedInput,
    ) -> dataflow_rs::Result<TaskOutcome> {
        let started = Instant::now();
        match self.run(ctx, input).await {
            Ok(labels) => {
                crate::metrics::record_model_inference(
                    &labels.model,
                    labels.runtime,
                    "ok",
                    started.elapsed().as_secs_f64(),
                );
                Ok(TaskOutcome::Success)
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
                Err(refused.error.prefixed(NAME).into())
            }
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

fn refuse(labels: Option<&Labels>, failure: Failure) -> Refused {
    Refused {
        labels: labels.cloned(),
        category: failure.category,
        error: failure.into_handler_error(),
    }
}

/// An evaluation error under `category`, with the engine's own text kept
/// so the caller sees what the expression did — and an `engine.ops_budget`
/// refusal carried through as the `BudgetExceeded` variant, so its
/// `BUDGET_EXCEEDED` code survives the trip through the handler shell.
fn evaluation_refused(
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

/// A template field of the task's own that did not resolve. The
/// `DataflowError` is kept as the original, so a budget refusal in
/// `model` or `input` keeps its code.
fn field_refused(field: &str, e: DataflowError) -> Refused {
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

/// `[1,2,6,7]` as `1x2x6x7`-style text for a message.
fn dims(shape: &[usize]) -> String {
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

/// Evaluate a compiled expression over `root` on `engine` — the generation's
/// own, which compiled it — and take the result out of the arena.
fn evaluate(
    engine: &datalogic::Engine,
    logic: &datalogic::Logic,
    root: &OwnedDataValue,
) -> Result<OwnedDataValue, datalogic::Error> {
    let arena = datalogic::bumpalo::Bump::new();
    engine.evaluate(logic, root, &arena).map(|v| v.to_owned())
}

/// A permit from `permits` before `deadline`, or a `permit` refusal naming
/// the ceiling that was full.
async fn acquire<'s>(
    permits: &'s Semaphore,
    deadline: Instant,
    ceiling: &str,
    labels: &Labels,
) -> Result<SemaphorePermit<'s>, Refused> {
    match tokio::time::timeout_at(deadline, permits.acquire()).await {
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

impl ModelInferHandler {
    async fn run(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &TemplatedInput,
    ) -> Result<Labels, Refused> {
        let started = Instant::now();
        // One set and one engine for this call: the entry, its adapters and
        // the engine they were compiled on, off one load of the generation
        // on a node, or the offline set on the engine that is running us.
        let (set, datalogic): (Arc<ModelSet>, Arc<datalogic::Engine>) = match &self.source {
            ModelSource::Node(runtime) => {
                let generation = runtime.load();
                (
                    generation.models.clone(),
                    generation.engine.datalogic().clone(),
                )
            }
            ModelSource::Offline(offline) => {
                (offline.set_on(ctx.datalogic()), ctx.datalogic().clone())
            }
        };
        let datalogic: &datalogic::Engine = &datalogic;
        let Some(host) = &self.host else {
            return Err(refuse(
                None,
                Failure::new(
                    Category::Unavailable,
                    "models are disabled on this node (models.enabled = false)",
                ),
            ));
        };

        // 1. The model.
        let model_id = match input.resolve_owned("model", ctx) {
            None => return Err(caller_input(None, "requires 'model' (string)".to_string())),
            Some(Err(e)) => return Err(field_refused("model", e)),
            Some(Ok(value)) => match value.as_str() {
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
            },
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
        let runtime_name = match input.get("runtime") {
            None | Some(Value::Null) => match self.config.default_runtime_for(format) {
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
            Some(Value::String(name)) => name.as_str(),
            Some(_) => return Err(caller_input(None, "'runtime' must be a string".to_string())),
        };
        let (runtime, device) = host
            .runtimes
            .for_format(&self.config, runtime_name, format)
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

        // The rest of the task's own fields, read as written.
        let timeout_ms = match input.get("timeout_ms") {
            None | Some(Value::Null) => None,
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
        let deadline = started + budget;
        let raw = match input.get("raw") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                return Err(caller_input(
                    Some(&labels),
                    "'raw' must be a boolean".to_string(),
                ));
            }
        };
        let output = match input.get("output") {
            None | Some(Value::Null) => DEFAULT_OUTPUT,
            Some(Value::String(path)) if !path.is_empty() => path.as_str(),
            Some(_) => {
                return Err(caller_input(
                    Some(&labels),
                    "'output' must be a dotted path (string)".to_string(),
                ));
            }
        };
        let stats_output = match input.get("stats_output") {
            None | Some(Value::Null) => None,
            Some(Value::String(path)) if !path.is_empty() => Some(path.as_str()),
            Some(_) => {
                return Err(caller_input(
                    Some(&labels),
                    "'stats_output' must be a dotted path (string)".to_string(),
                ));
            }
        };

        // 3. The adapters, on the generation's engine, before anything is
        // loaded: a message that does not marshal costs no cold load.
        let root = match input.resolve_owned("input", ctx) {
            None => {
                return Err(caller_input(
                    Some(&labels),
                    "requires 'input': the JSON root the manifest's adapters read".to_string(),
                ));
            }
            Some(Err(e)) => {
                let mut refused = field_refused("input", e);
                refused.labels = Some(labels.clone());
                return Err(refused);
            }
            Some(Ok(root)) => root,
        };
        let mut tensors: Vec<OwnedDataTensor> = Vec::with_capacity(entry.adapters.len());
        let mut input_elements = 0usize;
        for ((name, logic), decl) in entry.adapters.iter().zip(&entry.manifest.inputs) {
            let value = evaluate(datalogic, logic, &root).map_err(|e| {
                evaluation_refused(
                    Some(&labels),
                    Category::Adapter,
                    &format!("adapter for input '{name}' failed"),
                    &e,
                )
            })?;
            let tensor = match value {
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
            if tensor.dtype().name() != decl.dtype || tensor.shape() != decl.shape.as_slice() {
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
        let timed = tokio::time::timeout_at(deadline, async {
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
                    .saturating_sub(host.inference_permits.available_permits())
                    as u64,
            );

            let run_started = Instant::now();
            let outputs = tokio::task::spawn_blocking(move || loaded.run(tensors))
                .await
                .map_err(|e| {
                    refuse(
                        Some(&labels),
                        Failure::new(Category::Run, "the inference did not complete")
                            .with_detail(e),
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
        })
        .await;
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
            if tensor.dtype().name() != decl.dtype || tensor.shape() != decl.shape.as_slice() {
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
        let object = OwnedDataValue::Object(
            entry
                .manifest
                .outputs
                .iter()
                .zip(outputs)
                .map(|(decl, tensor)| (decl.name.clone(), OwnedDataValue::Tensor(Arc::new(tensor))))
                .collect(),
        );
        let value = if raw {
            object
        } else {
            evaluate(datalogic, &entry.result, &object).map_err(|e| {
                evaluation_refused(
                    Some(&labels),
                    Category::Adapter,
                    "the result expression failed",
                    &e,
                )
            })?
        };

        // Written last, together: nothing lands on a failure above.
        ctx.set(output, value);
        if let Some(path) = stats_output {
            ctx.set_json(
                path,
                &json!({
                    "id": entry.id,
                    "version": entry.version,
                    "digest": entry.digest,
                    "runtime": labels.runtime,
                    "device": device,
                    "parameters": entry.stats.as_ref().map(|s| s.parameters),
                    "artifact_bytes": entry.stats.as_ref().map(|s| s.artifact_bytes),
                    "queued_ms": queued_for.as_secs_f64() * 1000.0,
                    "inference_ms": inference_took.as_secs_f64() * 1000.0,
                    "cold_load": cold_load,
                }),
            );
        }
        Ok(labels)
    }
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
    let bytes = host.artifacts.bytes(entry).await?;
    let entry = entry.clone();
    let device = device.to_string();
    tokio::task::spawn_blocking(move || runtime.load(&bytes, &entry.manifest, &device))
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
        let set = ModelSet::load_active(&[row], &config, true, generation.engine.datalogic());
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

    /// The pieces of a message: a shape as `dtype[dims]`, a value's kind.
    #[test]
    fn messages_spell_shapes_and_kinds() {
        assert_eq!(dims(&[1, 2, 6, 7]), "1,2,6,7");
        assert_eq!(dims(&[]), "");
        assert_eq!(kind_of(&OwnedDataValue::Null), "null");
        assert_eq!(kind_of(&OwnedDataValue::from(&json!([1]))), "a list");
        assert_eq!(
            kind_of(&OwnedDataValue::from(&json!({"a": 1}))),
            "an object"
        );
        assert_eq!(kind_of(&OwnedDataValue::from(&json!("s"))), "a string");
    }
}
