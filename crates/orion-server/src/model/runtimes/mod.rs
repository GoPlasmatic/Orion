//! The runtime abstraction: what it means to load a model and run it, the
//! names and devices this build knows, and the registry a generation reads.
//!
//! A runtime is a *mechanism* — tract today, another engine later — behind
//! two traits: [`ModelRuntime`] turns bytes plus a [`LoadBinding`] into a
//! [`LoadedModel`], and a loaded model turns tensors into tensors. Everything
//! Orion adds (adapters, limits, permits, timeouts, metrics) sits above the
//! traits, so a second runtime is a second `impl` and nothing else.
//!
//! [`NAMES`], [`devices_of`] and [`formats_of`] are a static table rather
//! than a query on a built registry because config validation reads them
//! before any runtime exists — a config is checked at startup, a runtime is
//! constructed after. The registry pins itself to the table: a runtime whose
//! `name()` the table does not list cannot be registered, so the two cannot
//! disagree. The table is the *known* vocabulary; what a build actually
//! offers is the runtime's [`ModelRuntime::devices`], which may be smaller
//! (`metal` on a Linux build, `cuda` without the toolkit) and is what a load
//! checks.
//!
//! Which runtime a model runs on is decided **per artifact format**: a
//! runtime declares the formats it serves ([`ModelRuntime::formats`]), and
//! `[models.default_runtime]` maps each format to the runtime that serves it
//! by default. [`ModelRuntimes::default_for`] is that lookup for a
//! manifest's format and [`ModelRuntimes::for_format`] the same check for a
//! runtime named explicitly; both answer with a [`RuntimeSelection`] naming
//! the format, the runtime and what was wrong. A second format (NNEF,
//! TFLite) or a second runtime is a row in the table and an `impl`, not a
//! config or schema break.
//!
//! [`self::tract`] is the one implementation; [`ModelRuntimes::builtin`]
//! builds the registry a node carries from `[models.runtimes]`.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use dataflow_rs::datavalue::{DType, OwnedDataTensor};
use serde::Serialize;

use super::manifest::Manifest;
use crate::config::ModelsConfig;

pub mod tract;

pub use self::tract::TractRuntime;

/// Every runtime this build knows the name of: `tract`, implemented by
/// [`TractRuntime`].
pub const NAMES: &[&str] = &["tract"];

/// Every artifact format some runtime in [`NAMES`] serves — the values a
/// manifest's `format` and the keys of `[models.default_runtime]` may take.
/// Pinned by test to the union of [`formats_of`] over [`NAMES`].
pub const KNOWN_FORMATS: &[&str] = &["onnx"];

/// The devices the tract runtime can be asked for. `cpu` is what every build
/// has; `metal` and `cuda` are accepted names whose availability the runtime
/// reports at load, so a config naming one is not refused on a build that
/// lacks it — the load is, with the reason.
const TRACT_DEVICES: &[&str] = &["cpu", "metal", "cuda"];

/// The formats the tract runtime loads.
const TRACT_FORMATS: &[&str] = &["onnx"];

/// The devices runtime `name` lists, or `None` for a name this build does not
/// know.
pub fn devices_of(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "tract" => Some(TRACT_DEVICES),
        _ => None,
    }
}

/// The artifact formats runtime `name` serves, or `None` for a name this
/// build does not know. The static twin of [`ModelRuntime::formats`], for
/// config validation.
pub fn formats_of(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "tract" => Some(TRACT_FORMATS),
        _ => None,
    }
}

/// The `'static` spelling of a runtime name, for metric labels: a label value
/// must be a registered name, never a string a row or a request chose.
pub fn intern(name: &str) -> Option<&'static str> {
    NAMES.iter().copied().find(|n| *n == name)
}

/// Why a model did not load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    /// `parse`, `device`, `memory`, `probe`, or a runtime's own stage.
    pub stage: &'static str,
    pub message: String,
}

impl LoadError {
    pub fn new(stage: &'static str, message: impl Into<String>) -> Self {
        Self {
            stage,
            message: message.into(),
        }
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.stage, self.message)
    }
}

impl std::error::Error for LoadError {}

/// Why an inference did not produce outputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunError {
    /// `input` (the tensors handed in do not fit the graph), `run` (the
    /// runtime failed mid-graph), or `output` (what came back does not match
    /// the manifest).
    pub stage: &'static str,
    pub message: String,
}

impl RunError {
    pub fn new(stage: &'static str, message: impl Into<String>) -> Self {
        Self {
            stage,
            message: message.into(),
        }
    }
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.stage, self.message)
    }
}

impl std::error::Error for RunError {}

/// Why no runtime was selected for a format: the answer of
/// [`ModelRuntimes::default_for`] and [`ModelRuntimes::for_format`] when
/// they refuse. Every message names the format, the runtime and what was
/// wrong, because the reader is an operator looking at an admission verdict
/// or a task failure, not at the config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeSelection {
    /// `[models.default_runtime]` has no row for the format.
    NoDefault { format: String },
    /// The runtime is not one this build lists in [`NAMES`].
    UnknownRuntime { runtime: String, format: String },
    /// The runtime is known but not registered on this node — disabled or
    /// absent under `[models.runtimes]`.
    NotEnabled { runtime: String, format: String },
    /// The runtime is registered but its [`ModelRuntime::formats`] does not
    /// include the format.
    DoesNotServe {
        runtime: String,
        format: String,
        serves: &'static [&'static str],
    },
}

impl fmt::Display for RuntimeSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDefault { format } => write!(
                f,
                "no default runtime for format '{format}' (models.default_runtime.{format} is \
                 not set)"
            ),
            Self::UnknownRuntime { runtime, format } => write!(
                f,
                "runtime '{runtime}' for format '{format}' is unknown: this build lists {}",
                NAMES.join(", ")
            ),
            Self::NotEnabled { runtime, format } => write!(
                f,
                "runtime '{runtime}' for format '{format}' is not enabled on this node \
                 (models.runtimes.{runtime}.enabled)"
            ),
            Self::DoesNotServe {
                runtime,
                format,
                serves,
            } => write!(
                f,
                "runtime '{runtime}' does not serve format '{format}'; it serves {}",
                serves
                    .iter()
                    .map(|s| format!("'{s}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

impl std::error::Error for RuntimeSelection {}

/// Everything a runtime reads out of a manifest to build a session: the
/// inputs it pins a fact for and the output names it resolves to graph
/// indices, both in manifest order.
///
/// [`ModelRuntime::load`] takes this rather than the manifest, and the
/// loaded-session cache keys on its [`fingerprint`](Self::fingerprint), so
/// what *shapes* a session and what *identifies* one are one value. A
/// session is a function of the bytes, this binding and the device; keyed
/// on the bytes alone, two models over one artifact shared a session and
/// the second was served the first's — silently, when the two declared the
/// same outputs in a different order.
///
/// What is deliberately absent is what keeps an artifact shared: the
/// model's name and version, its description, its adapters and its result
/// expression, and its outputs' dtypes and shapes. A load reads none of
/// them — the outputs' dtype and shape are checked per call against the
/// row's own manifest — so two manifests over one graph differing only in
/// those keep one resident session between them, which is the whole point
/// of registering one artifact twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadBinding {
    inputs: Vec<BoundInput>,
    outputs: Vec<String>,
    fingerprint: String,
}

/// One input as a load pins it: which graph tensor to feed, and the fact it
/// is given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BoundInput {
    /// The graph's input name.
    pub name: String,
    /// A datavalue dtype wire name, as the manifest spells it.
    pub dtype: String,
    /// The shape the fact pins, a named dimension included: a load is a
    /// function of the *declaration*, not of what some call brought, and
    /// two manifests that make one axis variable and fixed are two plans
    /// over one graph. A fixed dimension renders as the number it always
    /// did, so a manifest that names none fingerprints exactly as before.
    pub shape: Vec<super::manifest::Dim>,
}

impl BoundInput {
    /// The declared dtype. `None` only for a manifest that did not
    /// validate — the same answer [`super::InputDecl::dtype`] gives.
    pub fn dtype(&self) -> Option<DType> {
        super::manifest::parse_dtype(&self.dtype).ok()
    }
}

impl LoadBinding {
    /// The binding `manifest` imposes on its graph.
    pub fn of(manifest: &Manifest) -> Self {
        /// What the fingerprint is taken over. A derived struct of strings
        /// and numbers: the field order is this declaration's and there are
        /// no map keys, so the rendering is canonical without a sort.
        #[derive(Serialize)]
        struct Rendered<'a> {
            inputs: &'a [BoundInput],
            outputs: &'a [String],
        }

        let inputs: Vec<BoundInput> = manifest
            .inputs
            .iter()
            .map(|input| BoundInput {
                name: input.name.clone(),
                dtype: input.dtype.clone(),
                shape: input.shape.clone(),
            })
            .collect();
        let outputs: Vec<String> = manifest.output_names().map(str::to_string).collect();
        let rendered = serde_json::to_vec(&Rendered {
            inputs: &inputs,
            outputs: &outputs,
        })
        .expect("a binding of strings and numbers serializes");
        Self {
            fingerprint: crate::crypto::sha256_digest(&rendered),
            inputs,
            outputs,
        }
    }

    /// The inputs to pin, in manifest order.
    pub fn inputs(&self) -> &[BoundInput] {
        &self.inputs
    }

    /// Every input name, in manifest order.
    pub fn input_names(&self) -> impl Iterator<Item = &str> {
        self.inputs.iter().map(|input| input.name.as_str())
    }

    /// Every output name, in manifest order.
    pub fn output_names(&self) -> impl Iterator<Item = &str> {
        self.outputs.iter().map(String::as_str)
    }

    /// `sha256:…` over the binding — what the session cache keys on, beside
    /// the artifact digest, the runtime and the device.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

/// One inference engine.
pub trait ModelRuntime: Send + Sync {
    /// The name a config and a row use for it — one of [`NAMES`].
    fn name(&self) -> &'static str;
    /// The devices *this build* of the runtime offers: a subset of what
    /// [`devices_of`] lists for its name, always including `cpu`. The table
    /// is what a config may name; this is what a load can use.
    fn devices(&self) -> &'static [&'static str];
    /// The artifact formats it loads — what [`formats_of`] lists for its
    /// name. Never empty: a runtime that serves no format cannot be
    /// selected for anything, and [`ModelRuntimes::register`] refuses it.
    fn formats(&self) -> &'static [&'static str];
    /// Parse `bytes` and make the graph runnable on `device`, bound as
    /// `binding` declares. Blocks for the length of a parse — a caller on a
    /// request path runs it on the blocking pool.
    ///
    /// It takes the binding rather than the whole manifest deliberately:
    /// the session this returns is a function of exactly these three
    /// arguments, and the loaded-session cache keys on exactly them, so no
    /// runtime can come to depend on something the key does not carry.
    fn load(
        &self,
        bytes: &[u8],
        binding: &LoadBinding,
        device: &str,
    ) -> Result<Arc<dyn LoadedModel>, LoadError>;
}

/// A model resident in a runtime, ready to run.
pub trait LoadedModel: Send + Sync {
    /// The digest of the bytes it was loaded from.
    fn digest(&self) -> &str;
    /// What it costs to keep resident, counted against
    /// `models.max_loaded_bytes`.
    fn resident_bytes(&self) -> usize;
    /// One inference: inputs in manifest order, outputs in manifest order.
    fn run(&self, inputs: Vec<OwnedDataTensor>) -> Result<Vec<OwnedDataTensor>, RunError>;
}

/// The runtimes a node offers, by name.
#[derive(Default)]
pub struct ModelRuntimes {
    by_name: BTreeMap<&'static str, Arc<dyn ModelRuntime>>,
}

impl ModelRuntimes {
    /// No runtimes — what a node with models disabled carries, and the
    /// starting point every registration builds on.
    pub fn empty() -> Self {
        Self::default()
    }

    /// The runtimes this build ships, as `config.runtimes` enables them:
    /// [`TractRuntime`] when its entry is present and enabled. A disabled
    /// entry registers nothing, so an admission naming it fails at `probe`
    /// with the runtime's name rather than loading on a runtime the
    /// operator switched off.
    pub fn builtin(config: &ModelsConfig) -> Self {
        let mut registry = Self::empty();
        if config
            .runtimes
            .get(self::tract::NAME)
            .is_some_and(|runtime| runtime.enabled)
        {
            // Inserted directly rather than through `register`: the name is
            // the table's own constant, and the registry is empty.
            registry
                .by_name
                .insert(self::tract::NAME, Arc::new(TractRuntime));
        }
        registry
    }

    /// Add a runtime. Refused when its name is not in [`NAMES`] — config
    /// validation could never have accepted a row naming it — when it
    /// serves no format, or when a runtime of that name is already
    /// registered.
    pub fn register(&mut self, runtime: Arc<dyn ModelRuntime>) -> Result<(), String> {
        let name = runtime.name();
        if !NAMES.contains(&name) {
            return Err(format!(
                "runtime '{name}' is not one this build lists ({}); add it to \
                 model::runtimes::NAMES before registering it",
                NAMES.join(", ")
            ));
        }
        if runtime.formats().is_empty() {
            return Err(format!(
                "runtime '{name}' serves no format, so nothing could ever select it"
            ));
        }
        if self.by_name.contains_key(name) {
            return Err(format!("runtime '{name}' is registered twice"));
        }
        self.by_name.insert(name, runtime);
        Ok(())
    }

    /// The runtime registered under `name`, if any.
    pub fn get(&self, name: &str) -> Option<Arc<dyn ModelRuntime>> {
        self.by_name.get(name).cloned()
    }

    /// The runtime `[models.default_runtime]` names for `format`, and the
    /// device `[models.runtimes]` puts it on — what an admission probes on
    /// and a generation loads into when nothing names a runtime explicitly.
    pub fn default_for<'c>(
        &self,
        config: &'c ModelsConfig,
        format: &str,
    ) -> Result<(Arc<dyn ModelRuntime>, &'c str), RuntimeSelection> {
        let Some(name) = config.default_runtime_for(format) else {
            return Err(RuntimeSelection::NoDefault {
                format: format.to_string(),
            });
        };
        self.for_format(config, name, format)
    }

    /// The runtime `name`, checked for `format`, and the device it runs on
    /// here — the lookup behind an explicit choice (a later
    /// `model_infer.runtime` field). Refused, in this order, when the name
    /// is not one this build lists, when the runtime is not registered on
    /// this node, or when it does not serve the format.
    pub fn for_format<'c>(
        &self,
        config: &'c ModelsConfig,
        name: &str,
        format: &str,
    ) -> Result<(Arc<dyn ModelRuntime>, &'c str), RuntimeSelection> {
        if !NAMES.contains(&name) {
            return Err(RuntimeSelection::UnknownRuntime {
                runtime: name.to_string(),
                format: format.to_string(),
            });
        }
        // Registered *and* enabled by the config: `builtin` registers only
        // what the config enables, so the two agree on every node the
        // server builds; a registry assembled by hand still has to answer
        // for a device, which only the config knows.
        let enabled = config
            .runtimes
            .get(name)
            .is_some_and(|runtime| runtime.enabled);
        let (Some(runtime), Some(device)) =
            (self.get(name).filter(|_| enabled), config.device_of(name))
        else {
            return Err(RuntimeSelection::NotEnabled {
                runtime: name.to_string(),
                format: format.to_string(),
            });
        };
        let serves = runtime.formats();
        if !serves.contains(&format) {
            return Err(RuntimeSelection::DoesNotServe {
                runtime: name.to_string(),
                format: format.to_string(),
                serves,
            });
        }
        Ok((runtime, device))
    }

    /// The names registered, in order.
    pub fn names(&self) -> Vec<&'static str> {
        self.by_name.keys().copied().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::fixture;
    use serde_json::json;

    /// The binding is everything a load reads out of a manifest, and
    /// nothing else.
    ///
    /// The first half is the correctness half: every field a runtime binds
    /// a graph with — an input's name, dtype or shape, an output's name,
    /// either list's length or order — moves the fingerprint, so two models
    /// over one artifact that differ in any of them get their own session.
    /// The second half is the sharing half: a load reads none of the rest,
    /// so two manifests differing only there keep one session between them,
    /// which is why registering one artifact under two manifests is cheap
    /// rather than double.
    #[test]
    fn the_binding_covers_what_a_load_reads_and_nothing_more() {
        let base = fixture::manifest();
        let fingerprint = |m: &Manifest| LoadBinding::of(m).fingerprint().to_string();
        let changed = |f: fn(&mut Manifest)| {
            let mut m = base.clone();
            f(&mut m);
            fingerprint(&m)
        };
        let untouched = fingerprint(&base);
        assert!(
            crate::crypto::is_sha256_digest(&untouched),
            "{untouched} is the one digest spelling"
        );
        assert_eq!(untouched, fingerprint(&base.clone()), "and it is stable");

        for (what, moved) in [
            (
                "an input name",
                changed(|m| m.inputs[0].name = "boards".into()),
            ),
            (
                "an input dtype",
                changed(|m| m.inputs[0].dtype = "f64".into()),
            ),
            (
                "an input shape",
                changed(|m| m.inputs[0].shape = crate::model::manifest::fixed_shape(&[1, 2, 6, 8])),
            ),
            (
                "an input axis becoming variable",
                changed(|m| {
                    m.inputs[0].shape[3] = crate::model::manifest::Dim::Named("W".to_string());
                }),
            ),
            (
                "the name a variable axis is given",
                changed(|m| {
                    m.inputs[0].shape[3] = crate::model::manifest::Dim::Named("H".to_string());
                }),
            ),
            (
                "an output name",
                changed(|m| m.outputs[0].name = "logits".into()),
            ),
            (
                "a second input",
                changed(|m| {
                    let mut extra = m.inputs[0].clone();
                    extra.name = "other".into();
                    m.inputs.push(extra);
                }),
            ),
            (
                "a second output",
                changed(|m| {
                    let mut extra = m.outputs[0].clone();
                    extra.name = "other".into();
                    m.outputs.push(extra);
                }),
            ),
        ] {
            assert_ne!(untouched, moved, "{what} is part of the binding");
        }

        // Order, in both lists. The fixture declares one of each, so the
        // input pair is made here; the output pair is the `two-out` fixture,
        // which is two manifests over one graph differing in nothing else.
        let mut two_inputs = base.clone();
        let mut extra = two_inputs.inputs[0].clone();
        extra.name = "other".to_string();
        two_inputs.inputs.push(extra);
        let mut swapped = two_inputs.clone();
        swapped.inputs.swap(0, 1);
        assert_ne!(
            fingerprint(&two_inputs),
            fingerprint(&swapped),
            "input order is part of the binding"
        );
        let (order_a, order_b) = fixture::two_out();
        assert_ne!(
            fingerprint(&order_a),
            fingerprint(&order_b),
            "output order is part of the binding"
        );

        for (what, moved) in [
            ("the model name", changed(|m| m.name = "ada.other".into())),
            ("the version", changed(|m| m.version = "9.9.9".into())),
            (
                "the description",
                changed(|m| m.description = "another graph entirely".into()),
            ),
            (
                "the artifact path",
                changed(|m| m.artifact = Some("other.onnx".into())),
            ),
            (
                "an input adapter",
                changed(|m| m.inputs[0].adapter = Some(json!({"var": "data.other"}))),
            ),
            (
                "the result expression",
                changed(|m| m.result = Some(json!({"other": {"var": "policy"}}))),
            ),
            (
                "an output dtype",
                changed(|m| m.outputs[0].dtype = "f64".into()),
            ),
            (
                "an output shape",
                changed(|m| m.outputs[0].shape = crate::model::manifest::fixed_shape(&[1, 9])),
            ),
        ] {
            assert_eq!(untouched, moved, "{what} is not part of the binding");
        }
    }

    /// A runtime by name, serving the given formats.
    struct Stub(&'static str, &'static [&'static str]);

    impl ModelRuntime for Stub {
        fn name(&self) -> &'static str {
            self.0
        }
        fn devices(&self) -> &'static [&'static str] {
            TRACT_DEVICES
        }
        fn formats(&self) -> &'static [&'static str] {
            self.1
        }
        fn load(
            &self,
            _bytes: &[u8],
            _binding: &LoadBinding,
            _device: &str,
        ) -> Result<Arc<dyn LoadedModel>, LoadError> {
            Err(LoadError::new("parse", "stub"))
        }
    }

    /// The static table and the registry agree by construction: every name
    /// in `NAMES` has a device list and a format list, `KNOWN_FORMATS` is
    /// the union of the latter, and nothing outside the table registers.
    #[test]
    fn every_known_runtime_lists_cpu_and_a_format_and_nothing_else_registers() {
        let mut union: Vec<&str> = Vec::new();
        for name in NAMES {
            let devices = devices_of(name).expect("every known runtime lists its devices");
            assert!(devices.contains(&"cpu"), "{name} must run on cpu");
            let formats = formats_of(name).expect("every known runtime lists its formats");
            assert!(!formats.is_empty(), "{name} must serve a format");
            for format in formats {
                if !union.contains(format) {
                    union.push(format);
                }
            }
            assert_eq!(intern(name), Some(*name));
        }
        union.sort_unstable();
        let mut known = KNOWN_FORMATS.to_vec();
        known.sort_unstable();
        assert_eq!(
            known, union,
            "KNOWN_FORMATS is the union of formats_of over NAMES"
        );
        assert!(devices_of("onnxruntime").is_none());
        assert!(formats_of("onnxruntime").is_none());
        assert!(intern("onnxruntime").is_none());

        let mut registry = ModelRuntimes::empty();
        assert!(registry.is_empty());
        registry
            .register(Arc::new(Stub("tract", TRACT_FORMATS)))
            .expect("a listed name registers");
        let err = registry
            .register(Arc::new(Stub("tract", TRACT_FORMATS)))
            .expect_err("twice");
        assert!(err.contains("registered twice"), "{err}");
        let err = registry
            .register(Arc::new(Stub("onnxruntime", TRACT_FORMATS)))
            .expect_err("unlisted");
        assert!(err.contains("model::runtimes::NAMES"), "{err}");
        let err = ModelRuntimes::empty()
            .register(Arc::new(Stub("tract", &[])))
            .expect_err("no format");
        assert!(err.contains("serves no format"), "{err}");
        assert_eq!(registry.names(), ["tract"]);
        assert!(registry.get("tract").is_some());
        assert!(registry.get("onnxruntime").is_none());
    }

    /// A selection expected to be refused. (`expect_err` needs the `Ok`
    /// type to be `Debug`, and a runtime is not.)
    fn refused(
        result: Result<(Arc<dyn ModelRuntime>, &str), RuntimeSelection>,
    ) -> RuntimeSelection {
        match result {
            Err(selection) => selection,
            Ok((runtime, device)) => {
                unreachable!("expected a refusal, got {} on {device}", runtime.name())
            }
        }
    }

    /// The per-format lookup: the configured default resolves to the
    /// runtime and its device, and each way it cannot is named.
    #[test]
    fn selection_follows_the_format_table_and_names_every_refusal() {
        let config = ModelsConfig::default();
        let registry = ModelRuntimes::builtin(&config);
        let (runtime, device) = registry
            .default_for(&config, "onnx")
            .expect("the default config maps onnx to tract");
        assert_eq!(runtime.name(), "tract");
        assert_eq!(device, "cpu");
        let (runtime, device) = registry
            .for_format(&config, "tract", "onnx")
            .expect("explicit tract for onnx");
        assert_eq!(runtime.name(), "tract");
        assert_eq!(device, "cpu");

        // The device is the config's, per runtime.
        let mut metal = ModelsConfig::default();
        metal.runtimes.get_mut("tract").expect("entry").device = "metal".to_string();
        let (_, device) = registry
            .default_for(&metal, "onnx")
            .expect("device comes from the config");
        assert_eq!(device, "metal");

        // No row for the format.
        let err = refused(registry.default_for(&config, "nnef"));
        assert_eq!(
            err,
            RuntimeSelection::NoDefault {
                format: "nnef".to_string()
            }
        );
        assert_eq!(
            err.to_string(),
            "no default runtime for format 'nnef' (models.default_runtime.nnef is not set)"
        );

        // A row naming a runtime this build does not have.
        let mut ort = ModelsConfig::default();
        ort.default_runtime
            .insert("onnx".to_string(), "ort".to_string());
        let err = refused(registry.default_for(&ort, "onnx"));
        assert_eq!(
            err,
            RuntimeSelection::UnknownRuntime {
                runtime: "ort".to_string(),
                format: "onnx".to_string()
            }
        );
        assert_eq!(
            err.to_string(),
            "runtime 'ort' for format 'onnx' is unknown: this build lists tract"
        );

        // Disabled in the config (so `builtin` did not register it), and
        // enabled in the config but absent from the registry: both are
        // "not enabled on this node".
        let mut disabled = ModelsConfig::default();
        disabled.runtimes.get_mut("tract").expect("entry").enabled = false;
        let err = refused(ModelRuntimes::builtin(&disabled).default_for(&disabled, "onnx"));
        assert_eq!(
            err,
            RuntimeSelection::NotEnabled {
                runtime: "tract".to_string(),
                format: "onnx".to_string()
            }
        );
        assert_eq!(
            err.to_string(),
            "runtime 'tract' for format 'onnx' is not enabled on this node \
             (models.runtimes.tract.enabled)"
        );
        let err = refused(ModelRuntimes::empty().default_for(&config, "onnx"));
        assert!(matches!(err, RuntimeSelection::NotEnabled { .. }), "{err}");
        // Registered by hand under a config with no entry for it: the
        // config has no device to offer, so the same answer.
        let mut by_hand = ModelRuntimes::empty();
        by_hand
            .register(Arc::new(Stub("tract", TRACT_FORMATS)))
            .expect("registers");
        let mut absent = ModelsConfig::default();
        absent.runtimes.clear();
        let err = refused(by_hand.for_format(&absent, "tract", "onnx"));
        assert!(matches!(err, RuntimeSelection::NotEnabled { .. }), "{err}");

        // Registered and enabled, but the runtime does not load the format.
        let err = refused(registry.for_format(&config, "tract", "nnef"));
        assert_eq!(
            err,
            RuntimeSelection::DoesNotServe {
                runtime: "tract".to_string(),
                format: "nnef".to_string(),
                serves: TRACT_FORMATS
            }
        );
        assert_eq!(
            err.to_string(),
            "runtime 'tract' does not serve format 'nnef'; it serves 'onnx'"
        );
    }

    /// The config's `runtimes` table decides what a node carries: the
    /// default enables tract; a disabled entry, or none, carries nothing.
    #[test]
    fn builtin_follows_the_config() {
        let config = ModelsConfig::default();
        let registry = ModelRuntimes::builtin(&config);
        assert_eq!(registry.names(), ["tract"]);
        let tract = registry.get("tract").expect("registered");
        assert_eq!(tract.name(), "tract");
        assert!(tract.devices().contains(&"cpu"));
        assert_eq!(tract.formats(), formats_of("tract").expect("listed"));

        let mut disabled = ModelsConfig::default();
        disabled
            .runtimes
            .get_mut("tract")
            .expect("default entry")
            .enabled = false;
        assert!(ModelRuntimes::builtin(&disabled).is_empty());

        let mut absent = ModelsConfig::default();
        absent.runtimes.clear();
        assert!(ModelRuntimes::builtin(&absent).is_empty());
    }

    #[test]
    fn errors_name_their_stage() {
        assert_eq!(
            LoadError::new("device", "no metal here").to_string(),
            "device: no metal here"
        );
        assert_eq!(
            RunError::new("input", "rank 3 given, rank 4 expected").to_string(),
            "input: rank 3 given, rank 4 expected"
        );
    }
}
