use super::*;

/// One call's fields, as the engine running it read them for this message:
/// each its value, or the refusal 1.12 gave for it. [`infer`] consumes them
/// in 1.12's order, so the first refusal a call reports is the one it
/// always reported.
pub(super) struct Request<'s> {
    /// When the call started: the model's own deadline counts from here.
    pub(super) started: Instant,
    pub(super) model: Result<OwnedDataValue, Refused>,
    /// `None` for `[models.default_runtime]`'s row for the format.
    pub(super) runtime: Result<Option<&'s str>, Refused>,
    /// The value `timeout_ms` gave, as the JSON 1.12 judged; `None` when it
    /// is unset or gave `null`.
    pub(super) timeout_ms: Result<Option<Value>, Refused>,
    pub(super) raw: Result<bool, Refused>,
    /// The `input` field's value: the root every adapter reads.
    pub(super) root: Result<OwnedDataValue, Refused>,
    pub(super) select: Option<Select<'s>>,
    /// Whether `stats_output` is set, and so whether the stats are built.
    pub(super) stats: bool,
    /// The deadline of the run the call belongs to (`Ctx::deadline`), if it
    /// has one.
    pub(super) run_deadline: Option<dataflow_rs::run::Instant>,
}

/// The task's own `select`, with the engine that compiled it.
pub(super) enum Select<'s> {
    /// A [`kit::DetachedExpr`] compiled on the program's expression engine.
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
            Self::V4(exprs, expr) => exprs
                .eval(expr, root)
                .map(|evaluated| (evaluated.value.into_owned(), evaluated.ops.unwrap_or(0)))
                .map_err(|e| evaluation_refused(Some(labels), Category::Adapter, CONTEXT, e)),
        }
    }
}

/// What a call produced, for the engine running it to write: the result,
/// the stats when `stats_output` is set, and where they go.
pub(super) struct Inferred {
    pub(super) labels: Labels,
    pub(super) value: OwnedDataValue,
    pub(super) stats: Option<Value>,
}

/// The shell around one call: the inference counted
/// under its outcome, a failure's operator-only detail logged, and its
/// error named once with the handler's name.
pub(super) fn settle(
    started: Instant,
    result: Result<Inferred, Refused>,
) -> Result<Inferred, HandlerError> {
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
pub(super) struct Labels {
    pub(super) model: String,
    pub(super) runtime: &'static str,
}

/// A refused call: the category it counts under, the error the task sees,
/// and the labels when the call got far enough to have them.
pub(super) struct Refused {
    pub(super) labels: Option<Labels>,
    pub(super) category: Category,
    pub(super) error: HandlerError,
}

impl Refused {
    /// The refusal, counted under `labels`: a field's, read before the
    /// labels were known and consumed after.
    fn labelled(mut self, labels: &Labels) -> Self {
        self.labels = Some(labels.clone());
        self
    }
}

pub(super) fn refuse(labels: Option<&Labels>, failure: Failure) -> Refused {
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
pub(super) fn evaluation_refused(
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

/// A field of the task's own that did not evaluate, on dataflow-rs 4: the
/// error under the field's name, its kind kept, so a budget refusal in
/// `model` or `input` keeps its code.
pub(super) fn field_refused(field: &str, e: dataflow_rs::Error) -> Refused {
    Refused {
        labels: None,
        category: Category::CallerInput,
        error: HandlerError::from(e.wrap(format!("'{field}'"))),
    }
}

pub(super) fn caller_input(labels: Option<&Labels>, message: String) -> Refused {
    refuse(labels, Failure::new(Category::CallerInput, message))
}

/// `[1,2,6,7]` as `1x2x6x7`-style text for a message. Takes a declared
/// shape as readily as an actual one, so a refusal can put `1,2,H,7`
/// beside the `1,2,5,7` that did not fit it.
pub(super) fn dims<D: std::fmt::Display>(shape: &[D]) -> String {
    shape
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// What kind of value an adapter produced, for the message that says it
/// was not a tensor.
pub(super) fn kind_of(value: &OwnedDataValue) -> &'static str {
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

/// What one call's expressions charged: every input adapter and, unless the
/// task asked for `raw`, the task's `select` or the manifest's result.
///
/// Two numbers because they answer different questions. `total` is what the
/// message cost. `peak` is what a ceiling has to clear, because
/// `engine.ops_budget` bounds one *evaluation* and a call is several: a
/// manifest with four adapters at 300 operations each is refused by a
/// budget of 250 and admitted by one of 400, whatever the total says.
#[derive(Default)]
pub(super) struct Charged {
    pub(super) total: u64,
    pub(super) peak: u64,
}

impl Charged {
    fn add(&mut self, ops: u64) {
        self.total = self.total.saturating_add(ops);
        self.peak = Ord::max(self.peak, ops);
    }
}

pub(super) struct InferencePermits {
    pub(super) held: Option<(OwnedSemaphorePermit, OwnedSemaphorePermit)>,
    pub(super) global: Arc<Semaphore>,
    pub(super) slots: usize,
}

impl Drop for InferencePermits {
    fn drop(&mut self) {
        self.held.take();
        crate::metrics::set_model_live_inferences(
            self.slots.saturating_sub(self.global.available_permits()) as u64,
        );
    }
}

/// A permit from `permits` before `deadline`, or a `permit` refusal naming
/// the ceiling that was full. With no deadline of the call's own, the wait
/// is the run's to end.
pub(super) async fn acquire(
    permits: &Arc<Semaphore>,
    deadline: Option<Instant>,
    ceiling: &str,
    labels: &Labels,
) -> Result<OwnedSemaphorePermit, Refused> {
    let acquire = permits.clone().acquire_owned();
    let acquired = match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, acquire).await,
        None => Ok(acquire.await),
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

/// The call, from the fields read to the value written.
pub(super) async fn infer(
    source: &ModelSource,
    host: Option<&Arc<InferenceHost>>,
    config: &ModelsConfig,
    request: Request<'_>,
) -> Result<Inferred, Refused> {
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
                        crate::model::runtimes::RuntimeSelection::NoDefault {
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
        let global = acquire(
            &host.inference_permits,
            deadline,
            "models.max_concurrent_inferences",
            &labels,
        )
        .await?;
        let own = acquire(
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

        let permits = InferencePermits {
            held: Some((global, own)),
            global: host.inference_permits.clone(),
            slots: host.inference_slots,
        };

        let run_started = Instant::now();
        let outputs = tokio::task::spawn_blocking(move || {
            // Timing out the waiter does not stop a running blocking job.
            // Capacity belongs to that job until it really ends.
            let _permits = permits;
            loaded.run(tensors)
        })
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

pub(super) async fn fetch_and_load(
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
    let load_slot = match &host.load_permits {
        Some(permits) => Some(
            permits
                .clone()
                .acquire_owned()
                .await
                .map_err(|e| LoadError::new("load", format!("the load slot is gone: {e}")))?,
        ),
        None => None,
    };
    let bytes = host.artifacts.bytes(entry).await?;
    let entry = entry.clone();
    let device = device.to_string();
    tokio::task::spawn_blocking(move || {
        let _load_slot = load_slot;
        runtime.load(&bytes, &entry.binding, &device)
    })
    .await
    .map_err(|e| LoadError::new("load", format!("the load did not complete: {e}")))?
}
