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
        let mut set = DefinitionSet::default();
        let mut report = LoadReport::default();
        walk(dir, &mut set, &mut report)?;
        // Stable order regardless of directory iteration, so two runs on two
        // machines produce the same report and a diff of the output is signal.
        set.definitions.sort_by(|a, b| a.origin.cmp(&b.origin));
        report.skipped.sort();
        report.unparseable.sort();
        Ok((set, report))
    }
}

fn walk(dir: &Path, set: &mut DefinitionSet, report: &mut LoadReport) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("cannot read '{}': {e}", dir.display()))?;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Dotfiles and build output are noise, not skips worth reporting.
        if name.starts_with('.') || name == "target" || name == "node_modules" {
            continue;
        }
        if path.is_dir() {
            walk(&path, set, report)?;
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) => {
                report.unparseable.push((path, e.to_string()));
                continue;
            }
        };
        let doc: Value = match serde_json::from_str(&raw) {
            Ok(doc) => doc,
            Err(e) => {
                report.unparseable.push((path, e.to_string()));
                continue;
            }
        };
        match Entity::classify(&doc) {
            Some(entity) => set.definitions.push(Definition {
                entity,
                origin: path.display().to_string(),
                doc,
            }),
            None => report.skipped.push(path),
        }
    }
    Ok(())
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
