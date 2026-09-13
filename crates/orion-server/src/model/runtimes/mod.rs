//! The runtime abstraction: what it means to load a model and run it, the
//! names and devices this build knows, and the registry a generation reads.
//!
//! A runtime is a *mechanism* — tract today, another engine later — behind
//! two traits: [`ModelRuntime`] turns bytes plus a manifest into a
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

use dataflow_rs::datavalue::OwnedDataTensor;

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
    /// Parse `bytes` as the manifest's format and make the graph runnable on
    /// `device`. Blocks for the length of a parse — a caller on a request
    /// path runs it on the blocking pool.
    fn load(
        &self,
        bytes: &[u8],
        manifest: &Manifest,
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
            _manifest: &Manifest,
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
