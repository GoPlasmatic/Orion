//! `[models]`: the ONNX model runtime, its artifact cache, and the ceilings an
//! operator sets on it.
//!
//! A model's bytes never travel through the admin API: a stored model row
//! names a storage connector, an object key and a digest, and the node that
//! admits it fetches the object, verifies the digest, and keeps the file in
//! `cache_dir`. Every limit below is the host's, and a per-model override may
//! only reduce one — the same rule `[plugins]` follows, for the same reason:
//! an author who needs more asks the operator. Off by default: `enabled =
//! false` loads no runtime, admits nothing, and leaves a stored model row as
//! a load issue that quarantines the workflows naming it rather than an
//! abort.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{Deserializer, MapAccess, Visitor};
use serde::{Deserialize, Serialize};

use super::validation::{reduce_only, require_nonzero};
use crate::errors::OrionError;
use crate::model::runtimes::{KNOWN_FORMATS, NAMES, devices_of, formats_of};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelsConfig {
    /// Whether this node loads and runs models at all.
    pub enabled: bool,
    /// The runtime a model runs on when nothing names one, **per artifact
    /// format**: `[models.default_runtime]` is a table of format → runtime
    /// name. Every key must be a format some runtime this build knows
    /// serves, every value a key of `runtimes` that is enabled and serves
    /// that format, and the `onnx` row must be present — it is the only
    /// format a manifest can declare today. A second format or runtime is a
    /// row here, not a schema change.
    #[serde(deserialize_with = "deserialize_default_runtime")]
    pub default_runtime: BTreeMap<String, String>,
    /// Directory the verified artifacts are cached in, one file per digest.
    /// Required when `enabled`: a node that cannot keep the bytes it admitted
    /// would fetch every model again at every generation.
    pub cache_dir: String,
    /// Ceiling on the cache directory. Swept least-recently-used, by file
    /// modification time, after every fetch.
    pub max_cache_bytes: u64,
    /// Ceiling on the bytes of models resident in memory at once, across
    /// every runtime. A load that would cross it fails as a limit rather
    /// than evicting a model another workflow is using.
    pub max_loaded_bytes: u64,
    /// Which admitted models a generation loads before it serves — see
    /// [`ModelPreload`].
    pub preload: ModelPreload,
    /// Model tags to warm in addition to whatever [`Self::preload`] selects.
    ///
    /// `preload` infers what to warm from the workflows, and can only see a
    /// `model` named by literal id. A workflow that routes with a computed
    /// one — `{"var": "data.mover_model"}`, the form the guide recommends
    /// for serving many models from one workflow — therefore warms nothing
    /// under `referenced`, and `all` is the only alternative. This is how
    /// an operator names the set instead: the tags travel on the
    /// registration and through a package, so whoever deploys the models
    /// says which are hot, and the node does not have to infer it.
    ///
    /// Empty by default, and a union rather than a mode: `referenced` plus
    /// tags warms both, and `none` plus tags warms exactly the tagged ones.
    pub preload_tags: Vec<String>,
    /// Largest artifact an admission will fetch. Checked against the
    /// object's declared size before a byte is read, and again while the
    /// body streams.
    pub max_artifact_bytes: usize,
    /// Ceiling on a model's parameter count, as read from the graph at
    /// admission. `0` leaves it unbounded.
    pub max_parameters: u64,
    /// Elements one inference may hand a model, summed over its inputs.
    pub max_input_elements: usize,
    /// Elements one inference may take back, summed over its outputs.
    pub max_output_elements: usize,
    /// Wall-clock ceiling per inference. The task's own deadline applies too;
    /// the shorter wins.
    pub max_timeout_ms: u64,
    /// Ceiling on the admission probe — the median of five inferences over
    /// zero-filled inputs that prove the graph runs on this node. A model
    /// slower than this at rest is refused, because it could never meet
    /// `max_timeout_ms` under load.
    pub max_probe_ms: u64,
    /// How long one artifact fetch may take, connection to last byte.
    pub fetch_timeout_secs: u64,
    /// How long one admission may take end to end: signature, fetch, parse,
    /// probe. A row still pending after this is marked failed with the stage
    /// it was in.
    pub admission_timeout_secs: u64,
    /// Inferences of one model that may run at once. Beyond it a task waits
    /// for a permit until its deadline and then fails as a limit.
    pub max_concurrency_per_model: u32,
    /// Inferences that may run at once across every model. `0` means the
    /// host's available parallelism.
    pub max_concurrent_inferences: u32,
    pub trust: ModelTrustConfig,
    /// The runtimes this node offers, by name. The names a build knows are
    /// `model::runtimes::NAMES`; each entry's `device` must be one that
    /// runtime lists.
    pub runtimes: BTreeMap<String, ModelRuntimeConfig>,
    /// Per-model ceilings, each at most the host's.
    pub overrides: Vec<ModelOverride>,
}

/// Which admitted models a generation loads before it serves.
///
/// A load is a parse plus a runtime allocation — seconds and hundreds of
/// megabytes for a large model — so what is loaded eagerly is a choice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelPreload {
    /// Load nothing until the first inference asks. The first call pays the
    /// load.
    None,
    /// Load every model an active workflow references. The default: what
    /// serves is warm, what is merely stored is not.
    #[default]
    Referenced,
    /// Load every admitted model, referenced or not.
    All,
}

impl ModelPreload {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Referenced => "referenced",
            Self::All => "all",
        }
    }
}

/// Optional hardening: when `public_keys` is non-empty, a model row must
/// carry a signature over its artifact digest by one of them, verified by
/// the node that admits it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelTrustConfig {
    pub public_keys: Vec<String>,
}

/// One runtime's configuration on this node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelRuntimeConfig {
    /// Whether models may run on this runtime here. A disabled runtime keeps
    /// its entry so a row naming it is refused with the runtime's name
    /// rather than "unknown runtime".
    pub enabled: bool,
    /// The device the runtime executes on — one of the names the runtime
    /// lists (`cpu` everywhere; `metal` and `cuda` where the build has them).
    pub device: String,
}

impl Default for ModelRuntimeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            device: "cpu".to_string(),
        }
    }
}

/// A ceiling lowered for one model. Any field left unset keeps the host's.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelOverride {
    pub id: String,
    pub timeout_ms: Option<u64>,
    pub max_concurrency: Option<u32>,
    pub max_input_elements: Option<usize>,
    pub max_output_elements: Option<usize>,
}

/// The runtime a fresh config names for `onnx`, and the one entry
/// `runtimes` holds by default.
const DEFAULT_RUNTIME: &str = "tract";

/// The one format a manifest can declare today, and so the row
/// `default_runtime` must always carry.
const REQUIRED_FORMAT: &str = "onnx";

/// `default_runtime` as a table, with the refusal of the pre-table spelling
/// (`default_runtime = "tract"`) naming the shape to write instead. Serde's
/// own message would say "expected a map", which does not tell an operator
/// what the map is keyed by.
fn deserialize_default_runtime<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    struct Table;

    impl<'de> Visitor<'de> for Table {
        type Value = BTreeMap<String, String>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                f,
                "a table keyed by model format — write it as [models.default_runtime] with \
                 {REQUIRED_FORMAT} = \"{DEFAULT_RUNTIME}\", not as a single runtime name"
            )
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut out = BTreeMap::new();
            while let Some((format, runtime)) = map.next_entry::<String, String>()? {
                out.insert(format, runtime);
            }
            Ok(out)
        }
    }

    deserializer.deserialize_map(Table)
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_runtime: BTreeMap::from([(
                REQUIRED_FORMAT.to_string(),
                DEFAULT_RUNTIME.to_string(),
            )]),
            cache_dir: String::new(),
            max_cache_bytes: 8 * 1024 * 1024 * 1024,
            max_loaded_bytes: 2 * 1024 * 1024 * 1024,
            preload: ModelPreload::default(),
            preload_tags: Vec::new(),
            max_artifact_bytes: 512 * 1024 * 1024,
            max_parameters: 0,
            max_input_elements: 1_048_576,
            max_output_elements: 1_048_576,
            max_timeout_ms: 1_000,
            max_probe_ms: 250,
            fetch_timeout_secs: 300,
            admission_timeout_secs: 900,
            max_concurrency_per_model: 16,
            max_concurrent_inferences: 0,
            trust: ModelTrustConfig::default(),
            runtimes: BTreeMap::from([(
                DEFAULT_RUNTIME.to_string(),
                ModelRuntimeConfig::default(),
            )]),
            overrides: Vec::new(),
        }
    }
}

impl ModelsConfig {
    pub(crate) fn validate(&self) -> Result<(), OrionError> {
        if self.enabled && self.cache_dir.trim().is_empty() {
            return Err(OrionError::Config {
                message: "models.cache_dir is required when models.enabled = true: the node \
                          keeps every verified artifact there, one file per digest"
                    .to_string(),
            });
        }
        require_nonzero(self.max_cache_bytes, "models.max_cache_bytes")?;
        require_nonzero(self.max_loaded_bytes, "models.max_loaded_bytes")?;
        require_nonzero(self.max_artifact_bytes as u64, "models.max_artifact_bytes")?;
        require_nonzero(self.max_input_elements as u64, "models.max_input_elements")?;
        require_nonzero(
            self.max_output_elements as u64,
            "models.max_output_elements",
        )?;
        require_nonzero(self.max_timeout_ms, "models.max_timeout_ms")?;
        require_nonzero(self.max_probe_ms, "models.max_probe_ms")?;
        require_nonzero(self.fetch_timeout_secs, "models.fetch_timeout_secs")?;
        require_nonzero(self.admission_timeout_secs, "models.admission_timeout_secs")?;
        require_nonzero(
            u64::from(self.max_concurrency_per_model),
            "models.max_concurrency_per_model",
        )?;
        // `max_parameters` and `max_concurrent_inferences` read `0` as
        // "unbounded" and "available parallelism" respectively, so neither
        // is checked here.

        // Every runtime named must be one this build knows, on a device that
        // runtime lists. Checked whether or not models are enabled: a config
        // is either right or wrong, and the flag is the one thing an operator
        // flips last.
        for (name, runtime) in &self.runtimes {
            let at = format!("models.runtimes.{name}");
            let Some(devices) = devices_of(name) else {
                return Err(OrionError::Config {
                    message: format!(
                        "{at} names a runtime this build does not have; known runtimes: {}",
                        NAMES.join(", ")
                    ),
                });
            };
            if !devices.contains(&runtime.device.as_str()) {
                return Err(OrionError::Config {
                    message: format!(
                        "{at}.device '{}' is not a device the {name} runtime lists: {}",
                        runtime.device,
                        devices.join(", ")
                    ),
                });
            }
        }
        // The default runtime per format: every key a format some runtime
        // serves, every value a runtime that is a key of `runtimes`, enabled
        // there, and serves that format — and the one format a manifest can
        // declare today must have a row, or every admission would fail at
        // the probe for a reason that belongs to the config.
        if !self.default_runtime.contains_key(REQUIRED_FORMAT) {
            return Err(OrionError::Config {
                message: format!(
                    "models.default_runtime has no '{REQUIRED_FORMAT}' row, and {REQUIRED_FORMAT} \
                     is the one format a manifest can declare today; write \
                     [models.default_runtime] with {REQUIRED_FORMAT} = \"{DEFAULT_RUNTIME}\""
                ),
            });
        }
        for (format, runtime) in &self.default_runtime {
            let at = format!("models.default_runtime.{format}");
            if !KNOWN_FORMATS.contains(&format.as_str()) {
                return Err(OrionError::Config {
                    message: format!(
                        "{at}: '{format}' is not a format any runtime this build serves; known \
                         formats: {}",
                        KNOWN_FORMATS.join(", ")
                    ),
                });
            }
            match self.runtimes.get(runtime) {
                None => {
                    return Err(OrionError::Config {
                        message: format!(
                            "{at} names runtime '{runtime}', which is not a key of \
                             models.runtimes ({})",
                            if self.runtimes.is_empty() {
                                "which is empty".to_string()
                            } else {
                                self.runtimes.keys().cloned().collect::<Vec<_>>().join(", ")
                            }
                        ),
                    });
                }
                Some(entry) if !entry.enabled => {
                    return Err(OrionError::Config {
                        message: format!(
                            "{at} names runtime '{runtime}', which is disabled \
                             (models.runtimes.{runtime}.enabled = false)"
                        ),
                    });
                }
                Some(_) => {}
            }
            // `runtimes` was checked above, so the name is known here.
            let serves = formats_of(runtime).unwrap_or_default();
            if !serves.contains(&format.as_str()) {
                return Err(OrionError::Config {
                    message: format!(
                        "{at} names runtime '{runtime}', which does not serve '{format}'; it \
                         serves: {}",
                        serves.join(", ")
                    ),
                });
            }
        }
        // A key that does not decode would make every admission fail to
        // verify with a message about the signature, when the mistake is in
        // the config; refused here, where it can be named.
        for (i, key) in self.trust.public_keys.iter().enumerate() {
            if let Err(reason) = crate::crypto::ed25519::parse_public_key(key) {
                return Err(OrionError::Config {
                    message: format!(
                        "models.trust.public_keys[{i}] is not an Ed25519 public key: {reason}"
                    ),
                });
            }
        }
        let mut seen: Vec<&str> = Vec::new();
        for (i, o) in self.overrides.iter().enumerate() {
            let at = format!("models.overrides[{i}]");
            if o.id.trim().is_empty() {
                return Err(OrionError::Config {
                    message: format!("{at}.id must name a model"),
                });
            }
            if seen.contains(&o.id.as_str()) {
                return Err(OrionError::Config {
                    message: format!("{at}.id '{}' appears twice", o.id),
                });
            }
            seen.push(&o.id);
            reduce_only(&at, "timeout_ms", o.timeout_ms, self.max_timeout_ms)?;
            reduce_only(
                &at,
                "max_concurrency",
                o.max_concurrency.map(u64::from),
                u64::from(self.max_concurrency_per_model),
            )?;
            reduce_only(
                &at,
                "max_input_elements",
                o.max_input_elements.map(|v| v as u64),
                self.max_input_elements as u64,
            )?;
            reduce_only(
                &at,
                "max_output_elements",
                o.max_output_elements.map(|v| v as u64),
                self.max_output_elements as u64,
            )?;
        }
        Ok(())
    }

    /// The override for `model_id`, if the operator wrote one.
    pub fn override_for(&self, model_id: &str) -> Option<&ModelOverride> {
        self.overrides.iter().find(|o| o.id == model_id)
    }

    /// The device `runtime` executes on here (`models.runtimes.<runtime>.device`),
    /// or `None` for a runtime the config has no entry for.
    pub fn device_of(&self, runtime: &str) -> Option<&str> {
        self.runtimes.get(runtime).map(|r| r.device.as_str())
    }

    /// The runtime `[models.default_runtime]` names for `format`, or `None`
    /// when the table has no row for it.
    pub fn default_runtime_for(&self, format: &str) -> Option<&str> {
        self.default_runtime.get(format).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> ModelsConfig {
        ModelsConfig {
            enabled: true,
            cache_dir: "/var/cache/orion/models".to_string(),
            ..ModelsConfig::default()
        }
    }

    #[test]
    fn the_defaults_validate_and_are_the_documented_numbers() {
        let c = ModelsConfig::default();
        c.validate().expect("defaults are valid");
        assert!(!c.enabled);
        assert_eq!(
            c.default_runtime,
            BTreeMap::from([("onnx".to_string(), "tract".to_string())])
        );
        assert_eq!(c.default_runtime_for("onnx"), Some("tract"));
        assert_eq!(c.default_runtime_for("nnef"), None);
        assert_eq!(c.cache_dir, "");
        assert_eq!(c.max_cache_bytes, 8 << 30);
        assert_eq!(c.max_loaded_bytes, 2 << 30);
        assert_eq!(c.preload, ModelPreload::Referenced);
        assert_eq!(c.max_artifact_bytes, 512 << 20);
        assert_eq!(c.max_parameters, 0);
        assert_eq!(c.max_input_elements, 1 << 20);
        assert_eq!(c.max_output_elements, 1 << 20);
        assert_eq!(c.max_timeout_ms, 1_000);
        assert_eq!(c.max_probe_ms, 250);
        assert_eq!(c.fetch_timeout_secs, 300);
        assert_eq!(c.admission_timeout_secs, 900);
        assert_eq!(c.max_concurrency_per_model, 16);
        assert_eq!(c.max_concurrent_inferences, 0);
        assert!(c.trust.public_keys.is_empty());
        assert_eq!(
            c.runtimes.get("tract"),
            Some(&ModelRuntimeConfig {
                enabled: true,
                device: "cpu".to_string()
            })
        );
        assert_eq!(c.device_of("tract"), Some("cpu"));
        assert_eq!(c.device_of("onnxruntime"), None);
        assert!(c.overrides.is_empty());
    }

    #[test]
    fn the_preload_wire_form_is_snake_case() {
        let c: ModelsConfig = toml::from_str("preload = \"all\"").expect("parses");
        assert_eq!(c.preload, ModelPreload::All);
        assert_eq!(
            toml::to_string(&ModelsConfig::default())
                .expect("serialises")
                .lines()
                .find(|l| l.starts_with("preload"))
                .expect("preload line"),
            "preload = \"referenced\""
        );
        assert!(toml::from_str::<ModelsConfig>("preload = \"Referenced\"").is_err());
    }

    #[test]
    fn enabling_models_requires_a_cache_dir() {
        let c = ModelsConfig {
            enabled: true,
            ..ModelsConfig::default()
        };
        let err = c.validate().expect_err("no cache dir");
        assert!(err.to_string().contains("models.cache_dir"), "{err}");
        enabled().validate().expect("a cache dir makes it valid");
        // Disabled, an empty cache_dir is the default and fine.
        ModelsConfig::default().validate().expect("disabled");
    }

    /// Sets one ceiling to zero.
    type Zero = Box<dyn Fn(&mut ModelsConfig)>;

    #[test]
    fn every_hard_ceiling_refuses_zero_and_the_two_sentinels_accept_it() {
        let cases: Vec<(&str, Zero)> = vec![
            (
                "models.max_cache_bytes",
                Box::new(|c| c.max_cache_bytes = 0),
            ),
            (
                "models.max_loaded_bytes",
                Box::new(|c| c.max_loaded_bytes = 0),
            ),
            (
                "models.max_artifact_bytes",
                Box::new(|c| c.max_artifact_bytes = 0),
            ),
            (
                "models.max_input_elements",
                Box::new(|c| c.max_input_elements = 0),
            ),
            (
                "models.max_output_elements",
                Box::new(|c| c.max_output_elements = 0),
            ),
            ("models.max_timeout_ms", Box::new(|c| c.max_timeout_ms = 0)),
            ("models.max_probe_ms", Box::new(|c| c.max_probe_ms = 0)),
            (
                "models.fetch_timeout_secs",
                Box::new(|c| c.fetch_timeout_secs = 0),
            ),
            (
                "models.admission_timeout_secs",
                Box::new(|c| c.admission_timeout_secs = 0),
            ),
            (
                "models.max_concurrency_per_model",
                Box::new(|c| c.max_concurrency_per_model = 0),
            ),
        ];
        for (field, zero) in cases {
            let mut c = enabled();
            zero(&mut c);
            let err = c.validate().expect_err(field);
            assert!(err.to_string().contains(field), "{field}: {err}");
        }
        let c = ModelsConfig {
            max_parameters: 0,
            max_concurrent_inferences: 0,
            ..enabled()
        };
        c.validate()
            .expect("0 is unbounded / available parallelism");
    }

    /// `[models.default_runtime]` is the TOML table form; the pre-table
    /// spelling — one runtime name for every format — is refused with the
    /// table named, not with serde's "expected a map".
    #[test]
    fn the_default_runtime_is_a_table_keyed_by_format() {
        let c: ModelsConfig =
            toml::from_str("[default_runtime]\nonnx = \"tract\"").expect("the table form parses");
        assert_eq!(c.default_runtime_for("onnx"), Some("tract"));
        c.validate().expect("and validates");

        // A second row is a parse-time nothing: validation is what knows
        // which formats exist.
        let c: ModelsConfig =
            toml::from_str("[default_runtime]\nonnx = \"tract\"\nnnef = \"tract\"")
                .expect("any keys parse");
        assert_eq!(c.default_runtime.len(), 2);

        assert!(
            toml::from_str::<ModelsConfig>("[default_runtime]\nonnx = 1").is_err(),
            "a row's value is a runtime name"
        );
        let err = toml::from_str::<ModelsConfig>("default_runtime = \"tract\"")
            .expect_err("a bare string is the old shape");
        let text = err.to_string();
        assert!(text.contains("[models.default_runtime]"), "{text}");
        assert!(text.contains("onnx = \"tract\""), "{text}");
        assert!(text.contains("string"), "{text}");

        // The table survives a round trip through the serialised form
        // `validate-config` prints.
        let printed = toml::to_string(&ModelsConfig::default()).expect("serialises");
        assert!(printed.contains("[default_runtime]"), "{printed}");
        assert!(printed.contains("onnx = \"tract\""), "{printed}");
        let back: ModelsConfig = toml::from_str(&printed).expect("parses back");
        assert_eq!(
            back.default_runtime,
            ModelsConfig::default().default_runtime
        );
    }

    /// Every row of the table is checked: the format must be one a known
    /// runtime serves, the runtime a key of `runtimes`, enabled there, and
    /// serving that format — and the `onnx` row must exist.
    #[test]
    fn every_default_runtime_row_names_a_format_and_a_runtime_that_serves_it() {
        let mut c = enabled();
        c.default_runtime
            .insert("nnef".to_string(), "tract".to_string());
        let err = c.validate().expect_err("unknown format");
        let text = err.to_string();
        assert!(text.contains("models.default_runtime.nnef"), "{text}");
        assert!(
            text.contains("'nnef' is not a format any runtime this build serves"),
            "{text}"
        );
        assert!(text.contains("known formats: onnx"), "{text}");

        let mut c = enabled();
        c.default_runtime
            .insert("onnx".to_string(), "onnxruntime".to_string());
        let err = c.validate().expect_err("not a key");
        let text = err.to_string();
        assert!(
            text.contains("models.default_runtime.onnx names runtime 'onnxruntime'"),
            "{text}"
        );
        assert!(
            text.contains("not a key of models.runtimes (tract)"),
            "{text}"
        );

        let mut c = enabled();
        c.runtimes.get_mut("tract").expect("default entry").enabled = false;
        let err = c.validate().expect_err("disabled");
        let text = err.to_string();
        assert!(
            text.contains("models.default_runtime.onnx names runtime 'tract', which is disabled"),
            "{text}"
        );
        assert!(
            text.contains("models.runtimes.tract.enabled = false"),
            "{text}"
        );

        let mut c = enabled();
        c.runtimes.clear();
        let err = c.validate().expect_err("empty map");
        assert!(err.to_string().contains("which is empty"), "{err}");

        let mut c = enabled();
        c.default_runtime.clear();
        let err = c.validate().expect_err("no onnx row");
        let text = err.to_string();
        assert!(
            text.contains("models.default_runtime has no 'onnx' row"),
            "{text}"
        );
        assert!(text.contains("[models.default_runtime]"), "{text}");
        // Not enabling models does not excuse the table: a config is either
        // right or wrong, as with `runtimes`.
        c.enabled = false;
        assert!(c.validate().is_err(), "checked while disabled too");
    }

    /// The "does not serve" refusal cannot be reached with one runtime that
    /// serves one format — every known format is served by every known
    /// runtime — so it is asserted on the static table itself: the check
    /// fires the moment a runtime and a format exist that do not meet.
    #[test]
    fn a_runtime_that_does_not_serve_the_format_would_be_refused() {
        for name in crate::model::runtimes::NAMES {
            let serves = formats_of(name).expect("listed");
            for format in KNOWN_FORMATS {
                let mut c = enabled();
                c.default_runtime
                    .insert((*format).to_string(), (*name).to_string());
                let result = c.validate();
                if serves.contains(format) {
                    result.expect("served");
                } else {
                    let text = result.expect_err("not served").to_string();
                    assert!(text.contains("does not serve"), "{text}");
                }
            }
        }
    }

    #[test]
    fn a_runtime_this_build_lacks_and_a_device_it_lacks_are_refused() {
        let mut c = enabled();
        c.runtimes
            .insert("onnxruntime".to_string(), ModelRuntimeConfig::default());
        let err = c.validate().expect_err("unknown runtime");
        let text = err.to_string();
        assert!(text.contains("models.runtimes.onnxruntime"), "{text}");
        assert!(text.contains("known runtimes: tract"), "{text}");

        let mut c = enabled();
        c.runtimes.get_mut("tract").expect("default entry").device = "tpu".to_string();
        let err = c.validate().expect_err("unknown device");
        let text = err.to_string();
        assert!(
            text.contains("models.runtimes.tract.device 'tpu'"),
            "{text}"
        );
        assert!(text.contains("cpu"), "{text}");

        for device in ["cpu", "metal", "cuda"] {
            let mut c = enabled();
            c.runtimes.get_mut("tract").expect("default entry").device = device.to_string();
            c.validate().expect(device);
        }
    }

    #[test]
    fn a_trust_key_that_does_not_decode_is_refused_by_index() {
        let mut c = enabled();
        c.trust.public_keys = vec![
            crate::crypto::ed25519::SigningKey::generate().public_key_base64(),
            "not a key".to_string(),
        ];
        let err = c.validate().expect_err("bad key");
        assert!(
            err.to_string().contains("models.trust.public_keys[1]"),
            "{err}"
        );
        c.trust.public_keys.pop();
        c.validate().expect("one good key");
    }

    #[test]
    fn an_override_may_only_reduce() {
        let mut c = enabled();
        c.overrides.push(ModelOverride {
            id: "ada.c4-tiny".to_string(),
            timeout_ms: Some(100),
            max_concurrency: Some(2),
            max_input_elements: Some(1024),
            max_output_elements: Some(64),
        });
        c.validate().expect("lower everywhere is fine");
        assert!(c.override_for("ada.c4-tiny").is_some());
        assert!(c.override_for("other").is_none());

        for (field, raise) in [
            (
                "timeout_ms",
                Box::new(|o: &mut ModelOverride| o.timeout_ms = Some(2_000))
                    as Box<dyn Fn(&mut ModelOverride)>,
            ),
            (
                "max_concurrency",
                Box::new(|o: &mut ModelOverride| o.max_concurrency = Some(17)),
            ),
            (
                "max_input_elements",
                Box::new(|o: &mut ModelOverride| o.max_input_elements = Some(1 << 21)),
            ),
            (
                "max_output_elements",
                Box::new(|o: &mut ModelOverride| o.max_output_elements = Some(1 << 21)),
            ),
        ] {
            let mut raised = c.clone();
            raise(&mut raised.overrides[0]);
            let err = raised.validate().expect_err(field);
            let text = err.to_string();
            assert!(text.contains(field), "{field}: {text}");
            assert!(text.contains("exceeds the host ceiling"), "{field}: {text}");
        }

        c.overrides[0].timeout_ms = Some(0);
        let err = c.validate().expect_err("zero");
        assert!(err.to_string().contains("must be non-zero"), "{err}");
        c.overrides[0].timeout_ms = None;

        c.overrides.push(ModelOverride {
            id: "ada.c4-tiny".to_string(),
            ..Default::default()
        });
        let err = c.validate().expect_err("dup");
        assert!(err.to_string().contains("appears twice"), "{err}");
        c.overrides[1].id = " ".to_string();
        let err = c.validate().expect_err("blank id");
        assert!(err.to_string().contains("must name a model"), "{err}");
    }

    #[test]
    fn an_unknown_key_is_refused_at_every_level() {
        for doc in [
            "cache_di = \"x\"",
            "[trust]\nkeys = []",
            "[runtimes.tract]\ndevce = \"cpu\"",
            "[[overrides]]\nid = \"m\"\nmemory = 1",
        ] {
            assert!(
                toml::from_str::<ModelsConfig>(doc).is_err(),
                "{doc} must not parse"
            );
        }
    }
}
