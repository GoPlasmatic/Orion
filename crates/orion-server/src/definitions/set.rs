//! The set itself, and the two ways one is loaded.

use super::json::Document;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Which kind of entity a JSON document describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entity {
    Workflow,
    Channel,
    Connector,
}

impl Entity {
    pub fn as_str(self) -> &'static str {
        match self {
            Entity::Workflow => "workflow",
            Entity::Channel => "channel",
            Entity::Connector => "connector",
        }
    }

    /// Classify a document by **shape**, not by filename.
    ///
    /// A definition directory is the natural home for the fixtures and request
    /// bodies its entities are exercised with — Orion's own
    /// `examples/packages/` holds `request.json` beside `workflow.json` — so a
    /// loader that treats every `*.json` as an entity reports the fixtures as
    /// broken entities. `orion-server test` hit the same problem and answered
    /// it with the `*.case.json` suffix; a suffix is the wrong answer here,
    /// because the layout belongs to whoever authored the directory and Orion
    /// should not impose one on a set that already exists.
    ///
    /// The three kinds are disjoint on a field that create already requires,
    /// so a positive match is reliable and anything unmatched is honestly "not
    /// an entity" rather than "an entity I could not read".
    pub fn classify(doc: &Value) -> Option<Entity> {
        let obj = doc.as_object()?;
        if obj.contains_key("tasks") {
            return Some(Entity::Workflow);
        }
        if obj.contains_key("connector_type") {
            return Some(Entity::Connector);
        }
        if obj.contains_key("channel_type") || obj.contains_key("protocol") {
            return Some(Entity::Channel);
        }
        None
    }
}

/// Names a set may reference without containing.
///
/// A promotion artifact declares these in `requires`, because the target
/// instance is expected to already have them and closure checking would
/// otherwise refuse a deliberately small package. A directory gets the same
/// concept with an **empty** default: everything must resolve in-set, because
/// the whole point of the gate is the reference that does not.
#[derive(Debug, Default, Clone)]
pub struct Boundary {
    pub channels: Vec<String>,
    pub connectors: Vec<String>,
    /// Model ids a `model_infer` task may name without a manifest in the
    /// set — an artifact's `requires.models`. A directory has no flag for
    /// this: a manifest without its artifact is all a set needs to lint,
    /// and costs nothing to carry.
    pub models: Vec<String>,
}

impl Boundary {
    pub fn allows_channel(&self, name: &str) -> bool {
        self.channels.iter().any(|n| n == name)
    }

    pub fn allows_connector(&self, name: &str) -> bool {
        self.connectors.iter().any(|n| n == name)
    }

    pub fn allows_model(&self, id: &str) -> bool {
        self.models.iter().any(|n| n == id)
    }
}

/// One entity as authored, with where it came from.
///
/// `origin` is what turns a finding into something actionable: `channels[2]`
/// is enough inside an artifact, but a directory of sixty files needs the
/// path.
#[derive(Debug, Clone)]
pub struct Definition {
    pub entity: Entity,
    /// How to name this document in a finding — a file path, or
    /// `workflows[3]` for an artifact entry.
    pub origin: String,
    pub doc: Value,
    /// The same bytes, parsed by the span-carrying front end, when it could
    /// read them.
    ///
    /// Carried here so the document is parsed **once**. `definitions/json.rs`
    /// exists to put `file:line:col` on a finding, but the set used to load
    /// through `serde_json` and throw the spans away — so `lint`, `check` and
    /// `compile` findings had no location at all, and `clippy` got one only by
    /// re-reading every file from disk and parsing it a third time.
    ///
    /// `None` when the strict front end refused the bytes `serde_json`
    /// accepted — a duplicate key is the realistic case. The document still
    /// loads and is still checked; it simply cannot be located, which is
    /// strictly better than refusing to load it. `serde_json` stays the
    /// authority on what a set contains, because it is what the admin API
    /// parses with: a file this front end accepted and `serde_json` did not
    /// would be a set `lint` passes and the server rejects.
    pub spans: Option<Document>,
}

impl Definition {
    /// `(line, column)` of `path` within this document, when it has spans and
    /// the path resolves.
    ///
    /// Two coordinate spaces meet here. A validator's `FieldError` path is
    /// rooted at the *entity* — `workflow.name`, `channel.config.auth` —
    /// because that is what a client reading the error envelope needs. A
    /// document's root, on the other hand, *is* the entity, so its own
    /// coordinate for the same node is `name`. The prefix comes off before the
    /// lookup; without it every field path resolved to nothing and every
    /// schema finding came back with a file but no line.
    pub fn locate(&self, path: &str) -> Option<(usize, usize)> {
        let doc = self.spans.as_ref()?;
        let prefix = format!("{}.", self.entity.as_str());
        let path = path.strip_prefix(&prefix).unwrap_or(path);
        let span = doc.locate(path)?;
        Some(doc.line_col(span.start))
    }
}

/// A plugin manifest that travels with a set — a `plugin.toml` in the tree, a
/// `--plugin-dir`, or a `plugins[]` entry of an artifact.
///
/// The fourth kind, with its own walk rather than a fourth shape: a manifest
/// is TOML, not a JSON entity, and what the set needs from it is the field
/// tables its functions declare. Those go into the registry every check
/// reads, so a workflow naming a plugin function is validated against the
/// manifest exactly as the admin API validates it against the active row.
#[derive(Debug, Clone)]
pub struct PluginDefinition {
    /// How to name the manifest in a finding — its path, or `plugins[2]`.
    pub origin: String,
    pub manifest: crate::plugin::Manifest,
    /// `sha256:…` of the component, when known: read from the file beside a
    /// manifest on disk, or carried by an artifact entry. `None` when the
    /// manifest names a component that is not there — the set still
    /// validates against the manifest, and only running the function needs
    /// the bytes.
    pub digest: Option<String>,
    /// The component file, when the manifest was read from disk and names
    /// one that exists. What `dry-run` and `test` load into the sandbox.
    pub component_path: Option<PathBuf>,
}

impl PluginDefinition {
    /// The manifest read from `path`, with the component beside it hashed
    /// when it is there.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let manifest = crate::plugin::Manifest::parse(&text).map_err(|errors| {
            errors
                .iter()
                .map(|e| format!("{}: {}", e.path, e.message))
                .collect::<Vec<_>>()
                .join("; ")
        })?;
        let component_path = manifest
            .component
            .as_deref()
            .map(|rel| path.parent().unwrap_or_else(|| Path::new(".")).join(rel))
            .filter(|p| p.is_file());
        let digest = match &component_path {
            Some(p) => Some(crate::plugin::WasmRuntime::digest(
                &std::fs::read(p).map_err(|e| format!("reading '{}': {e}", p.display()))?,
            )),
            None => None,
        };
        Ok(Self {
            origin: path.display().to_string(),
            manifest,
            digest,
            component_path,
        })
    }

    /// The registry entries this manifest declares, bound to the digest the
    /// set knows — or to none, which the catalogue shows as an empty digest.
    pub fn entries(&self) -> Vec<crate::engine::FunctionEntry> {
        let binding = crate::engine::PluginBinding {
            id: self.manifest.name.clone(),
            version: 0,
            digest: self.digest.clone().unwrap_or_default(),
            abi: self.manifest.abi.clone(),
        };
        self.manifest.entries(&binding)
    }
}

/// Whether a TOML document is a plugin manifest — by shape, like an entity:
/// an `abi` string under the `orion:plugin` package. A `config.toml` sitting
/// in a definitions tree is not one and is not reported.
pub fn is_plugin_manifest(text: &str) -> bool {
    toml::from_str::<toml::Value>(text).is_ok_and(|doc| {
        doc.get("abi")
            .and_then(toml::Value::as_str)
            .is_some_and(|abi| abi.starts_with("orion:plugin@"))
    })
}

/// A model manifest that travels with a set — a `model.json` in the tree, a
/// `--model-dir`, or a `models[]` entry of an artifact.
///
/// The fifth kind. Found by shape like an entity (`abi` under
/// `orion:model@`), and what the set needs from it is the id a literal
/// `model_infer` reference must resolve to, plus — when the file the
/// manifest's `artifact` names sits beside it — the digest and the graph's
/// stats, which are what `lint` reports, `compile` hashes and an offline run
/// loads.
#[derive(Debug, Clone)]
pub struct ModelDefinition {
    /// How to name the manifest in a finding — its path, or `models[2]`.
    pub origin: String,
    pub manifest: crate::model::Manifest,
    /// The artifact file, when the manifest was read from disk and names one
    /// that exists. What `dry-run` and `test` load.
    pub artifact_path: Option<PathBuf>,
    /// `sha256:…` of that file, read at load. `None` when there is no file:
    /// the set still checks references against the manifest, and only
    /// running the model or compiling it into an artifact needs the bytes.
    pub digest: Option<String>,
    /// The file's size, with the digest.
    pub artifact_bytes: Option<u64>,
    /// What the graph says about itself, read from the bytes the way
    /// admission reads them — or why they do not read as a graph. `None`
    /// without a file.
    pub graph: Option<Result<crate::model::GraphStats, String>>,
}

/// Why a file whose `abi` says model is not a usable manifest.
///
/// Two cases rather than one string, because they report differently: a
/// validation failure has a problem per field and a path for each, which is
/// what an author fixes from, and an unreadable file has neither.
pub enum ModelReadError {
    /// The manifest file, or the artifact beside it, could not be read.
    Unreadable(String),
    /// It is JSON and it did not validate: every problem, each with its path
    /// into the document.
    Invalid(Vec<crate::errors::FieldError>),
}

impl ModelDefinition {
    /// The manifest read from `path`, with the artifact beside it hashed
    /// and read when it is there.
    pub fn from_file(path: &Path) -> Result<Self, ModelReadError> {
        let text =
            std::fs::read_to_string(path).map_err(|e| ModelReadError::Unreadable(e.to_string()))?;
        let manifest = crate::model::Manifest::parse(&text).map_err(ModelReadError::Invalid)?;
        let artifact_path = manifest
            .artifact
            .as_deref()
            .map(|rel| path.parent().unwrap_or_else(|| Path::new(".")).join(rel))
            .filter(|p| p.is_file());
        let (digest, artifact_bytes, graph) = match &artifact_path {
            Some(p) => {
                let bytes = std::fs::read(p).map_err(|e| {
                    ModelReadError::Unreadable(format!("reading '{}': {e}", p.display()))
                })?;
                (
                    Some(crate::crypto::sha256_digest(&bytes)),
                    Some(bytes.len() as u64),
                    Some(crate::model::read_stats(&bytes)),
                )
            }
            None => (None, None, None),
        };
        Ok(Self {
            origin: path.display().to_string(),
            manifest,
            artifact_path,
            digest,
            artifact_bytes,
            graph,
        })
    }

    /// A manifest with no file behind it — an artifact's `models[]` entry,
    /// whose bytes live in a bucket the target fetches from.
    pub fn from_manifest(origin: String, manifest: crate::model::Manifest) -> Self {
        Self {
            origin,
            manifest,
            artifact_path: None,
            digest: None,
            artifact_bytes: None,
            graph: None,
        }
    }

    /// What an offline run loads, when the artifact is on disk and reads as
    /// a graph.
    pub fn manifest_entry(&self) -> Option<crate::model::ManifestEntry> {
        let artifact_path = self.artifact_path.clone()?;
        let digest = self.digest.clone()?;
        let stats = match (&self.graph, self.artifact_bytes) {
            (Some(Ok(graph)), Some(bytes)) => Some(crate::model::Stats::offline(graph, bytes)),
            _ => None,
        };
        Some(crate::model::ManifestEntry {
            manifest: self.manifest.clone(),
            artifact_path,
            digest,
            stats,
        })
    }
}

/// Channels, workflows and connectors that must be consistent with each other.
#[derive(Debug, Default, Clone)]
pub struct DefinitionSet {
    pub definitions: Vec<Definition>,
    /// The plugin manifests the set carries, in origin order.
    pub plugins: Vec<PluginDefinition>,
    /// The model manifests the set carries, in origin order.
    pub models: Vec<ModelDefinition>,
}

/// What a directory load skipped, so the caller can say so.
///
/// A set lint that silently ignores a file reports green over a set it did not
/// finish reading. That is the same class of failure as a path that resolves
/// to nothing and passes, and it is worse here because the whole feature is
/// "look at everything at once".
#[derive(Debug, Default)]
pub struct LoadReport {
    /// Problems found while merging or expanding the shared definitions —
    /// a duplicate name, an unresolvable `$from`, a missing fragment.
    /// Reported alongside the check pass's own findings.
    pub findings: Vec<super::diagnostic::Diagnostic>,
    /// The catalog the set declared, after merging every shared document.
    pub shared: super::SharedDefinitions,
    /// Authoring pass id → how many documents it rewrote. What `lint` and
    /// `compile` report so an author can see what the compiler did to their
    /// set rather than having to diff the output to find out.
    pub compiled: std::collections::BTreeMap<&'static str, usize>,
    pub skipped: Vec<PathBuf>,
    /// Files that are JSON but classified as no entity kind, versus files that
    /// did not parse at all — the second is far more likely to be a mistake.
    pub unparseable: Vec<(PathBuf, String)>,
}

impl DefinitionSet {
    pub fn is_empty(&self) -> bool {
        self.definitions.is_empty()
    }

    pub fn iter(&self, kind: Entity) -> impl Iterator<Item = &Definition> {
        self.definitions.iter().filter(move |d| d.entity == kind)
    }

    pub fn count(&self, kind: Entity) -> usize {
        self.iter(kind).count()
    }

    /// The registry every check over this set reads: the built-ins plus the
    /// functions the set's manifests declare.
    ///
    /// `Err` names a function two manifests both declare, or one that shadows
    /// a built-in — the same refusal the loader makes, so a set that lints
    /// clean is one whose plugins can all be active at once.
    pub fn function_registry(&self) -> Result<crate::engine::FunctionRegistry, String> {
        crate::engine::FunctionRegistry::builtin().with_entries(
            self.plugins
                .iter()
                .flat_map(PluginDefinition::entries)
                .collect(),
        )
    }

    /// The plugin id a function name belongs to, when a manifest in the set
    /// declares that plugin — `acme.codec.parse` is `acme.codec`'s whether or
    /// not the manifest declares `parse`. `None` for a name no manifest here
    /// could account for.
    pub fn plugin_of(&self, function: &str) -> Option<&PluginDefinition> {
        self.plugins
            .iter()
            .find(|p| function.starts_with(&format!("{}.", p.manifest.name)))
    }

    /// The model manifest for `id`, when the set carries one.
    pub fn model_of(&self, id: &str) -> Option<&ModelDefinition> {
        self.models.iter().find(|m| m.manifest.name == id)
    }

    /// Add every model manifest under each of `dirs` — the `--model-dir`
    /// flag. A JSON file whose `abi` is in the `orion:model@` family is a
    /// manifest; every other file is left alone. Problems reading one are
    /// findings rather than a refusal of the whole set, like an unreadable
    /// entity file.
    pub fn add_model_dirs(
        &mut self,
        dirs: &[String],
    ) -> Result<Vec<super::diagnostic::Diagnostic>, String> {
        let mut findings = Vec::new();
        for dir in dirs {
            let dir = Path::new(dir);
            if dir.is_file() {
                if let Ok(doc) = std::fs::read_to_string(dir)
                    .map_err(|e| e.to_string())
                    .and_then(|raw| serde_json::from_str::<Value>(&raw).map_err(|e| e.to_string()))
                    && crate::model::is_model_manifest(&doc)
                {
                    self.add_model_file(dir, &mut findings);
                }
                continue;
            }
            let mut paths = Vec::new();
            walk_json_files(dir, &mut |path, parsed, _spans| {
                if parsed.is_ok_and(|doc| crate::model::is_model_manifest(&doc)) {
                    paths.push(path);
                }
            })?;
            paths.sort();
            for path in paths {
                self.add_model_file(&path, &mut findings);
            }
        }
        Ok(findings)
    }

    fn add_model_file(&mut self, path: &Path, findings: &mut Vec<super::diagnostic::Diagnostic>) {
        match ModelDefinition::from_file(path) {
            Ok(model) => {
                if let Some(existing) = self.model_of(&model.manifest.name) {
                    findings.push(super::diagnostic::Diagnostic::error(
                        "duplicate.model",
                        format!("model '{}'", model.manifest.name),
                        format!("declared twice: {} and {}", existing.origin, path.display()),
                    ));
                    return;
                }
                self.models.push(model);
            }
            // A file the walk recognised as a manifest and the validator
            // then refused is a definition with a problem, not a file that
            // was never one: every field error is reported under its own
            // path, the way a workflow's schema refusal is, so the author
            // reads the same message here as `POST /admin/models` gives.
            Err(ModelReadError::Invalid(problems)) => {
                let origin = path.display().to_string();
                let spans = std::fs::read_to_string(path)
                    .ok()
                    .and_then(|text| super::json::Document::parse(&text).ok());
                findings.extend(problems.iter().map(|problem| {
                    let d = super::diagnostic::Diagnostic::from_field_error(
                        "parse.model",
                        "model manifest",
                        problem,
                    );
                    let line = d
                        .path
                        .as_deref()
                        .and_then(|p| spans.as_ref()?.locate(p))
                        .map(|span| spans.as_ref().expect("located in it").line_col(span.start));
                    let path = d.path.clone();
                    d.with_location(&origin, path.as_deref(), line)
                }));
            }
            Err(ModelReadError::Unreadable(reason)) => {
                findings.push(super::diagnostic::Diagnostic::error(
                    "parse.model",
                    "model manifest",
                    format!("not a valid model manifest: {reason}"),
                ))
            }
        }
    }

    /// Add every manifest under each of `dirs` — the `--plugin-dir` flag.
    /// Problems reading one are returned as findings rather than refusing
    /// the whole set, like an unreadable entity file.
    pub fn add_plugin_dirs(
        &mut self,
        dirs: &[String],
    ) -> Result<Vec<super::diagnostic::Diagnostic>, String> {
        let mut findings = Vec::new();
        for dir in dirs {
            let dir = Path::new(dir);
            if dir.is_file() {
                self.add_manifest_file(dir, &mut findings);
                continue;
            }
            let mut paths = Vec::new();
            walk_paths(dir, "toml", &mut |p| paths.push(p))?;
            paths.sort();
            for path in paths {
                self.add_manifest_file(&path, &mut findings);
            }
        }
        Ok(findings)
    }

    fn add_manifest_file(
        &mut self,
        path: &Path,
        findings: &mut Vec<super::diagnostic::Diagnostic>,
    ) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        if !is_plugin_manifest(&text) {
            return;
        }
        match PluginDefinition::from_file(path) {
            Ok(plugin) => {
                if let Some(existing) = self
                    .plugins
                    .iter()
                    .find(|p| p.manifest.name == plugin.manifest.name)
                {
                    findings.push(super::diagnostic::Diagnostic::error(
                        "duplicate.plugin",
                        format!("plugin '{}'", plugin.manifest.name),
                        format!("declared twice: {} and {}", existing.origin, path.display()),
                    ));
                    return;
                }
                self.plugins.push(plugin);
            }
            Err(reason) => findings.push(super::diagnostic::Diagnostic::error(
                "parse.plugin",
                path.display().to_string(),
                format!("not a valid plugin manifest: {reason}"),
            )),
        }
    }

    /// Build a set from already-parsed documents — the artifact loader's entry
    /// point, and the one unit tests use.
    ///
    /// `origin` is called with the index within its array, so an artifact can
    /// keep reporting `workflows[3]` exactly as it did before.
    pub fn from_entries(entries: impl IntoIterator<Item = (Entity, String, Value)>) -> Self {
        Self {
            definitions: entries
                .into_iter()
                .map(|(entity, origin, doc)| Definition {
                    entity,
                    origin,
                    doc,
                    // An entry handed over as a `Value` has no source text to
                    // span — an artifact entry, or a single document a caller
                    // already parsed.
                    spans: None,
                })
                .collect(),
            plugins: Vec::new(),
            models: Vec::new(),
        }
    }

    /// Load every entity under `dir`, recursively.
    ///
    /// Recursive because the layouts this exists to serve nest — a
    /// `definitions/` tree with `fragments/` and per-service subdirectories is
    /// the shape that motivated #286 — and because a one-level walk would
    /// silently omit a nested entity, which is the failure this is meant to
    /// remove rather than relocate.
    ///
    /// Hidden entries and `target/` are skipped without comment; everything
    /// else that is not an entity is reported in the [`LoadReport`].
    pub fn from_directory(dir: &Path) -> Result<(Self, LoadReport), String> {
        Self::from_directory_with(dir, &super::SharedDefinitions::default())
    }

    /// Load every entity under `dir` **without compiling** — `use` and
    /// `$from` left as the author wrote them — and the shared catalog beside
    /// them.
    ///
    /// What the duplication rules of `clippy` read: after expansion every
    /// fragment call site *is* a repeated sequence, so a rule that suggests
    /// fragments must see the form that still names them.
    pub fn from_directory_raw(dir: &Path) -> Result<(Self, LoadReport), String> {
        let mut set = DefinitionSet::default();
        let mut report = LoadReport::default();
        let mut shared_docs: Vec<(String, Value)> = Vec::new();
        walk(dir, &mut set, &mut report, &mut shared_docs)?;
        set.definitions.sort_by(|a, b| a.origin.cmp(&b.origin));
        report.skipped.sort();
        report.unparseable.sort();
        shared_docs.sort_by(|a, b| a.0.cmp(&b.0));
        let mut shared = super::SharedDefinitions::default();
        for (origin, doc) in &shared_docs {
            shared.merge(doc, origin, &mut report.findings);
        }
        report.shared = shared;
        Ok((set, report))
    }

    /// [`Self::from_directory`] starting from shared definitions the caller
    /// already has — how a single-file command (`dry-run`, `test`,
    /// `lint <file>`) borrows a directory's catalog without linting it.
    pub fn from_directory_with(
        dir: &Path,
        seed: &super::SharedDefinitions,
    ) -> Result<(Self, LoadReport), String> {
        let mut set = DefinitionSet::default();
        let mut report = LoadReport::default();
        let mut shared_docs: Vec<(String, Value)> = Vec::new();
        walk(dir, &mut set, &mut report, &mut shared_docs)?;

        // Stable order regardless of directory iteration, so two runs on two
        // machines produce the same report and a diff of the output is signal.
        set.definitions.sort_by(|a, b| a.origin.cmp(&b.origin));
        report.skipped.sort();
        report.unparseable.sort();
        shared_docs.sort_by(|a, b| a.0.cmp(&b.0));

        // Shared definitions are collected across the whole tree before any
        // entity is expanded: a workflow may reference a fragment declared in
        // a file the walk reaches later, and load order is not something an
        // author should have to reason about.
        let mut shared = seed.clone();
        for (origin, doc) in &shared_docs {
            shared.merge(doc, origin, &mut report.findings);
        }
        if !shared.is_empty() {
            for def in &mut set.definitions {
                let origin = def.origin.clone();
                let cx = super::compile::Cx {
                    shared: &shared,
                    origin: &origin,
                };
                for pass in super::compile::compile(&mut def.doc, &cx, &mut report.findings) {
                    *report.compiled.entry(pass).or_default() += 1;
                }
            }
        }
        report.shared = shared;
        Ok((set, report))
    }
}

/// The one traversal of a definition directory: which files are part of a set,
/// and what each one parsed to.
///
/// Both loaders drive this — the full set load below, and
/// [`super::SharedDefinitions::from_directory`], which wants only the shared
/// documents. They had a walk each, and the copies had already drifted on what
/// happens to a file that will not parse. What counts as part of a set is a
/// single decision, so it is made once here; the callers differ only in what
/// they keep.
///
/// `visit` receives every `.json` file found, with its parsed document or the
/// reason it could not be read. Hidden entries, `target/` and `node_modules/`
/// are skipped without comment — they are noise, not omissions worth
/// reporting.
pub(super) fn walk_json_files(
    dir: &Path,
    visit: &mut impl FnMut(std::path::PathBuf, Result<Value, String>, Option<Document>),
) -> Result<(), String> {
    walk_json_paths(dir, &mut |path| {
        let read = std::fs::read_to_string(&path).map_err(|e| e.to_string());
        let parsed = read
            .as_ref()
            .map_err(|e| e.clone())
            .and_then(|raw| serde_json::from_str::<Value>(raw).map_err(|e| e.to_string()));
        // The same bytes through the span-carrying front end, so a finding can
        // say `file:line:col`. `serde_json` above stays the authority on
        // whether the document *loads* — it is what the admin API parses with,
        // so a file this front end accepts and `serde_json` refuses must not
        // reach the checks. A file it cannot read just has no spans.
        let spans = read.ok().and_then(|raw| Document::parse(&raw).ok());
        visit(path, parsed, spans);
    })
}

/// Every `.json` file under `dir`, sorted, without reading any of them —
/// what `fmt` walks, since it parses with its own front end.
///
/// The same traversal as `walk_json_files`, so what `fmt` formats and what
/// `lint` reads are one set of files.
pub fn json_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    walk_json_paths(dir, &mut |path| out.push(path))?;
    out.sort();
    Ok(out)
}

/// The traversal itself: which paths are part of a set.
///
/// A symlinked directory is **not** followed. A set that links to itself, or
/// two sets that link to each other, would otherwise walk forever; a
/// symlinked *file* is fine and is visited through the link.
fn walk_json_paths(dir: &Path, visit: &mut impl FnMut(PathBuf)) -> Result<(), String> {
    walk_paths(dir, "json", visit)
}

/// The traversal for one file extension. `json` is the entities and shared
/// documents; `toml` is the plugin manifests, walked by the same rules so the
/// two kinds of file in one tree are found by one decision.
fn walk_paths(dir: &Path, ext: &str, visit: &mut impl FnMut(PathBuf)) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("cannot read '{}': {e}", dir.display()))?;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "target" || name == "node_modules" {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            walk_paths(&path, ext, visit)?;
            continue;
        }
        if file_type.is_symlink() && path.is_dir() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some(ext) {
            continue;
        }
        visit(path);
    }
    Ok(())
}

fn walk(
    dir: &Path,
    set: &mut DefinitionSet,
    report: &mut LoadReport,
    shared_docs: &mut Vec<(String, Value)>,
) -> Result<(), String> {
    // The plugin manifests in the tree, before the entities: a workflow is
    // checked against the registry the manifests build, so they have to be
    // known first. Sorted, like everything else, so two machines agree.
    let mut manifests = Vec::new();
    walk_paths(dir, "toml", &mut |p| manifests.push(p))?;
    manifests.sort();
    for path in manifests {
        set.add_manifest_file(&path, &mut report.findings);
    }
    walk_json_files(dir, &mut |path, parsed, spans| {
        let doc = match parsed {
            Ok(doc) => doc,
            Err(e) => {
                report.unparseable.push((path, e));
                return;
            }
        };
        match Entity::classify(&doc) {
            Some(entity) => set.definitions.push(Definition {
                entity,
                origin: path.display().to_string(),
                doc,
                spans,
            }),
            None if super::SharedDefinitions::is_shared_document(&doc) => {
                shared_docs.push((path.display().to_string(), doc));
            }
            // A model manifest is JSON like an entity, found by its `abi`
            // rather than a filename, so a set holds its models where it
            // holds everything else.
            None if crate::model::is_model_manifest(&doc) => {
                set.add_model_file(&path, &mut report.findings);
            }
            None => report.skipped.push(path),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The three kinds must be told apart by a field create already requires,
    /// and a fixture must not be mistaken for any of them.
    #[test]
    fn entities_are_classified_by_shape() {
        assert_eq!(
            Entity::classify(&json!({"name": "w", "tasks": []})),
            Some(Entity::Workflow)
        );
        assert_eq!(
            Entity::classify(&json!({"name": "c", "connector_type": "db", "config": {}})),
            Some(Entity::Connector)
        );
        assert_eq!(
            Entity::classify(&json!({"name": "ch", "channel_type": "rest", "protocol": "http"})),
            Some(Entity::Channel)
        );
        // `examples/packages/*/request.json` — a fixture living beside the
        // entities it exercises.
        assert_eq!(Entity::classify(&json!({"data": {"amount": 5}})), None);
        assert_eq!(Entity::classify(&json!([1, 2, 3])), None);
    }

    /// A connector carries `config`, not `tasks`; a workflow carries `tasks`.
    /// Nothing carries both, so the order of the checks cannot misfile one.
    #[test]
    fn a_connector_is_not_read_as_a_workflow() {
        let connector = json!({
            "name": "orders-db", "connector_type": "db",
            "config": {"connection_string": "postgres://x"}
        });
        assert_eq!(Entity::classify(&connector), Some(Entity::Connector));
    }

    #[test]
    fn a_directory_load_reports_what_it_skipped() {
        let dir = std::env::temp_dir().join(format!("orion-defs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("nested")).expect("test fixture");
        std::fs::write(
            dir.join("workflow.json"),
            r#"{"name":"w","workflow_id":"w","tasks":[]}"#,
        )
        .expect("test fixture");
        // Nested, to prove the walk recurses rather than silently missing it.
        std::fs::write(
            dir.join("nested/channel.json"),
            r#"{"name":"c","channel_id":"c","channel_type":"rest","protocol":"http"}"#,
        )
        .expect("test fixture");
        std::fs::write(dir.join("request.json"), r#"{"data":{}}"#).expect("test fixture");
        std::fs::write(dir.join("broken.json"), r#"{not json"#).expect("test fixture");
        std::fs::write(dir.join("README.md"), "not json at all").expect("test fixture");

        let (set, report) = DefinitionSet::from_directory(&dir).expect("loads");

        assert_eq!(set.count(Entity::Workflow), 1);
        assert_eq!(set.count(Entity::Channel), 1, "the nested entity must load");
        assert_eq!(set.count(Entity::Connector), 0);
        assert_eq!(report.skipped.len(), 1, "request.json is not an entity");
        assert_eq!(report.unparseable.len(), 1, "broken.json must be reported");
        assert!(
            report.unparseable[0].0.ends_with("broken.json"),
            "a file that does not parse is a likely mistake, not a skip"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_boundary_is_empty_by_default() {
        let b = Boundary::default();
        assert!(!b.allows_channel("anything"));
        assert!(!b.allows_connector("anything"));
        assert!(!b.allows_model("anything"));
    }

    /// A model manifest in the tree is the fifth kind: found by its `abi`,
    /// its artifact hashed and read when it sits beside it, and reported as
    /// a manifest without one otherwise. A `--model-dir` finds the same
    /// files by the same rule, and a second manifest for one id is a
    /// duplicate.
    #[test]
    fn model_manifests_are_found_by_shape_and_their_artifacts_read() {
        use crate::model::fixture;
        let dir = std::env::temp_dir().join(format!("orion-defs-models-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("models/c4")).expect("test fixture");
        std::fs::create_dir_all(dir.join("elsewhere")).expect("test fixture");
        std::fs::write(dir.join("models/c4/model.json"), fixture::MANIFEST).expect("test fixture");
        std::fs::write(dir.join("models/c4/c4-tiny.onnx"), fixture::ONNX).expect("test fixture");
        // A manifest whose artifact is not there — deployable, not runnable.
        std::fs::write(
            dir.join("elsewhere/model.json"),
            fixture::MANIFEST.replace("ada.c4-tiny", "ada.other"),
        )
        .expect("test fixture");
        std::fs::write(
            dir.join("wf.json"),
            r#"{"name":"w","workflow_id":"w","tasks":[]}"#,
        )
        .expect("test fixture");

        let (set, report) = DefinitionSet::from_directory(&dir).expect("loads");
        assert!(report.findings.is_empty(), "{:?}", report.findings);
        assert_eq!(set.count(Entity::Workflow), 1);
        assert_eq!(set.models.len(), 2, "both manifests are models, not skips");
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);

        let c4 = set.model_of("ada.c4-tiny").expect("the fixture");
        assert!(
            c4.artifact_path
                .as_ref()
                .is_some_and(|p| p.ends_with("c4-tiny.onnx"))
        );
        assert_eq!(
            c4.digest.as_deref(),
            Some(crate::crypto::sha256_digest(fixture::ONNX).as_str())
        );
        assert_eq!(c4.artifact_bytes, Some(fixture::ONNX.len() as u64));
        let graph = c4.graph.as_ref().expect("read").as_ref().expect("a graph");
        assert_eq!(graph.parameters, 1479);
        let entry = c4.manifest_entry().expect("runnable");
        assert_eq!(entry.stats.as_ref().map(|s| s.parameters), Some(1479));
        assert_eq!(entry.stats.as_ref().map(|s| s.artifact_bytes), Some(6171));

        let other = set.model_of("ada.other").expect("the other");
        assert!(other.artifact_path.is_none());
        assert!(other.digest.is_none() && other.graph.is_none());
        assert!(other.manifest_entry().is_none(), "nothing to run");

        // `--model-dir` over the same tree finds the same two, and a second
        // copy of one is a duplicate rather than a silent replacement.
        let mut by_flag = DefinitionSet::default();
        let findings = by_flag
            .add_model_dirs(&[dir.display().to_string()])
            .expect("walks");
        assert!(findings.is_empty(), "{findings:?}");
        assert_eq!(by_flag.models.len(), 2);
        let findings = by_flag
            .add_model_dirs(&[dir.join("models/c4/model.json").display().to_string()])
            .expect("walks");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check, "duplicate.model");
        assert_eq!(by_flag.models.len(), 2);

        // A file that is a model by `abi` but not a valid manifest is a
        // parse finding, not a skip — one per problem, each keeping the path
        // and the file it is in, so the author reads what the admin API
        // would have told them rather than a joined string.
        std::fs::write(
            dir.join("elsewhere/broken.json"),
            "{\n  \"abi\": \"orion:model@1.0.0\",\n  \"name\": \"Bad.Name\",\n  \"version\": \"\"\n}",
        )
        .expect("test fixture");
        let (_, report) = DefinitionSet::from_directory(&dir).expect("loads");
        let parse: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.check == "parse.model")
            .collect();
        let paths: Vec<_> = parse.iter().filter_map(|f| f.path.as_deref()).collect();
        assert_eq!(paths, ["name", "version", "inputs"], "{parse:?}");
        assert!(
            parse.iter().all(|f| f
                .file
                .as_deref()
                .is_some_and(|p| p.ends_with("broken.json"))),
            "every problem names the file it is in: {parse:?}"
        );
        // A path the document has is located; `inputs` is missing rather than
        // wrong, so there is no line to point at and none is invented.
        assert_eq!(
            parse.iter().map(|f| f.line.is_some()).collect::<Vec<_>>(),
            [true, true, false],
            "{parse:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
