//! The set itself, and the two ways one is loaded.

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
}

impl Boundary {
    pub fn allows_channel(&self, name: &str) -> bool {
        self.channels.iter().any(|n| n == name)
    }

    pub fn allows_connector(&self, name: &str) -> bool {
        self.connectors.iter().any(|n| n == name)
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
}

/// Channels, workflows and connectors that must be consistent with each other.
#[derive(Debug, Default, Clone)]
pub struct DefinitionSet {
    pub definitions: Vec<Definition>,
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
    pub findings: Vec<super::finding::Finding>,
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
                })
                .collect(),
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
    visit: &mut impl FnMut(std::path::PathBuf, Result<Value, String>),
) -> Result<(), String> {
    walk_json_paths(dir, &mut |path| {
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|raw| serde_json::from_str::<Value>(&raw).map_err(|e| e.to_string()));
        visit(path, parsed);
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
            walk_json_paths(&path, visit)?;
            continue;
        }
        if file_type.is_symlink() && path.is_dir() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
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
    walk_json_files(dir, &mut |path, parsed| {
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
            }),
            None if super::SharedDefinitions::is_shared_document(&doc) => {
                shared_docs.push((path.display().to_string(), doc));
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
    }
}
