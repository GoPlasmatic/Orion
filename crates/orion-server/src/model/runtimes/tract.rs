//! The tract runtime: the first [`ModelRuntime`], over the `tract` facade.
//!
//! A load is the facade's sequence — parse the protobuf into an inference
//! model, pin every input to the binding's dtype and shape, run shape
//! inference, type and declutter the graph, prepare it for a device — and an
//! inference is one call on the prepared plan. Everything here is
//! **synchronous** work on the calling thread (a Metal or CUDA plan still
//! marshals there): a load takes milliseconds to seconds and an inference the
//! model's own time, so a caller on the async runtime — admission, the
//! `model_infer` handler — runs both on the blocking pool. The loaded model
//! holds the plan behind an `Arc` and [`LoadedModel::run`] takes `&self`, so
//! one loaded model serves any number of concurrent inferences.
//!
//! Names are matched through [`crate::model::onnx`], not the facade: the
//! facade names an output after the node that *produces* it, which an
//! exporter names freely (`/fc2/Gemm`), while the protobuf's `graph.output`
//! carries the tensor name the manifest speaks — in the order tract selects
//! its outputs, which is what makes the index mapping sound. The two readers
//! are pinned to each other at load by their input and output counts.

use std::sync::{Arc, Mutex, OnceLock};

use ::tract::prelude::*;
use dataflow_rs::datavalue::{DType, OwnedDataTensor};

use super::{LoadBinding, LoadError, LoadedModel, ModelRuntime, RunError, devices_of, formats_of};
use crate::model::manifest::Bindings;
use crate::model::onnx;

/// The name the runtime registers under — the `tract` in
/// [`super::NAMES`].
pub const NAME: &str = "tract";

/// The tract runtime. Stateless: every loaded model owns its own plan.
#[derive(Debug, Default, Clone, Copy)]
pub struct TractRuntime;

impl TractRuntime {
    /// The devices this build offers, computed once: `cpu` always, and each
    /// accelerator name in [`devices_of`] that tract's registry answers for
    /// — `metal` on an Apple build with a Metal device, `cuda` where the
    /// feature is on and a toolkit is present. The static table is the
    /// *known* set, which config validation accepts on every build; this is
    /// the subset a load here can use, and a load asking for the difference
    /// fails at stage `device` naming it.
    pub fn available_devices() -> &'static [&'static str] {
        static DEVICES: OnceLock<Vec<&'static str>> = OnceLock::new();
        DEVICES.get_or_init(|| {
            devices_of(NAME)
                .into_iter()
                .flatten()
                .copied()
                .filter(|device| *device == "cpu" || ::tract::runtime_for_name(device).is_ok())
                .collect()
        })
    }
}

impl ModelRuntime for TractRuntime {
    fn name(&self) -> &'static str {
        NAME
    }

    fn devices(&self) -> &'static [&'static str] {
        Self::available_devices()
    }

    fn formats(&self) -> &'static [&'static str] {
        // The table's own row: tract loads ONNX and nothing else here.
        formats_of(NAME).unwrap_or_default()
    }

    fn load(
        &self,
        bytes: &[u8],
        binding: &LoadBinding,
        device: &str,
    ) -> Result<Arc<dyn LoadedModel>, LoadError> {
        Ok(Arc::new(Self::prepare(bytes, binding, device)?))
    }
}

impl TractRuntime {
    /// [`ModelRuntime::load`], answering the concrete model.
    fn prepare(bytes: &[u8], binding: &LoadBinding, device: &str) -> Result<TractModel, LoadError> {
        // The device first: it is the cheapest check, and a graph parsed
        // for a device this build cannot run is work thrown away.
        let devices = Self::available_devices();
        if !devices.contains(&device) {
            return Err(LoadError::new(
                "device",
                format!(
                    "device '{device}' is not one this build of tract offers ({})",
                    devices.join(", ")
                ),
            ));
        }

        let graph = onnx::read_stats(bytes).map_err(|reason| LoadError::new("parse", reason))?;
        let mut model = ::tract::onnx()
            .and_then(|onnx| onnx.load_buffer(bytes))
            .map_err(|e| {
                LoadError::new("parse", format!("tract could not read the graph: {e:#}"))
            })?;
        let input_count = model.input_count().map_err(parse_error)?;
        let output_count = model.output_count().map_err(parse_error)?;
        if input_count != graph.input_names.len() || output_count != graph.output_names.len() {
            return Err(LoadError::new(
                "parse",
                format!(
                    "the graph declares {} inputs and {} outputs, but tract read {input_count} \
                     and {output_count}",
                    graph.input_names.len(),
                    graph.output_names.len()
                ),
            ));
        }

        let inputs = bind_inputs(&mut model, binding, &graph.input_names)?;
        let output_order = order_of(
            "outputs",
            "output",
            binding.output_names(),
            &graph.output_names,
        )?;

        model.analyse().map_err(|e| {
            LoadError::new(
                "parse",
                format!("the graph does not type-check with the manifest's inputs: {e:#}"),
            )
        })?;
        let typed = model
            .into_model()
            .map_err(|e| LoadError::new("parse", format!("the graph could not be typed: {e:#}")))?;
        // A named axis leaves the plan general: tract resolves the symbol on
        // every run. Where [`specialising_pays`] says so, the typed graph is
        // kept on the CPU and `run` prepares a plan for each concrete shape it
        // is fed (`shaped`).
        let reshapable = device == "cpu"
            && inputs
                .iter()
                .any(|slot| slot.shape.iter().any(|dim| dim.fixed().is_none()))
            && specialising_pays(bytes);
        let typed_kept = reshapable.then(|| typed.clone());
        let runnable = if device == "cpu" {
            typed.into_runnable()
        } else {
            ::tract::runtime_for_name(device).and_then(|runtime| runtime.prepare(typed))
        }
        .map_err(|e| {
            LoadError::new(
                "device",
                format!("tract could not prepare the graph for '{device}': {e:#}"),
            )
        })?;

        Ok(TractModel {
            digest: crate::crypto::sha256_digest(bytes),
            runnable,
            typed: typed_kept,
            shaped: Mutex::new(Vec::new()),
            inputs,
            output_order,
            resident_bytes: bytes.len(),
        })
    }
}

/// The facade's errors are `anyhow` chains; `{:#}` prints the whole chain,
/// and taking `Display` keeps `anyhow` out of this crate's dependencies.
fn parse_error(e: impl std::fmt::Display) -> LoadError {
    LoadError::new("parse", format!("{e:#}"))
}

/// One bound input as the plan expects it: where it goes, and what a
/// tensor handed in must be.
struct InputSlot {
    name: String,
    /// The graph's input index.
    index: usize,
    dtype: DType,
    /// The declared shape, a named dimension included — checked per run
    /// through one [`Bindings`] for the call, so a name means the same axis
    /// across every input.
    shape: Vec<crate::model::manifest::Dim>,
}

/// Pin every bound input to its declared dtype and shape, in graph order.
/// Both directions are checked — a declared input the graph lacks and a
/// graph input the binding leaves out — because a plan with an unfed input
/// would fail every run with tract's words instead of the manifest's.
fn bind_inputs(
    model: &mut InferenceModel,
    binding: &LoadBinding,
    graph_inputs: &[String],
) -> Result<Vec<InputSlot>, LoadError> {
    let order = order_of("inputs", "input", binding.input_names(), graph_inputs)?;
    if let Some(missing) = graph_inputs
        .iter()
        .find(|name| !binding.inputs().iter().any(|input| &input.name == *name))
    {
        return Err(LoadError::new(
            "inputs",
            format!(
                "graph input '{missing}' is not declared by the manifest; the graph's inputs \
                 are: {}",
                quoted(graph_inputs)
            ),
        ));
    }
    let mut slots = Vec::with_capacity(order.len());
    for (input, index) in binding.inputs().iter().zip(order) {
        let dtype = input.dtype().ok_or_else(|| {
            LoadError::new(
                "inputs",
                format!(
                    "input '{}': dtype '{}' is not one a manifest may declare",
                    input.name, input.dtype
                ),
            )
        })?;
        let datum_type = datum_type_of(dtype).map_err(|reason| {
            LoadError::new("inputs", format!("input '{}': {reason}", input.name))
        })?;
        let spec = fact_spec(&input.shape, datum_type);
        model.set_input_fact(index, spec.as_str()).map_err(|e| {
            LoadError::new("inputs", format!("input '{}' as {spec}: {e:#}", input.name))
        })?;
        slots.push(InputSlot {
            name: input.name.clone(),
            index,
            dtype,
            shape: input.shape.clone(),
        });
    }
    Ok(slots)
}

/// The graph index of each bound name, in manifest order. A name the
/// graph lacks fails at `stage`, listing the graph's names.
fn order_of<'a>(
    stage: &'static str,
    kind: &str,
    names: impl Iterator<Item = &'a str>,
    graph: &[String],
) -> Result<Vec<usize>, LoadError> {
    names
        .map(|name| {
            graph.iter().position(|g| g == name).ok_or_else(|| {
                LoadError::new(
                    stage,
                    format!(
                        "{kind} '{name}' is not a graph {kind}; the graph's {stage} are: {}",
                        quoted(graph)
                    ),
                )
            })
        })
        .collect()
}

fn quoted(names: &[String]) -> String {
    names
        .iter()
        .map(|n| format!("'{n}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The fact spec tract parses: the dimensions comma-joined, then the datum
/// type, lowercase — `1,2,6,7,f32`.
///
/// A named dimension goes in as its name, `1,2,H,7,f32`, which is what
/// tract's own spec parser takes: it maps each dimension through
/// `parse_tdim`, and a symbol survives into the plan, which resolves it
/// from the actual inputs at run time. So a variable axis costs nothing
/// here — the declaration was always the only thing in the way.
fn fact_spec(shape: &[crate::model::manifest::Dim], datum_type: DatumType) -> String {
    let mut spec = shape
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    spec.push(',');
    spec.push_str(&format!("{datum_type:?}").to_lowercase());
    spec
}

/// datavalue → tract, for every dtype a manifest may declare. The two
/// half-precision types have no adapter on the datavalue side, so a graph
/// with an f16 boundary declares f32 and casts inside.
fn datum_type_of(dtype: DType) -> Result<DatumType, String> {
    Ok(match dtype {
        DType::Bool => DatumType::Bool,
        DType::I8 => DatumType::I8,
        DType::U8 => DatumType::U8,
        DType::I16 => DatumType::I16,
        DType::U16 => DatumType::U16,
        DType::I32 => DatumType::I32,
        DType::U32 => DatumType::U32,
        DType::I64 => DatumType::I64,
        DType::U64 => DatumType::U64,
        DType::F32 => DatumType::F32,
        DType::F64 => DatumType::F64,
        DType::F16 | DType::BF16 => {
            return Err(format!(
                "dtype '{}' is not supported: declare the model's f32 boundary and let the \
                 graph cast",
                dtype.name()
            ));
        }
        // `DType` is non-exhaustive: a dtype a later datavalue adds is
        // refused here until this table learns it.
        _ => {
            return Err(format!(
                "dtype '{}' is not one the tract runtime maps",
                dtype.name()
            ));
        }
    })
}

/// tract → datavalue, for what a graph may produce.
fn dtype_of(datum_type: DatumType) -> Result<DType, String> {
    Ok(match datum_type {
        DatumType::Bool => DType::Bool,
        DatumType::I8 => DType::I8,
        DatumType::U8 => DType::U8,
        DatumType::I16 => DType::I16,
        DatumType::U16 => DType::U16,
        DatumType::I32 => DType::I32,
        DatumType::U32 => DType::U32,
        DatumType::I64 => DType::I64,
        DatumType::U64 => DType::U64,
        DatumType::F32 => DType::F32,
        DatumType::F64 => DType::F64,
        DatumType::F16 => {
            return Err(
                "the graph produced an f16 tensor, which 1.x cannot decode; cast to f32 \
                        inside the graph"
                    .to_string(),
            );
        }
    })
}

/// A graph prepared by tract for one device.
///
/// `resident_bytes` is the artifact's size, as an approximation: the plan
/// keeps the weights, which are most of an artifact, plus per-node state
/// this runtime does not measure, minus the protobuf framing. The ceiling
/// it counts against (`models.max_loaded_bytes`) is a budget, not an
/// allocator, and the artifact's size is the one number every runtime can
/// report the same way.
struct TractModel {
    digest: String,
    /// The general plan, every named axis still a symbol.
    runnable: Runnable,
    /// The typed graph, kept on the CPU when an input has a named axis and
    /// [`specialising_pays`], from which a plan for one concrete shape is
    /// prepared on first use.
    typed: Option<Model>,
    /// The plans prepared so far, keyed by what each named axis was bound
    /// to. Bounded by [`MAX_SHAPED_PLANS`], least recently run out first.
    shaped: Mutex<ShapedPlans>,
    /// Manifest order.
    inputs: Vec<InputSlot>,
    /// The graph output index of each manifest output, in manifest order.
    output_order: Vec<usize>,
    resident_bytes: usize,
}

/// How many concrete-shape plans one loaded model keeps. A plan is the
/// weights again plus its buffers, so this is the bound on what a model with
/// a named axis costs past its artifact. A caller cycling through more
/// shapes than this pays a plan preparation for the one least recently run,
/// and still runs the concrete plan: which plan runs is a function of the
/// graph and the shape, never of what this node happened to see before.
const MAX_SHAPED_PLANS: usize = 8;

/// The plans prepared so far, keyed by what each named axis was bound to,
/// most recently run last.
type ShapedPlans = Vec<(Vec<(String, usize)>, Runnable)>;

/// How much more of a graph's work must be in spatial convolutions than in
/// plain matrix products for a plan per concrete shape to be prepared.
///
/// Once every axis is known, tract lays a matrix product out with its larger
/// dimension first (`EinSumMatMul::codegen`, `m < n` transposes). A 1x1
/// convolution is a matrix product whose larger dimension is the board, so
/// specialising turns it around and writes its output transposed: about 5x
/// slower (#369). A 3x3 convolution takes its own im2col path, which a
/// known geometry makes 1.3x to 1.6x faster. Measured on graphs of both and
/// of mixes (tract 0.23.8, one thread), specialising won from a spatial to
/// pointwise ratio of about 7.5 up and lost below 1; eight leaves margin on
/// the side of the plan every graph already had.
const SPATIAL_TO_POINTWISE: u64 = 8;

/// Whether a plan per concrete shape is worth preparing for the graph in
/// `bytes`: decided from the graph alone, so every node makes the same
/// choice for the same artifact. A graph whose work the reader cannot
/// account for keeps its one general plan.
fn specialising_pays(bytes: &[u8]) -> bool {
    match crate::model::conv_work(bytes) {
        Ok(Some(work)) => {
            work.spatial > 0 && work.spatial >= work.pointwise.saturating_mul(SPATIAL_TO_POINTWISE)
        }
        Ok(None) | Err(_) => false,
    }
}

impl TractModel {
    /// The plan to run for the sizes `bindings` holds: the one prepared for
    /// exactly those, preparing it on first use.
    ///
    /// The same graph with its symbols substituted computes the same
    /// function; `a_concrete_shape_runs_on_its_own_plan_and_agrees` pins the
    /// two plans' outputs against each other for the fixture. What changes is
    /// the kernels tract can choose once the geometry is known.
    fn plan_for(&self, bindings: &Bindings) -> Runnable {
        let Some(typed) = &self.typed else {
            return self.runnable.clone();
        };
        let key: Vec<(String, usize)> = bindings
            .bound()
            .map(|(name, size)| (name.to_string(), size))
            .collect();
        if let Some(plan) = Self::recall(&self.shaped, &key) {
            return plan;
        }
        // Prepared outside the lock: it is the one slow step, and two calls
        // racing on a new shape each prepare one and keep the first.
        let mut graph = typed.clone();
        let symbols = key.iter().fold(SetSymbols::new(), |set, (name, size)| {
            set.value(name.clone(), i64::try_from(*size).unwrap_or(i64::MAX))
        });
        let Ok(plan) = graph
            .transform(symbols)
            .and_then(|()| graph.into_runnable())
        else {
            // A graph tract cannot specialise at this shape runs generally,
            // at this shape, on every node alike.
            return self.runnable.clone();
        };
        if let Some(existing) = Self::recall(&self.shaped, &key) {
            return existing;
        }
        let mut shaped = self
            .shaped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if shaped.len() >= MAX_SHAPED_PLANS {
            shaped.remove(0);
        }
        shaped.push((key, plan.clone()));
        plan
    }

    /// The plan prepared for `key`, marked most recently run.
    fn recall(shaped: &Mutex<ShapedPlans>, key: &[(String, usize)]) -> Option<Runnable> {
        let mut shaped = shaped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let at = shaped.iter().position(|(k, _)| k.as_slice() == key)?;
        let entry = shaped.remove(at);
        let plan = entry.1.clone();
        shaped.push(entry);
        Some(plan)
    }
}

impl LoadedModel for TractModel {
    fn digest(&self) -> &str {
        &self.digest
    }

    fn resident_bytes(&self) -> usize {
        self.resident_bytes
    }

    fn run(&self, inputs: Vec<OwnedDataTensor>) -> Result<Vec<OwnedDataTensor>, RunError> {
        if inputs.len() != self.inputs.len() {
            return Err(RunError::new(
                "inputs",
                format!(
                    "{} tensors given, {} declared by the manifest",
                    inputs.len(),
                    self.inputs.len()
                ),
            ));
        }
        // Manifest order in, graph order to the plan. `inputs` is a
        // permutation of the graph's inputs (bind_inputs checked both
        // directions), so every slot below is filled.
        let mut by_graph_index: Vec<Option<Tensor>> =
            (0..self.inputs.len()).map(|_| None).collect();
        // One set of bindings for the run, so a named dimension means the
        // same axis across every input the plan is fed.
        let mut bindings = Bindings::default();
        for (tensor, slot) in inputs.iter().zip(&self.inputs) {
            let fits =
                tensor.dtype() == slot.dtype && bindings.check(&slot.shape, tensor.shape()).is_ok();
            if !fits {
                return Err(RunError::new(
                    "inputs",
                    format!(
                        "input '{}' is {}{:?}, the manifest declares {}[{}]",
                        slot.name,
                        tensor.dtype().name(),
                        tensor.shape(),
                        slot.dtype.name(),
                        slot.shape
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ));
            }
            let datum_type = datum_type_of(slot.dtype).map_err(|reason| {
                RunError::new("inputs", format!("input '{}': {reason}", slot.name))
            })?;
            let converted = Tensor::from_bytes(datum_type, tensor.shape(), tensor.data())
                .map_err(|e| RunError::new("inputs", format!("input '{}': {e:#}", slot.name)))?;
            by_graph_index[slot.index] = Some(converted);
        }
        let graph_inputs: Vec<Tensor> = by_graph_index.into_iter().flatten().collect();

        let outputs = self
            .plan_for(&bindings)
            .run(graph_inputs)
            .map_err(|e| RunError::new("run", format!("{e:#}")))?;

        self.output_order
            .iter()
            .enumerate()
            .map(|(position, &index)| {
                let tensor = outputs.get(index).ok_or_else(|| {
                    RunError::new(
                        "outputs",
                        format!(
                            "output {position}: the plan produced {} outputs and graph output \
                             {index} is not among them",
                            outputs.len()
                        ),
                    )
                })?;
                let (datum_type, shape, data) = tensor
                    .as_bytes()
                    .map_err(|e| RunError::new("outputs", format!("output {position}: {e:#}")))?;
                let dtype = dtype_of(datum_type).map_err(|reason| {
                    RunError::new("outputs", format!("output {position}: {reason}"))
                })?;
                OwnedDataTensor::from_bytes(dtype, shape, data)
                    .map_err(|e| RunError::new("outputs", format!("output {position}: {e}")))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::fixture;
    use crate::model::manifest::Manifest;

    /// How far an accelerator may land from the CPU path, element-wise and
    /// absolute, before it is a different computation rather than a
    /// different order of the same one. See
    /// [`metal_agrees_with_the_cpu_path_where_this_build_has_it`].
    const DEVICE_AGREEMENT: f32 = 1e-5;

    fn load(manifest: &Manifest, device: &str) -> Result<Arc<dyn LoadedModel>, LoadError> {
        TractRuntime.load(fixture::ONNX, &LoadBinding::of(manifest), device)
    }

    /// A load expected to fail. (`expect_err` needs the `Ok` type to be
    /// `Debug`, and a loaded model is not.)
    fn load_err(manifest: &Manifest, device: &str) -> LoadError {
        match load(manifest, device) {
            Err(err) => err,
            Ok(_) => unreachable!("the load was expected to fail"),
        }
    }

    /// A board of ones: every cell occupied, which no game reaches, but
    /// which drives a non-zero activation through both layers.
    fn ones_board() -> Vec<OwnedDataTensor> {
        let ones = vec![1.0f32; 84];
        vec![OwnedDataTensor::from_slice(vec![1, 2, 6, 7], &ones).expect("a valid tensor")]
    }

    #[test]
    fn the_fixture_loads_on_cpu_and_runs() {
        let manifest = fixture::manifest();
        let model = load(&manifest, "cpu").expect("the fixture loads");
        assert_eq!(model.digest(), crate::crypto::sha256_digest(fixture::ONNX));
        assert_eq!(model.resident_bytes(), fixture::ONNX.len());

        // Zero biases and a zero board: every activation is zero, so the
        // policy is too — the shape and dtype are the assertion.
        let inputs = manifest.zero_inputs().expect("zero inputs");
        let out = model.run(inputs).expect("runs");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].dtype(), DType::F32);
        assert_eq!(out[0].shape(), [1, 7]);
        let policy = out[0].as_slice::<f32>().expect("f32 data");
        assert!(policy.iter().all(|v| *v == 0.0), "{policy:?}");

        // A board of ones reaches the weights.
        let out = model.run(ones_board()).expect("runs");
        let policy = out[0].as_slice::<f32>().expect("f32 data");
        assert!(policy.iter().any(|v| *v != 0.0), "{policy:?}");
        assert!(policy.iter().all(|v| v.is_finite()), "{policy:?}");

        // The same model, from many threads at once.
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let model = &model;
                scope.spawn(move || {
                    for _ in 0..8 {
                        model.run(ones_board()).expect("runs concurrently");
                    }
                });
            }
        });
    }

    /// One graph, two bindings, two sessions.
    ///
    /// The `two-out` manifests declare the same two graph outputs in
    /// opposite orders, and both are `f32[1, 2]`. A session built for one
    /// and handed to the other therefore answers with the tensors swapped
    /// and nothing to show for it — which is what keys the session cache on
    /// the binding as well as the bytes. Here each binding gets its own
    /// load, and each must report `double = x * 2` and `shift = x + 100`.
    #[test]
    fn two_bindings_over_one_graph_are_two_sessions() {
        use std::collections::BTreeMap;

        let (order_a, order_b) = fixture::two_out();
        let outputs = |manifest: &Manifest| {
            let model = TractRuntime
                .load(fixture::TWO_OUT_ONNX, &LoadBinding::of(manifest), "cpu")
                .expect("the two-out fixture loads");
            let x = vec![
                OwnedDataTensor::from_slice(vec![1, 2], &[1.0f32, 2.0]).expect("a valid tensor"),
            ];
            manifest
                .output_names()
                .zip(model.run(x).expect("runs"))
                .map(|(name, tensor)| {
                    (
                        name.to_string(),
                        tensor.as_slice::<f32>().expect("f32 data").to_vec(),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };
        let ground_truth = BTreeMap::from([
            ("double".to_string(), vec![2.0f32, 4.0]),
            ("shift".to_string(), vec![101.0f32, 102.0]),
        ]);
        assert_eq!(outputs(&order_a), ground_truth);
        assert_eq!(outputs(&order_b), ground_truth);
        assert_ne!(
            LoadBinding::of(&order_a).fingerprint(),
            LoadBinding::of(&order_b).fingerprint(),
            "and the two sessions are not one cache entry"
        );
    }

    #[test]
    fn the_build_offers_cpu_and_the_registry_lists_it() {
        let devices = TractRuntime.devices();
        assert!(devices.contains(&"cpu"), "{devices:?}");
        for device in devices {
            assert!(
                devices_of(NAME).is_some_and(|known| known.contains(device)),
                "{device} must be a known device"
            );
        }
        assert_eq!(TractRuntime.name(), "tract");
        assert_eq!(TractRuntime.formats(), ["onnx"]);
        assert!(super::super::NAMES.contains(&NAME));
    }

    #[test]
    fn a_manifest_input_the_graph_lacks_fails_at_inputs_naming_the_graphs() {
        let mut manifest = fixture::manifest();
        manifest.inputs[0].name = "boards".to_string();
        let err = load_err(&manifest, "cpu");
        assert_eq!(err.stage, "inputs");
        assert!(err.message.contains("'boards'"), "{err}");
        assert!(err.message.contains("'board'"), "{err}");

        // The other direction: a graph input the manifest leaves out.
        let mut manifest = fixture::manifest();
        manifest.inputs.clear();
        let err = load_err(&manifest, "cpu");
        assert_eq!(err.stage, "inputs");
        assert!(
            err.message.contains("not declared by the manifest"),
            "{err}"
        );
    }

    #[test]
    fn a_manifest_output_the_graph_lacks_fails_at_outputs() {
        let mut manifest = fixture::manifest();
        manifest.outputs[0].name = "logits".to_string();
        let err = load_err(&manifest, "cpu");
        assert_eq!(err.stage, "outputs");
        assert!(err.message.contains("'policy'"), "{err}");
    }

    #[test]
    fn a_tensor_that_does_not_fit_fails_at_inputs() {
        let manifest = fixture::manifest();
        let model = load(&manifest, "cpu").expect("loads");

        let wrong_dtype =
            vec![OwnedDataTensor::from_slice(vec![1, 2, 6, 7], &[0i64; 84]).expect("tensor")];
        let err = model.run(wrong_dtype).expect_err("i64 for f32");
        assert_eq!(err.stage, "inputs");
        assert!(err.message.contains("i64[1, 2, 6, 7]"), "{err}");
        assert!(err.message.contains("f32[1, 2, 6, 7]"), "{err}");

        let wrong_shape =
            vec![OwnedDataTensor::from_slice(vec![2, 6, 7], &[0f32; 84]).expect("tensor")];
        let err = model.run(wrong_shape).expect_err("rank 3");
        assert_eq!(err.stage, "inputs");

        let err = model.run(Vec::new()).expect_err("none");
        assert_eq!(err.stage, "inputs");
        assert!(err.message.contains("0 tensors given"), "{err}");
    }

    #[test]
    fn an_unknown_device_fails_at_device() {
        let err = load_err(&fixture::manifest(), "tpu");
        assert_eq!(err.stage, "device");
        assert!(err.message.contains("'tpu'"), "{err}");
        assert!(err.message.contains("cpu"), "{err}");
    }

    /// Where this build has Metal (an Apple target with a device), the
    /// same graph loads and runs there; elsewhere the assertion is that
    /// the device is honestly absent.
    ///
    /// The comparison against the CPU path is the point: an accelerator
    /// **agrees with the CPU to a tolerance, not to the bit**, which is why
    /// `docs/src/concepts/models.md` scopes its determinism claim to the CPU
    /// path and tells an operator running a scored competition to keep the
    /// fleet on one device. Measured on an M-series host against this
    /// fixture, the largest element-wise difference is ~9.3e-8 — f32 epsilon
    /// scale — so [`DEVICE_AGREEMENT`] is loose enough not to be flaky and
    /// tight enough that a runtime upgrade computing something genuinely
    /// different fails here rather than in a tournament.
    #[test]
    fn metal_agrees_with_the_cpu_path_where_this_build_has_it() {
        let manifest = fixture::manifest();
        if !TractRuntime.devices().contains(&"metal") {
            let err = load_err(&manifest, "metal");
            assert_eq!(err.stage, "device");
            return;
        }
        let model = load(&manifest, "metal").expect("loads on metal");
        let out = model.run(ones_board()).expect("runs on metal");
        assert_eq!(out[0].dtype(), DType::F32);
        assert_eq!(out[0].shape(), [1, 7]);
        let metal = out[0].as_slice::<f32>().expect("f32 data");
        assert!(metal.iter().any(|v| *v != 0.0), "{metal:?}");
        assert!(metal.iter().all(|v| v.is_finite()), "{metal:?}");

        let cpu = load(&manifest, "cpu").expect("loads on cpu");
        let cpu = cpu.run(ones_board()).expect("runs on cpu");
        let cpu = cpu[0].as_slice::<f32>().expect("f32 data");
        let worst = cpu
            .iter()
            .zip(metal)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            worst <= DEVICE_AGREEMENT,
            "metal and cpu differ by {worst:e}, over the {DEVICE_AGREEMENT:e} tolerance: \
             cpu {cpu:?} vs metal {metal:?}"
        );

        // One session, many threads — the accelerator path shares a queue
        // where the CPU path shares a plan, and `LoadedModel` promises both
        // are `Sync`.
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let model = &model;
                scope.spawn(move || {
                    for _ in 0..8 {
                        model.run(ones_board()).expect("runs concurrently on metal");
                    }
                });
            }
        });
    }

    /// One session serves a variable axis at whatever each call brings
    /// (#318).
    ///
    /// `scale.onnx` declares its first axis symbolic in the file itself, so
    /// the graph really does take any N; the manifest names the same axis
    /// `"N"`, which goes into the fact spec verbatim and survives into the
    /// plan. Before, a manifest could only say a number, and the only ways
    /// to serve this graph were to declare a maximum and pad every call to
    /// it, or to register one model per shape.
    #[test]
    fn one_session_serves_every_size_a_named_axis_takes() {
        let manifest = crate::model::fixture::dynamic();
        let loaded = TractRuntime
            .load(
                crate::model::fixture::DYNAMIC_ONNX,
                &LoadBinding::of(&manifest),
                "cpu",
            )
            .expect("loads");

        for rows in [1usize, 2, 5] {
            let data: Vec<f32> = (0..rows * 3).map(|i| i as f32).collect();
            let bytes: Vec<u8> = data.iter().flat_map(|f| f.to_le_bytes()).collect();
            let tensor =
                OwnedDataTensor::from_bytes(DType::F32, vec![rows, 3], &bytes).expect("a tensor");
            let out = loaded.run(vec![tensor]).expect("runs");
            assert_eq!(out[0].shape(), [rows, 3], "N = {rows}");
            let (words, rest) = out[0].data().as_chunks::<4>();
            assert!(rest.is_empty(), "whole f32s");
            let doubled: Vec<f32> = words.iter().copied().map(f32::from_le_bytes).collect();
            assert_eq!(
                doubled,
                data.iter().map(|f| f * 2.0).collect::<Vec<_>>(),
                "N = {rows}"
            );
        }

        // The fixed axis is still exact, and the message names the
        // declaration rather than a number the manifest never wrote.
        let bytes = vec![0u8; 4 * 2 * 4];
        let wrong = OwnedDataTensor::from_bytes(DType::F32, vec![2, 4], &bytes).expect("a tensor");
        let err = loaded.run(vec![wrong]).expect_err("4 is not 3");
        assert_eq!(err.stage, "inputs");
        assert!(err.message.contains("f32[N, 3]"), "{}", err.message);
    }

    /// An i8 board of `[1, 7, h, w]`, filled deterministically.
    fn board(h: usize, w: usize) -> (Vec<u8>, OwnedDataTensor) {
        let bytes: Vec<u8> = (0..7 * h * w)
            .map(|i| (((i * 31) % 7) as i8 - 3) as u8)
            .collect();
        let tensor =
            OwnedDataTensor::from_bytes(DType::I8, vec![1, 7, h, w], &bytes).expect("a tensor");
        (bytes, tensor)
    }

    /// A graph whose work is in 3x3 convolutions runs each board size on a
    /// plan prepared for it, and that plan answers what the general plan
    /// does. Past [`MAX_SHAPED_PLANS`] sizes the least recently run plan
    /// makes room: the cache stays bounded, and a size is never served by
    /// the general plan because of what came before it.
    #[test]
    fn a_concrete_shape_runs_on_its_own_plan_and_agrees() {
        let manifest = crate::model::fixture::board();
        let model = TractRuntime::prepare(
            crate::model::fixture::BOARD_ONNX,
            &LoadBinding::of(&manifest),
            "cpu",
        )
        .expect("loads");
        assert!(
            model.typed.is_some(),
            "a spatial graph with a named axis keeps its typed graph on the CPU"
        );

        let sizes: Vec<(usize, usize)> = (1..=MAX_SHAPED_PLANS + 3)
            .map(|n| (n + 2, 2 * n + 1))
            .collect();
        for &(h, w) in sizes.iter().chain(&sizes) {
            let (bytes, tensor) = board(h, w);
            let out = model.run(vec![tensor]).expect("runs");
            let general = model
                .runnable
                .run(vec![
                    Tensor::from_bytes(DatumType::I8, &[1, 7, h, w], &bytes).expect("t"),
                ])
                .expect("the general plan runs");
            let (_, _, want) = general[0].as_bytes().expect("bytes");
            assert_eq!(out[0].data(), want, "{h}x{w}: the two plans disagree");
            let shaped = model.shaped.lock().expect("unpoisoned");
            assert_eq!(
                shaped.last().map(|(key, _)| key.clone()),
                Some(vec![("H".to_string(), h), ("W".to_string(), w)]),
                "{h}x{w} ran on its own plan"
            );
        }
        let shaped = model.shaped.lock().expect("unpoisoned");
        assert_eq!(shaped.len(), MAX_SHAPED_PLANS, "bounded");
    }

    /// The same boundary with 1x1 layers only: specialising would lay each
    /// matrix product out transposed and run slower, so the graph keeps its
    /// one general plan (#369). So does a graph with no matrix product at
    /// all, which has nothing to gain.
    #[test]
    fn a_pointwise_graph_keeps_its_general_plan() {
        let manifest = crate::model::fixture::board();
        let model = TractRuntime::prepare(
            crate::model::fixture::ONE_BY_ONE_ONNX,
            &LoadBinding::of(&manifest),
            "cpu",
        )
        .expect("loads");
        assert!(model.typed.is_none());
        let (_, tensor) = board(5, 6);
        assert_eq!(
            model.run(vec![tensor]).expect("runs")[0].shape(),
            [1, 5, 5, 6]
        );

        let manifest = crate::model::fixture::dynamic();
        let model = TractRuntime::prepare(
            crate::model::fixture::DYNAMIC_ONNX,
            &LoadBinding::of(&manifest),
            "cpu",
        )
        .expect("loads");
        assert!(model.typed.is_none());
    }

    /// The decision is the graph's alone, and follows the work.
    #[test]
    fn specialising_pays_where_spatial_work_dominates() {
        assert!(specialising_pays(crate::model::fixture::BOARD_ONNX));
        assert!(!specialising_pays(crate::model::fixture::ONE_BY_ONE_ONNX));
        assert!(!specialising_pays(crate::model::fixture::ONNX));
        assert!(!specialising_pays(crate::model::fixture::AS_LIST_ONNX));
        assert!(!specialising_pays(b"not a model"));
    }

    /// A graph with no named axis keeps no typed graph: its one plan is
    /// already concrete.
    #[test]
    fn a_fixed_shape_keeps_only_its_one_plan() {
        let manifest = crate::model::fixture::manifest();
        let model = TractRuntime::prepare(
            crate::model::fixture::ONNX,
            &LoadBinding::of(&manifest),
            "cpu",
        )
        .expect("loads");
        assert!(model.typed.is_none());
    }

    /// A fact spec passes a named dimension through as its name, which is
    /// the syntax tract's own parser takes.
    #[test]
    fn a_fact_spec_writes_a_named_dimension_as_its_name() {
        let shape = vec![
            crate::model::manifest::Dim::Named("N".to_string()),
            crate::model::manifest::Dim::Fixed(3),
        ];
        assert_eq!(fact_spec(&shape, DatumType::F32), "N,3,f32");
    }

    #[test]
    fn dtypes_map_both_ways_and_halves_are_refused() {
        for dtype in DType::ALL {
            match datum_type_of(dtype) {
                Ok(datum_type) => {
                    assert_eq!(dtype_of(datum_type).expect("round trip"), dtype);
                    assert_eq!(
                        fact_spec(&crate::model::manifest::fixed_shape(&[1, 7]), datum_type),
                        format!("1,7,{}", dtype.name()),
                        "tract spells the type as datavalue does"
                    );
                }
                Err(reason) => {
                    assert!(!dtype.has_native_element(), "{dtype:?}: {reason}");
                    assert!(reason.contains("f32 boundary"), "{reason}");
                }
            }
        }
        assert!(dtype_of(DatumType::F16).is_err());
    }
}
