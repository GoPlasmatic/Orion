//! From active model rows to what a generation carries: one entry per
//! model that can serve on this node, with its adapters and result
//! expression compiled on the process's expression engine, and the reasons
//! anything did not load.
//!
//! A load never aborts. A row a node cannot serve — models disabled here,
//! an admission that has not passed, a stored manifest that no longer
//! parses, an adapter the serving engine refuses — becomes a
//! [`ModelLoadIssue`], and every workflow naming that model by its literal
//! id is quarantined with the reason attached (`runtime::models`), which is
//! the shape a plugin that fails to load already takes. The node keeps
//! serving everything else.
//!
//! The adapters are compiled **here and not at upload**, on the one
//! [`ExprEngine`] the process evaluates with (I188), which `model_infer`
//! evaluates them on. That engine outlives every generation, so
//! a reload carries a model whose manifest has not changed with its compiled
//! adapters ([`ModelSet::reload`]), and compiles only what is new. Nothing is
//! loaded into a runtime at this point; that is the [`LoadedCache`]'s, on
//! first use or at preload.
//!
//! The same compile serves an **offline** set ([`ModelSet::from_manifests`]):
//! `dry-run` and `orion-server test` build one from the manifests a
//! `--model-dir` holds, on the engine the offline run gives it, with no row
//! and no admission behind any entry — the bytes on disk are what the author
//! is testing, and are trusted as such.

use std::collections::HashMap;
use std::sync::Arc;

use dataflow_rs::Expr;
use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::ExprEngine;
use serde_json::Value;
use tokio::sync::Semaphore;

use super::admission::Stats;
use super::artifact::ArtifactRef;
use super::cache::LoadedCache;
use super::limits::Limits;
use super::manifest::Manifest;
use super::runtimes::LoadBinding;
use crate::config::ModelsConfig;
use crate::storage::models::{Model, ModelHealth};

/// The task function that runs a model, and the input field that names it.
/// The dependants walk and the quarantine read the authored task, so they
/// need only the spelling; the schema lives beside the handler.
pub const INFER_FUNCTION: &str = "model_infer";
pub const INFER_MODEL_FIELD: &str = "model";

/// One model version this generation can run.
pub struct ModelEntry {
    pub id: String,
    pub version: i64,
    pub digest: String,
    pub manifest: Manifest,
    /// What a load of this model's artifact is given — and, with the
    /// digest, the runtime and the device, what its session is keyed by.
    pub binding: Arc<LoadBinding>,
    pub artifact: ArtifactRef,
    /// What admission read out of the artifact, when the row carries it.
    pub stats: Option<Stats>,
    /// The row's tags. Empty for an offline set, which has no row — they
    /// are a property of the registration, not of the manifest.
    pub tags: Vec<String>,
    /// The manifest's expressions, compiled on the set's engine: shared with
    /// the previous generation's entry when the manifest is unchanged.
    pub adapters: Arc<Adapters>,
    pub limits: Limits,
    /// `limits.max_concurrency` inference slots for this model on this node.
    pub permits: Arc<Semaphore>,
}

/// A manifest's expressions, compiled once on the process's expression
/// engine (I188), and evaluated there by the call.
pub struct Adapters {
    /// One compiled adapter per manifest input, in manifest order.
    pub inputs: Vec<(String, Expr<OwnedDataValue>)>,
    /// The compiled result expression, over `{output name: tensor}`.
    pub result: Expr<OwnedDataValue>,
}

/// Why a model version is not serving on this node.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ModelLoadIssue {
    pub model: String,
    pub version: i64,
    pub digest: String,
    /// `disabled`, `admission`, `manifest`, `artifact`, `adapter`.
    pub stage: &'static str,
    pub reason: String,
}

/// The model half of a generation: what loaded, what did not, and the
/// expression engine every entry's adapters were compiled on — the one a
/// call evaluates them on, so the two are always a pair.
///
/// Deliberately without an identity of its own, unlike [`PluginSet`], whose
/// fingerprint decides whether a reload rebuilds the engine. A model set
/// needs no such shortcut: its adapters are compiled on the process's
/// [`ExprEngine`], which no reload replaces (I188), so [`Self::reload`]
/// carries each model whose manifest is unchanged with the adapters it
/// already has and compiles only the rest. A set is cheap to rebuild around
/// them; it is the compile that is worth keeping.
///
/// [`PluginSet`]: crate::plugin::PluginSet
#[derive(Default)]
pub struct ModelSet {
    models: HashMap<String, Arc<ModelEntry>>,
    pub issues: Vec<ModelLoadIssue>,
    exprs: ExprEngine,
}

impl ModelSet {
    /// The set a node boots on, and the one a node with models disabled
    /// keeps when no model row exists.
    pub fn empty() -> Self {
        Self::default()
    }

    /// A set with nothing in it yet, over `exprs`.
    fn on(exprs: &ExprEngine) -> Self {
        Self {
            models: HashMap::new(),
            issues: Vec::new(),
            exprs: exprs.clone(),
        }
    }

    /// Every active row, compiled on `exprs` — the process's expression
    /// engine. `enabled` is whether this node has the model runtime at all;
    /// `false` makes every row a `disabled` issue rather than an abort, so
    /// the estate stays observable and the workflows naming those models are
    /// quarantined with the reason. Never fails.
    pub fn load_active(
        rows: &[Model],
        config: &ModelsConfig,
        enabled: bool,
        exprs: &ExprEngine,
    ) -> Self {
        Self::load_rows(rows, config, enabled, exprs, None)
    }

    /// [`Self::load_active`] for a reload, over the `previous` generation's
    /// set: a model whose manifest is unchanged keeps `previous`'s compiled
    /// adapters, the same `Arc`, and only a new or changed manifest is
    /// compiled (I188). Everything else about an entry — its version, its
    /// digest, its tags, its limits and its permits — is the row's and the
    /// config's, as a fresh load makes it.
    ///
    /// `exprs` is the process's expression engine, the one `previous` was
    /// compiled on: there is one per process, and a reload keeps it.
    pub fn reload(
        previous: &ModelSet,
        rows: &[Model],
        config: &ModelsConfig,
        enabled: bool,
        exprs: &ExprEngine,
    ) -> Self {
        Self::load_rows(rows, config, enabled, exprs, Some(previous))
    }

    fn load_rows(
        rows: &[Model],
        config: &ModelsConfig,
        enabled: bool,
        exprs: &ExprEngine,
        previous: Option<&ModelSet>,
    ) -> Self {
        let mut set = ModelSet::on(exprs);
        let mut carried = 0usize;
        for row in rows {
            let compiled = decode_row(row, enabled).and_then(|item| {
                compile_entry(item, config, exprs, previous).map_err(|reason| ("adapter", reason))
            });
            match compiled {
                Ok((entry, kept)) => {
                    carried += usize::from(kept);
                    // Per model at debug and the count at info: this runs
                    // on every generation build, so an estate of any size
                    // would otherwise put one line per model into the log
                    // every time any workflow, channel or connector
                    // changed. A model that does *not* load still gets its
                    // own line, because that one is about something being
                    // wrong.
                    tracing::debug!(
                        model = %row.model_id,
                        version = row.version,
                        digest = %row.digest,
                        inputs = ?entry.manifest.input_names().collect::<Vec<_>>(),
                        carried = kept,
                        "Model ready"
                    );
                    set.models.insert(row.model_id.clone(), Arc::new(entry));
                }
                Err((stage, reason)) => {
                    tracing::error!(
                        model = %row.model_id,
                        version = row.version,
                        digest = %row.digest,
                        stage,
                        reason = %reason,
                        "Model not available: the workflows naming it are quarantined"
                    );
                    set.issues.push(ModelLoadIssue {
                        model: row.model_id.clone(),
                        version: row.version,
                        digest: row.digest.clone(),
                        stage,
                        reason,
                    });
                }
            }
        }
        if !rows.is_empty() {
            tracing::info!(
                ready = set.models.len(),
                carried,
                unavailable = set.issues.len(),
                "Model set compiled on this generation"
            );
        }
        set
    }

    /// An offline set: one entry per manifest, compiled on `exprs` — the
    /// engine the offline run evaluates them on — with the digest and stats
    /// the caller read off the file beside each manifest. No admission
    /// stands behind an entry: the bytes are the author's own, and what a
    /// serving node would verify at admission (the digest claim, the probe)
    /// is exactly what an offline run exists to try. A manifest whose
    /// adapters do not compile is an issue, as a row's would be, so the run
    /// that names it fails as `unavailable` with the reason rather than
    /// silently skipping the model.
    pub fn from_manifests(
        manifests: impl IntoIterator<Item = ManifestEntry>,
        config: &ModelsConfig,
        exprs: &ExprEngine,
    ) -> Self {
        let mut set = ModelSet::on(exprs);
        for entry in manifests {
            let id = entry.manifest.name.clone();
            let digest = entry.digest.clone();
            let item = CompileItem {
                id: id.clone(),
                version: 0,
                digest: digest.clone(),
                manifest: entry.manifest,
                // Offline the bytes come from the file, not a bucket: the
                // reference names that file so a log line can say where the
                // model was read from, and nothing fetches through it.
                artifact: ArtifactRef {
                    connector: String::new(),
                    key: entry.artifact_path.display().to_string(),
                    digest: digest.clone(),
                    size: None,
                },
                stats: entry.stats,
                // Offline there is no row, and so no registration to have
                // tagged.
                tags: Vec::new(),
            };
            match compile_entry(item, config, exprs, None) {
                Ok((compiled, _)) => {
                    set.models.insert(id, Arc::new(compiled));
                }
                Err(reason) => set.issues.push(ModelLoadIssue {
                    model: id,
                    version: 0,
                    digest,
                    stage: "adapter",
                    reason,
                }),
            }
        }
        set
    }

    /// The expression engine every entry's adapters were compiled on, and
    /// the one a call evaluates them on.
    pub fn exprs(&self) -> &ExprEngine {
        &self.exprs
    }

    /// The entry serving `model_id` — the active version — if this node
    /// loaded it.
    pub fn get(&self, model_id: &str) -> Option<&Arc<ModelEntry>> {
        self.models.get(model_id)
    }

    /// Why `model_id` is not serving here, if an active row exists for it
    /// and did not load.
    pub fn issue_for(&self, model_id: &str) -> Option<&ModelLoadIssue> {
        self.issues.iter().find(|i| i.model == model_id)
    }

    /// The ids this set serves, in no particular order.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.models.keys().map(String::as_str)
    }

    /// Every entry this set serves.
    pub fn entries(&self) -> impl Iterator<Item = &Arc<ModelEntry>> {
        self.models.values()
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    /// This node's account of one version, where the generation has one:
    /// `loaded` (with the runtime, device and resident bytes) while `cache`
    /// holds the version's digest on any runtime, `evicted` when it was
    /// loaded and is no longer, `failed` — or `disabled` — when the set
    /// carries an issue for it. `None` for a version the generation neither
    /// serves nor refused, so the caller keeps the admission-derived state.
    pub fn health_of(
        &self,
        model_id: &str,
        version: i64,
        cache: &LoadedCache,
    ) -> Option<ModelHealth> {
        if let Some(entry) = self.get(model_id).filter(|e| e.version == version) {
            if let Some((key, bytes)) = cache.loaded_for(&entry.digest, entry.binding.fingerprint())
            {
                return Some(ModelHealth {
                    state: "loaded".to_string(),
                    runtime: Some(key.runtime.to_string()),
                    device: Some(key.device),
                    resident_bytes: Some(bytes),
                    reason: None,
                });
            }
            if cache.was_evicted(&entry.digest, entry.binding.fingerprint()) {
                return Some(ModelHealth {
                    state: "evicted".to_string(),
                    ..ModelHealth::default()
                });
            }
            return None;
        }
        self.issues
            .iter()
            .find(|i| i.model == model_id && i.version == version)
            .map(|issue| ModelHealth {
                state: if issue.stage == "disabled" {
                    "disabled".to_string()
                } else {
                    "failed".to_string()
                },
                reason: Some(format!("{}: {}", issue.stage, issue.reason)),
                ..ModelHealth::default()
            })
    }
}

/// A manifest an offline set is built from: the file beside it, hashed and
/// read, by whoever found it on disk (`definitions::ModelDefinition`).
#[derive(Debug, Clone)]
pub struct ManifestEntry {
    pub manifest: Manifest,
    /// The artifact file the manifest's `artifact` names, resolved beside it.
    pub artifact_path: std::path::PathBuf,
    /// `sha256:…` of that file.
    pub digest: String,
    /// What `lint` read out of the graph, when it could — the offline twin
    /// of what admission records, minus the probe.
    pub stats: Option<Stats>,
}

/// What one entry is compiled from, once a row (or a manifest) has been
/// decoded: the part of the load that is the same on a node and offline.
struct CompileItem {
    id: String,
    version: i64,
    digest: String,
    manifest: Manifest,
    artifact: ArtifactRef,
    stats: Option<Stats>,
    tags: Vec<String>,
}

/// The row half of a load: the node has the runtime, the verdict passed,
/// and the stored columns decode. Nothing here touches an engine.
fn decode_row(row: &Model, enabled: bool) -> Result<CompileItem, (&'static str, String)> {
    if !enabled {
        return Err((
            "disabled",
            "models are disabled on this node (models.enabled = false)".to_string(),
        ));
    }
    // The verdict first: a row whose artifact no node has verified — or
    // that one refused — is not run here whatever its manifest says.
    let admission: Value = serde_json::from_str(&row.admission_json)
        .map_err(|e| ("admission", format!("stored admission does not parse: {e}")))?;
    let state = admission
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if state != "passed" {
        let mut reason = format!("admission is '{state}'");
        if let Some(stage) = admission.get("stage").and_then(Value::as_str) {
            reason.push_str(&format!(" at stage '{stage}'"));
        }
        if let Some(why) = admission.get("reason").and_then(Value::as_str) {
            reason.push_str(&format!(": {why}"));
        } else if state == "pending" {
            reason.push_str(": no node has verified the artifact yet");
        }
        return Err(("admission", reason));
    }
    // Decoded, not re-validated, as the admission worker does: the row holds
    // the validated form, and a rule added since it was written is
    // preflight's to report. What *is* checked again is compilation, in
    // `compile_entry`, because the serving engine is the one that has to
    // accept it.
    let manifest: Manifest = serde_json::from_str(&row.manifest_json)
        .map_err(|e| ("manifest", format!("stored manifest does not parse: {e}")))?;
    let artifact: ArtifactRef = serde_json::from_str(&row.artifact_json).map_err(|e| {
        (
            "artifact",
            format!("stored artifact reference does not parse: {e}"),
        )
    })?;
    let stats = row
        .stats_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<Stats>(s).ok());
    Ok(CompileItem {
        id: row.model_id.clone(),
        version: row.version,
        digest: row.digest.clone(),
        manifest,
        artifact,
        stats,
        // A tag list that does not parse is not a reason to refuse a model
        // the node can otherwise serve: it costs the row its `preload_tags`
        // membership and nothing else.
        tags: serde_json::from_str(&row.tags_json).unwrap_or_default(),
    })
}

/// The engine half: the adapters — `previous`'s entry's when its manifest
/// is this one, compiled on `exprs` otherwise — the limits from the config,
/// the permits sized by them. The flag says whether the adapters were
/// carried. `Err` is the reason the first expression that did not compile
/// gave.
fn compile_entry(
    item: CompileItem,
    config: &ModelsConfig,
    exprs: &ExprEngine,
    previous: Option<&ModelSet>,
) -> Result<(ModelEntry, bool), String> {
    // The adapters are a function of the manifest alone, on the one engine.
    let carried = previous
        .and_then(|set| set.get(&item.id))
        .filter(|entry| unchanged(entry, &item.manifest))
        .map(|entry| Arc::clone(&entry.adapters));
    let kept = carried.is_some();
    let adapters = match carried {
        Some(adapters) => adapters,
        None => Arc::new(compile_adapters(&item.manifest, exprs)?),
    };

    let limits = Limits::effective(config, &item.id);
    Ok((
        ModelEntry {
            id: item.id,
            version: item.version,
            digest: item.digest,
            binding: Arc::new(LoadBinding::of(&item.manifest)),
            manifest: item.manifest,
            artifact: item.artifact,
            stats: item.stats,
            tags: item.tags,
            adapters,
            limits,
            permits: Arc::new(Semaphore::new(limits.max_concurrency as usize)),
        },
        kept,
    ))
}

/// Whether `manifest` is `entry`'s, its expressions written as they were
/// compiled: equal, and every object in an adapter or the result in the
/// same key order, since a template's keys come out in the order it was
/// compiled in, and `Value`'s own equality does not count it.
fn unchanged(entry: &ModelEntry, manifest: &Manifest) -> bool {
    entry.manifest == *manifest
        && entry.adapters.inputs.len() == manifest.inputs.len()
        && entry
            .adapters
            .inputs
            .iter()
            .zip(&manifest.inputs)
            .all(|((_, adapter), input)| same(adapter.raw(), &manifest.adapter_for(input)))
        && same(entry.adapters.result.raw(), &manifest.result_logic())
}

/// `a == b`, counting the order of every object's keys.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|((ka, va), (kb, vb))| ka == kb && same(va, vb))
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same(a, b))
        }
        _ => a == b,
    }
}

/// Every adapter and the result of `manifest`, compiled on `exprs`, each
/// refusal in the words 1.12 gave it.
fn compile_adapters(manifest: &Manifest, exprs: &ExprEngine) -> Result<Adapters, String> {
    let mut inputs = Vec::with_capacity(manifest.inputs.len());
    for input in &manifest.inputs {
        let adapter = exprs.compile(&manifest.adapter_for(input)).map_err(|e| {
            format!(
                "adapter for input '{}' does not compile: {}",
                input.name,
                e.message()
            )
        })?;
        inputs.push((input.name.clone(), adapter));
    }
    let result = exprs
        .compile(&manifest.result_logic())
        .map_err(|e| format!("result expression does not compile: {}", e.message()))?;
    Ok(Adapters { inputs, result })
}

/// Every `(task id, model id)` pair in a workflow's steps, its loop's
/// `setup` included, where a task calls `model_infer` with a literal
/// `input.model`, through task groups. A
/// computed reference — an expression, a template — is not seen: the model
/// it resolves to is decided per message. The task id falls back to the
/// step path for a task without one.
pub fn literal_references(tasks: &Value, loop_config: Option<&Value>) -> Vec<(String, String)> {
    crate::engine::walk_steps(tasks, loop_config)
        .tasks
        .into_iter()
        .filter_map(|(path, task)| {
            let function = task.get("function")?;
            if function.get("name").and_then(Value::as_str) != Some(INFER_FUNCTION) {
                return None;
            }
            let named = function.get("input")?.get(INFER_MODEL_FIELD)?.as_str()?;
            let id = task
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or(path);
            Some((id, named.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::fixture;
    use serde_json::json;

    /// The process's expression engine, as a test builds it.
    fn engine() -> ExprEngine {
        crate::engine::kit::test_exprs()
    }

    /// An adapter nested past datalogic's compile ceiling (256 levels):
    /// what an engine in templating mode refuses outright, which a stored
    /// manifest cannot reach, since `serde_json` reads no document nested
    /// past 128 levels.
    fn too_deep() -> Value {
        let mut adapter = json!({ "var": "data.board" });
        for _ in 0..300 {
            adapter = json!({ "+": [adapter] });
        }
        adapter
    }

    fn row(admission: Value, manifest_json: &str) -> Model {
        let now = chrono::Utc::now().naive_utc();
        Model {
            model_id: "ada.c4-tiny".to_string(),
            version: 3,
            status: "active".to_string(),
            digest: "sha256:abc".to_string(),
            manifest_json: manifest_json.to_string(),
            artifact_json: json!({"connector": "bucket", "key": "k", "digest": "sha256:abc"})
                .to_string(),
            admission_json: admission.to_string(),
            stats_json: Some(
                json!({"parameters": 1479, "nodes": 4, "artifact_bytes": 6171, "probe_ms": 0.1,
                    "ir_version": 8, "opset": 17, "runtime": "tract", "device": "cpu"})
                .to_string(),
            ),
            tags_json: "[]".to_string(),
            signature: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn passed() -> Value {
        json!({"state": "passed", "node": "n", "at": "2026-09-13T00:00:00"})
    }

    /// A resident session of no size, standing in for a loaded plan.
    struct Resident;

    impl crate::model::LoadedModel for Resident {
        fn digest(&self) -> &str {
            "sha256:abc"
        }
        fn resident_bytes(&self) -> usize {
            1
        }
        fn run(
            &self,
            _inputs: Vec<dataflow_rs::datavalue::OwnedDataTensor>,
        ) -> Result<Vec<dataflow_rs::datavalue::OwnedDataTensor>, crate::model::RunError> {
            Ok(Vec::new())
        }
    }

    /// Health answers for the model that asked, not for the bytes.
    ///
    /// Two models over one artifact are one digest and two sessions. While
    /// residency was read by digest alone, a model reported `loaded` on the
    /// strength of the *other* model's session — and the load it was waiting
    /// for had not happened.
    #[tokio::test]
    async fn health_is_read_per_binding_not_per_digest() {
        let config = ModelsConfig::default();
        let (order_a, order_b) = fixture::two_out();
        let mut rows = [
            row(passed(), fixture::TWO_OUT_A),
            row(passed(), fixture::TWO_OUT_B),
        ];
        rows[0].model_id = order_a.name.clone();
        rows[1].model_id = order_b.name.clone();
        let set = ModelSet::load_active(&rows, &config, true, &engine());
        assert!(set.issues.is_empty(), "{:?}", set.issues);
        let entry_a = set.get(&order_a.name).expect("loaded").clone();
        let entry_b = set.get(&order_b.name).expect("loaded").clone();
        assert_eq!(entry_a.digest, entry_b.digest, "one artifact");
        assert_ne!(
            entry_a.binding.fingerprint(),
            entry_b.binding.fingerprint(),
            "two bindings"
        );

        // Nothing resident: neither reports a state of its own.
        let cache = LoadedCache::new(1024);
        assert!(set.health_of(&order_a.name, 3, &cache).is_none());
        assert!(set.health_of(&order_b.name, 3, &cache).is_none());

        // Only `a`'s session is resident, so only `a` is loaded.
        cache
            .get_or_load(
                crate::model::CacheKey {
                    digest: entry_a.digest.clone(),
                    runtime: "tract",
                    device: "cpu".to_string(),
                    binding: entry_a.binding.fingerprint().to_string(),
                },
                || async { Ok(Arc::new(Resident) as Arc<dyn crate::model::LoadedModel>) },
            )
            .await
            .expect("loads");
        assert_eq!(
            set.health_of(&order_a.name, 3, &cache).map(|h| h.state),
            Some("loaded".to_string())
        );
        assert!(
            set.health_of(&order_b.name, 3, &cache).is_none(),
            "the other model's session is not this one's"
        );
    }

    /// The fixture row loads: one compiled adapter per input, the result
    /// compiled, the stats decoded, the limits and permits from the config.
    #[test]
    fn an_admitted_row_loads_with_its_adapters_compiled() {
        let config = ModelsConfig::default();
        let set = ModelSet::load_active(
            &[row(passed(), fixture::MANIFEST)],
            &config,
            true,
            &engine(),
        );
        assert!(set.issues.is_empty(), "{:?}", set.issues);
        let entry = set.get("ada.c4-tiny").expect("loaded");
        assert_eq!(entry.version, 3);
        assert_eq!(entry.digest, "sha256:abc");
        assert_eq!(entry.adapters.inputs.len(), 1);
        assert_eq!(entry.adapters.inputs[0].0, "board");
        assert_eq!(entry.stats.as_ref().map(|s| s.parameters), Some(1479));
        assert_eq!(entry.limits, Limits::effective(&config, "ada.c4-tiny"));
        assert_eq!(
            entry.permits.available_permits(),
            config.max_concurrency_per_model as usize
        );
        assert_eq!(set.ids().collect::<Vec<_>>(), ["ada.c4-tiny"]);
        assert!(set.issue_for("ada.c4-tiny").is_none());

        // The adapters evaluate on the engine the set holds, the one they
        // were compiled on, and produce the declared tensor from the
        // fixture's `data.board` shape.
        let board: Vec<Vec<Vec<f32>>> = vec![vec![vec![0.0; 7]; 6]; 2];
        let root = OwnedDataValue::from(&json!({"data": {"board": [board]}}));
        let value = set
            .exprs()
            .eval(&entry.adapters.inputs[0].1, &root)
            .expect("evaluates");
        assert!(
            value.ops.is_some(),
            "a budget build meters every evaluation"
        );
        let tensor = value.value.as_tensor().expect("a tensor");
        assert_eq!(tensor.shape(), [1, 2, 6, 7]);
        assert_eq!(tensor.dtype().name(), "f32");
    }

    /// Each way a row does not load is an issue naming its stage, and the
    /// row is absent from the served set.
    #[test]
    fn every_issue_kind_is_reported_and_keeps_the_model_out() {
        let config = ModelsConfig::default();
        let engine = engine();
        let cases: Vec<(&str, Model, &str)> = vec![
            (
                "disabled",
                row(passed(), fixture::MANIFEST),
                "models.enabled = false",
            ),
            (
                "admission",
                row(json!({"state": "pending"}), fixture::MANIFEST),
                "admission is 'pending'",
            ),
            (
                "admission",
                row(
                    json!({"state": "failed", "stage": "fetch", "reason": "GET answered HTTP 503"}),
                    fixture::MANIFEST,
                ),
                "at stage 'fetch': GET answered HTTP 503",
            ),
            ("manifest", row(passed(), "{not json"), "does not parse"),
            // The `adapter` stage is not here: a serving engine runs in
            // templating mode, where only nesting past datalogic's ceiling
            // fails to compile, and a stored manifest cannot be nested that
            // deep (`too_deep`). The offline set exercises it.
        ];
        for (stage, row, needle) in cases {
            let enabled = stage != "disabled";
            let set = ModelSet::load_active(std::slice::from_ref(&row), &config, enabled, &engine);
            assert!(set.get("ada.c4-tiny").is_none(), "{stage}: must not serve");
            let issue = set.issue_for("ada.c4-tiny").expect(stage);
            assert_eq!(issue.stage, stage, "{}", issue.reason);
            assert!(issue.reason.contains(needle), "{stage}: {}", issue.reason);
            assert_eq!(issue.version, 3);
            assert_eq!(issue.digest, "sha256:abc");
            // The health view names the stage and the reason.
            let cache = LoadedCache::new(1 << 20);
            let health = set.health_of("ada.c4-tiny", 3, &cache).expect("an account");
            assert_eq!(
                health.state,
                if stage == "disabled" {
                    "disabled"
                } else {
                    "failed"
                }
            );
            assert!(health.reason.as_deref().unwrap_or("").starts_with(stage));
            assert!(set.health_of("ada.c4-tiny", 2, &cache).is_none());
        }
        // An unparseable artifact reference has its own stage.
        let mut broken = row(passed(), fixture::MANIFEST);
        broken.artifact_json = "nope".to_string();
        let set = ModelSet::load_active(&[broken], &config, true, &engine);
        assert_eq!(
            set.issue_for("ada.c4-tiny").expect("issue").stage,
            "artifact"
        );
    }

    /// An offline set compiles the same entries a row set does, without a
    /// row: version `0`, the file's digest, the stats the caller read, and
    /// a manifest whose adapters do not compile becomes an `adapter` issue
    /// rather than a silent absence.
    #[test]
    fn an_offline_set_is_built_from_manifests_alone() {
        let config = ModelsConfig::default();
        let engine = engine();
        let entry = |text: &str| ManifestEntry {
            manifest: Manifest::parse(text).expect("valid"),
            artifact_path: std::path::PathBuf::from("c4-tiny.onnx"),
            digest: "sha256:abc".to_string(),
            stats: None,
        };
        let set = ModelSet::from_manifests([entry(fixture::MANIFEST)], &config, &engine);
        assert!(set.issues.is_empty(), "{:?}", set.issues);
        let loaded = set.get("ada.c4-tiny").expect("compiled");
        assert_eq!(loaded.version, 0);
        assert_eq!(loaded.digest, "sha256:abc");
        assert_eq!(loaded.artifact.key, "c4-tiny.onnx");
        assert_eq!(loaded.adapters.inputs.len(), 1);

        let mut deep = entry(fixture::MANIFEST);
        deep.manifest.inputs[0].adapter = Some(too_deep());
        let set = ModelSet::from_manifests([deep], &config, &engine);
        assert!(set.get("ada.c4-tiny").is_none());
        let issue = set.issue_for("ada.c4-tiny").expect("an issue");
        assert_eq!(issue.stage, "adapter");
        assert_eq!(issue.version, 0);
        assert!(
            issue
                .reason
                .starts_with("adapter for input 'board' does not compile: "),
            "{}",
            issue.reason
        );
    }

    /// Literal references are found through task groups; computed ones and
    /// other functions are not references.
    #[test]
    fn literal_references_walk_groups_and_skip_computed_ones() {
        let infer = |id: &str, model: Value| {
            json!({"id": id, "name": id, "function": {"name": "model_infer",
                "input": {"model": model, "input": {"var": ""}}}})
        };
        let tasks = json!([
            infer("plain", json!("ada.c4-tiny")),
            {"id": "group", "tasks": [infer("nested", json!("ada.other"))]},
            infer("computed", json!({"var": "data.model"})),
            {"id": "log", "name": "log", "function": {"name": "log",
                "input": {"message": "ada.c4-tiny"}}},
        ]);
        assert_eq!(
            literal_references(&tasks, None),
            vec![
                ("plain".to_string(), "ada.c4-tiny".to_string()),
                ("nested".to_string(), "ada.other".to_string()),
            ]
        );
        assert!(literal_references(&json!([]), None).is_empty());
    }
}
