//! Where a compiled document's parts were authored.
//!
//! The authoring passes rewrite a document — a `$sql` reference becomes the
//! statement its file holds — so a coordinate in the compiled form no longer
//! always names text in the entity's own file. The passes record, per
//! compiled coordinate, where that subtree came from; a finding raised on the
//! compiled form asks here which file an author should open.

use std::collections::BTreeMap;

/// How a subtree reached the compiled document, outermost first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Via {
    /// Inlined from a `.sql` file by `$sql`.
    Sql { file: String },
}

impl std::fmt::Display for Via {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Via::Sql { file } => f.write_str(file),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    file: String,
    path: Option<String>,
    via: Vec<Via>,
}

/// Where each rewritten subtree of one compiled document was authored,
/// keyed by its compiled coordinate. Empty for a document no pass rewrote,
/// which is then its own source everywhere.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceMap {
    entries: BTreeMap<String, Entry>,
}

/// The authored source of one compiled coordinate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRef<'a> {
    /// The file holding the authored text: the entity's own file, a shared
    /// document, or a `.sql` file.
    pub file: &'a str,
    /// The coordinate inside `file`; `None` for a file that is not JSON.
    pub path: Option<String>,
    /// How it got here, outermost first; empty when it was typed in the
    /// document itself.
    pub via: &'a [Via],
}

impl SourceMap {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Record that the subtree at `compiled_path` was authored in `file`, at
    /// `path` inside it.
    pub fn record(&mut self, compiled_path: &str, file: &str, path: Option<&str>, via: Vec<Via>) {
        self.entries.insert(
            compiled_path.to_string(),
            Entry {
                file: file.to_string(),
                path: path.map(str::to_string),
                via,
            },
        );
    }

    /// Where `compiled_path` was authored: the entry of the longest recorded
    /// prefix, with the unmatched remainder appended to its path — or, with no
    /// entry, the document itself at the same coordinate.
    pub fn resolve<'a>(&'a self, own_file: &'a str, compiled_path: &str) -> SourceRef<'a> {
        let hit = self
            .entries
            .iter()
            .filter(|(at, _)| is_prefix(at, compiled_path))
            .max_by_key(|(at, _)| at.len());
        match hit {
            Some((at, entry)) => {
                let rest = &compiled_path[at.len()..];
                let path = entry.path.as_ref().map(|p| join(p, rest));
                SourceRef {
                    file: &entry.file,
                    path,
                    via: &entry.via,
                }
            }
            None => SourceRef {
                file: own_file,
                path: Some(compiled_path.to_string()),
                via: &[],
            },
        }
    }
}

/// Whether `prefix` is `path` or a coordinate above it — `a.b` is above
/// `a.b.c` and `a.b[0]`, not above `a.bc`.
fn is_prefix(prefix: &str, path: &str) -> bool {
    path == prefix
        || prefix.is_empty()
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('.') || rest.starts_with('['))
}

fn join(base: &str, rest: &str) -> String {
    match (base.is_empty(), rest.strip_prefix('.')) {
        (true, Some(rest)) => rest.to_string(),
        _ => format!("{base}{rest}"),
    }
}

impl SourceRef<'_> {
    /// Typed in the document itself, not carried in from anywhere — what an
    /// automatic rewrite of the document requires before it may edit here.
    pub fn is_authored_here(&self) -> bool {
        self.via.is_empty()
    }

    /// A short phrase naming where the text came from, for a finding — the
    /// `.sql` file, say — or `None` when it is the document itself.
    pub fn describe(&self) -> Option<String> {
        if self.via.is_empty() {
            return None;
        }
        Some(
            self.via
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_no_entry_a_coordinate_is_its_own_source() {
        let map = SourceMap::default();
        let at = map.resolve("wf.json", "tasks[2].function.input.query");
        assert_eq!(at.file, "wf.json");
        assert_eq!(at.path.as_deref(), Some("tasks[2].function.input.query"));
        assert!(at.is_authored_here());
        assert_eq!(at.describe(), None);
    }

    #[test]
    fn the_longest_prefix_wins_and_the_rest_is_rerooted() {
        let mut map = SourceMap::default();
        map.record(
            "tasks[2].function.input.query",
            "sql/settle.sql",
            None,
            vec![Via::Sql {
                file: "sql/settle.sql".to_string(),
            }],
        );
        map.record(
            "tasks[1]",
            "shared.json",
            Some("fragments.f.tasks[0]"),
            Vec::new(),
        );
        let sql = map.resolve("wf.json", "tasks[2].function.input.query");
        assert_eq!(sql.file, "sql/settle.sql");
        assert_eq!(sql.path, None);
        assert_eq!(sql.describe().as_deref(), Some("sql/settle.sql"));
        let inner = map.resolve("wf.json", "tasks[1].function.input");
        assert_eq!(inner.file, "shared.json");
        assert_eq!(
            inner.path.as_deref(),
            Some("fragments.f.tasks[0].function.input")
        );
        // `tasks[10]` is not under `tasks[1]`.
        assert_eq!(map.resolve("wf.json", "tasks[10]").file, "wf.json");
    }
}
