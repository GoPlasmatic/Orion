//! The authoring layer: source form in, canonical form out.
//!
//! An author writes conveniences a definition set understands — `$from`
//! splices a shared value, `use` expands a task fragment (#285). The admin
//! API, the engine, traces and the UI understand none of them, and are not
//! meant to: a runtime that had to know about authoring sugar would have to
//! keep knowing about every future piece of it.
//!
//! So the two forms are separated by a compiler, and this module is its
//! pipeline. Each simplification is one [`Pass`]; [`compile`] runs them in a
//! declared order; `orion-server compile` writes the result out as something
//! the admin API accepts.
//!
//! ## Why a pass declares its residue
//!
//! A [`Pass`] must say not only how to rewrite a document but *where its own
//! syntax still appears* in one ([`Pass::residue`]). That one extra method is
//! what makes the layer safe to grow, because it is read three times:
//!
//! 1. the pipeline test asserts residue is empty after [`compile`], which is
//!    the definition of "canonical" and the property the whole runtime relies
//!    on;
//! 2. `compile` reports which passes actually fired, so an author can see what
//!    the command did to their document;
//! 3. the admin API turns leftover residue into the error that **names** it.
//!    Before this existed, an uncompiled `$from` reached the function-input
//!    validator as literal JSON and was refused for missing the fields the
//!    reference would have supplied — an error describing the symptom and
//!    hiding the cause (#295). Every pass added from here gets that error for
//!    free.
//!
//! ## Adding a pass
//!
//! Implement [`Pass`], add it to [`passes`] in the position its inputs
//! require, and give it a **stable id**: ids appear in findings and in the
//! documented table, and a pipeline grandfathers by id — the same contract
//! [`super::check`] gives its checks.
//!
//! Two invariants are asserted for every registered pass, so a new one cannot
//! quietly break the layer:
//!
//! - **idempotent** — `compile(compile(x)) == compile(x)`;
//! - **canonical output** — `residue()` is empty once `compile()` has run,
//!   including on the failure paths, because a pass that leaves its own syntax
//!   behind after refusing it would have the API report a reference the
//!   compiler already rejected.
//!
//! And one rule that is not machine-checked: **residue must mirror the
//! rewrite exactly**. A pass that detects more than it expands refuses
//! documents the compiler would have accepted; one that detects less lets
//! source form reach the runtime.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use super::diagnostic::Diagnostic;
use super::provenance::{SourceMap, Via};
use super::shared::SharedDefinitions;

/// What a pass may resolve against.
///
/// A struct rather than loose arguments so a later pass can be given more —
/// the connector registry, say — without changing [`Pass`] and every
/// implementation of it.
pub struct Cx<'a> {
    pub shared: &'a SharedDefinitions,
    /// How to name this document in a finding: a file path, or
    /// `workflows[3]` for an artifact entry.
    pub origin: &'a str,
    /// The directory of the file this document was read from — what a
    /// relative file reference (`$sql`) resolves against. `None` for an
    /// artifact entry or an in-memory document.
    pub base_dir: Option<&'a Path>,
    /// The definition set's root, which a file reference may not leave.
    /// `None` when a command was given one file and no set.
    pub root: Option<&'a Path>,
}

impl<'a> Cx<'a> {
    /// A document with no file behind it: nothing file-relative resolves.
    pub fn detached(shared: &'a SharedDefinitions, origin: &'a str) -> Self {
        Self {
            shared,
            origin,
            base_dir: None,
            root: None,
        }
    }
}

/// One occurrence of a pass's source form in a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Residue {
    /// The id of the pass that owns this syntax.
    pub pass: &'static str,
    /// What it is, for a message: "a shared-value reference".
    pub noun: &'static str,
    /// The key that marks it — `$from`, `use`.
    pub key: &'static str,
    /// What it names: `constants.db`, or a fragment name.
    pub target: String,
    /// Authored coordinate, rooted at whatever [`residue`] was given:
    /// `tasks[1].function.input`.
    pub path: String,
}

impl Residue {
    /// The reference as it appears in the document: `{"$from": "constants.db"}`.
    ///
    /// Rendered rather than sliced out of the source so the message shows the
    /// reference alone, without whichever siblings happened to sit beside it.
    pub fn syntax(&self) -> String {
        format!(
            "{{{}: {}}}",
            Value::from(self.key),
            Value::from(&*self.target)
        )
    }

    /// The phrasing the single-file commands have always used.
    pub fn describe(&self) -> String {
        match self.key {
            "use" => format!("a reference to fragment '{}'", self.target),
            "$sql" => format!("a reference to SQL file '{}'", self.target),
            _ => format!("a reference to '{}'", self.target),
        }
    }
}

/// One authoring simplification: a rewrite from the source form an author
/// writes to the canonical form the admin API and the engine accept.
pub trait Pass: Send + Sync {
    /// Stable id, never renamed — findings, the compile report and the
    /// documented table all key on it.
    fn id(&self) -> &'static str;

    /// What this pass's source form is, for an error message.
    fn noun(&self) -> &'static str;

    /// Where this pass's syntax appears in `doc`, rooted at `root`.
    ///
    /// `root` is the coordinate `doc` sits at: `""` when it is a whole
    /// authored entity, `"tasks"` when the caller holds a task array on its
    /// own — which is the shape the admin API's validators have.
    fn residue(&self, doc: &Value, root: &str) -> Vec<Residue>;

    /// Rewrite `doc` in place, reporting what would not resolve and
    /// recording, in `map`, where a subtree it brought in was authored.
    fn apply(
        &self,
        doc: &mut Value,
        cx: &Cx<'_>,
        findings: &mut Vec<Diagnostic>,
        map: &mut SourceMap,
    );
}

/// The pipeline, in order.
///
/// Fragments before values: a fragment's tasks may themselves carry `$from`,
/// and splicing afterwards means a fragment is written exactly the way a
/// workflow is. SQL files last, so a `$sql` that arrived through an inlined
/// fragment or a spliced constant is resolved too.
pub fn passes() -> &'static [&'static dyn Pass] {
    static FRAGMENTS: Fragments = Fragments;
    static VALUES: Values = Values;
    static SQL: Sql = Sql;
    static PASSES: &[&dyn Pass] = &[&FRAGMENTS, &VALUES, &SQL];
    PASSES
}

/// Run every pass over one authored document, returning the ids of those that
/// had anything to do.
pub fn compile(doc: &mut Value, cx: &Cx<'_>, findings: &mut Vec<Diagnostic>) -> Vec<&'static str> {
    compile_with_map(doc, cx, findings, &mut SourceMap::default())
}

/// [`compile`], recording in `map` where each rewritten subtree was authored
/// — what a finding on the compiled form reads to name the file an author
/// should open.
pub fn compile_with_map(
    doc: &mut Value,
    cx: &Cx<'_>,
    findings: &mut Vec<Diagnostic>,
    map: &mut SourceMap,
) -> Vec<&'static str> {
    let mut applied = Vec::new();
    for pass in passes() {
        // Asked before the rewrite, because after it there is nothing left to
        // see — that is the point of the rewrite.
        let fired = !pass.residue(doc, "").is_empty();
        pass.apply(doc, cx, findings, map);
        if fired {
            applied.push(pass.id());
        }
    }
    applied
}

/// Every pass's residue in `doc`, in pipeline order.
///
/// Empty means the document is canonical: nothing in it is waiting to be
/// compiled, so it is safe to store, hash and run.
pub fn residue(doc: &Value, root: &str) -> Vec<Residue> {
    passes()
        .iter()
        .flat_map(|pass| pass.residue(doc, root))
        .collect()
}

// ============================================================
// shared.fragments — `{"id": "_x", "use": "f", "with": {..}}`
// ============================================================

struct Fragments;

impl Fragments {
    /// Named once and read by both the trait methods and the residue
    /// constructor below, so the id a finding carries and the id the pipeline
    /// reports cannot drift apart.
    const ID: &'static str = "shared.fragments";
    const NOUN: &'static str = "a task-fragment reference";
}

impl Pass for Fragments {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn noun(&self) -> &'static str {
        Self::NOUN
    }

    /// Walks the authored step tree, and only that.
    ///
    /// `use` names a fragment where the expander reads it — an element of a
    /// `tasks` array — and nowhere else, so a payload field that happens to be
    /// called `use` is left alone. The descent into a group mirrors
    /// `expand_tasks`: `is_group` is the engine's own test, so a step this
    /// walk declines to enter is exactly one the expander calls a task.
    fn residue(&self, doc: &Value, root: &str) -> Vec<Residue> {
        let mut out = Vec::new();
        // `root == "tasks"` says the caller already stepped through the key
        // and is holding the array itself.
        if root == "tasks" {
            steps(doc, root, &mut out);
        } else if let Some(tasks) = doc.get("tasks") {
            let at = if root.is_empty() {
                "tasks".to_string()
            } else {
                format!("{root}.tasks")
            };
            steps(tasks, &at, &mut out);
        }
        out
    }

    fn apply(
        &self,
        doc: &mut Value,
        cx: &Cx<'_>,
        findings: &mut Vec<Diagnostic>,
        _map: &mut SourceMap,
    ) {
        if let Some(tasks) = doc.get_mut("tasks").and_then(Value::as_array_mut) {
            let expanded = cx.shared.expand_tasks(tasks, cx, findings);
            *tasks = expanded;
        }
    }
}

fn steps(tasks: &Value, path: &str, out: &mut Vec<Residue>) {
    let Some(items) = tasks.as_array() else {
        return;
    };
    for (i, item) in items.iter().enumerate() {
        let at = format!("{path}[{i}]");
        if let Some(name) = item.get("use").and_then(Value::as_str) {
            out.push(Residue {
                pass: Fragments::ID,
                noun: Fragments::NOUN,
                key: "use",
                target: name.to_string(),
                path: at,
            });
            // The expander replaces this element wholesale; `with` is
            // arguments, not a document with steps of its own.
            continue;
        }
        if crate::engine::is_group(item)
            && let Some(inner) = item.get("tasks")
        {
            steps(inner, &format!("{at}.tasks"), out);
        }
    }
}

// ============================================================
// shared.values — `{"$from": "ns.key", ..siblings}`
// ============================================================

struct Values;

impl Values {
    const ID: &'static str = "shared.values";
    const NOUN: &'static str = "a shared-value reference";
}

impl Pass for Values {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn noun(&self) -> &'static str {
        Self::NOUN
    }

    /// Every depth, matching the splicer: a `$from` is as legal inside a
    /// `map` mapping's `logic` as it is in a task input.
    ///
    /// A non-string `$from` is skipped, because the splicer skips it too — the
    /// two have to refuse and rewrite the same set of documents.
    fn residue(&self, doc: &Value, root: &str) -> Vec<Residue> {
        let mut out = Vec::new();
        spliceable(doc, root, &mut out);
        out
    }

    fn apply(
        &self,
        doc: &mut Value,
        cx: &Cx<'_>,
        findings: &mut Vec<Diagnostic>,
        _map: &mut SourceMap,
    ) {
        cx.shared.splice(doc, cx, findings);
    }
}

fn spliceable(value: &Value, path: &str, out: &mut Vec<Residue>) {
    match value {
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                spliceable(item, &format!("{path}[{i}]"), out);
            }
        }
        Value::Object(map) => {
            if let Some(target) = map.get("$from").and_then(Value::as_str) {
                out.push(Residue {
                    pass: Values::ID,
                    noun: Values::NOUN,
                    key: "$from",
                    target: target.to_string(),
                    path: path.to_string(),
                });
            }
            for (key, v) in map {
                let at = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                spliceable(v, &at, out);
            }
        }
        _ => {}
    }
}

// ============================================================
// shared.sql — `{"$sql": "sql/settle.sql"}`
// ============================================================

/// The largest `.sql` file a reference may inline. Real statements run to a
/// few kilobytes; a file this size is a mistake, not a statement.
pub const MAX_SQL_FILE_BYTES: u64 = 1024 * 1024;

struct Sql;

impl Sql {
    const ID: &'static str = "shared.sql";
    const NOUN: &'static str = "a SQL file reference";
}

impl Pass for Sql {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn noun(&self) -> &'static str {
        Self::NOUN
    }

    /// Every object holding a string `$sql`, at any depth, siblings or not —
    /// the rewrite replaces the object whole in both cases. A non-string
    /// `$sql` is not a reference, as a non-string `$from` is not.
    fn residue(&self, doc: &Value, root: &str) -> Vec<Residue> {
        let mut out = Vec::new();
        sql_references(doc, root, &mut out);
        out
    }

    fn apply(
        &self,
        doc: &mut Value,
        cx: &Cx<'_>,
        findings: &mut Vec<Diagnostic>,
        map: &mut SourceMap,
    ) {
        inline_sql(doc, "", cx, findings, map);
    }
}

fn sql_references(value: &Value, path: &str, out: &mut Vec<Residue>) {
    match value {
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                sql_references(item, &format!("{path}[{i}]"), out);
            }
        }
        Value::Object(map) => {
            if let Some(target) = map.get("$sql").and_then(Value::as_str) {
                out.push(Residue {
                    pass: Sql::ID,
                    noun: Sql::NOUN,
                    key: "$sql",
                    target: target.to_string(),
                    path: path.to_string(),
                });
                return;
            }
            for (key, v) in map {
                let at = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                sql_references(v, &at, out);
            }
        }
        _ => {}
    }
}

/// Replace every `$sql` object under `value` with the normalised statement
/// its file holds — or, when it does not resolve, with `""`: never left in
/// place (a compiled document is canonical on the failure path too), and a
/// string rather than nothing so a required `query` does not draw a second,
/// misleading finding.
fn inline_sql(
    value: &mut Value,
    path: &str,
    cx: &Cx<'_>,
    findings: &mut Vec<Diagnostic>,
    map: &mut SourceMap,
) {
    match value {
        Value::Array(items) => {
            for (i, item) in items.iter_mut().enumerate() {
                inline_sql(item, &format!("{path}[{i}]"), cx, findings, map);
            }
        }
        Value::Object(object) => {
            if let Some(target) = object.get("$sql").and_then(Value::as_str) {
                let target = target.to_string();
                let sibling = object.keys().find(|k| *k != "$sql").cloned();
                let text = match resolve_sql(&target, sibling.as_deref(), path, cx, findings) {
                    Some((file, text)) => {
                        map.record(path, &file, None, vec![Via::Sql { file: file.clone() }]);
                        text
                    }
                    None => String::new(),
                };
                *value = Value::String(text);
                return;
            }
            for (key, v) in object.iter_mut() {
                let at = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                inline_sql(v, &at, cx, findings, map);
            }
        }
        _ => {}
    }
}

/// The file a `$sql` names and its statement in normal form, or `None` with
/// the reason reported.
fn resolve_sql(
    target: &str,
    sibling: Option<&str>,
    path: &str,
    cx: &Cx<'_>,
    findings: &mut Vec<Diagnostic>,
) -> Option<(String, String)> {
    let report = |findings: &mut Vec<Diagnostic>, check: &'static str, message: String| {
        findings.push(Diagnostic::error(check, cx.origin, message).with_location(
            cx.origin,
            Some(path),
            None,
        ));
    };
    if let Some(key) = sibling {
        report(
            findings,
            "shared.sql_shape",
            format!("'$sql' must be the only key in its object — found '{key}' beside it"),
        );
        return None;
    }
    if !is_relative_sql_path(target) {
        report(
            findings,
            "shared.sql_path",
            format!("'{target}' must be a relative path to a .sql file"),
        );
        return None;
    }
    let Some(base_dir) = cx.base_dir else {
        report(
            findings,
            "closure.sql_file",
            format!("'{target}' cannot be resolved: this document was not read from a file"),
        );
        return None;
    };
    let file = lexical_normalize(&base_dir.join(target));
    if let Some(root) = cx.root {
        // Compared absolute, so a root of `.` and a file of `sql/x.sql` —
        // both relative to the working directory — are one comparison.
        let absolute = |p: &Path| match std::env::current_dir() {
            Ok(cwd) if p.is_relative() => lexical_normalize(&cwd.join(p)),
            _ => lexical_normalize(p),
        };
        let escapes_lexically = !absolute(&file).starts_with(absolute(root));
        let escapes_on_disk = match (std::fs::canonicalize(&file), std::fs::canonicalize(root)) {
            (Ok(real), Ok(real_root)) => !real.starts_with(real_root),
            _ => false,
        };
        if escapes_lexically || escapes_on_disk {
            report(
                findings,
                "shared.sql_path",
                format!(
                    "'{target}' leaves the definition set ('{}')",
                    root.display()
                ),
            );
            return None;
        }
    }
    let shown = file.display().to_string();
    let unreadable = |reason: String| format!("'{target}' (resolved to '{shown}') {reason}");
    let meta = match std::fs::metadata(&file) {
        Ok(meta) if meta.is_file() => meta,
        Ok(_) => {
            report(
                findings,
                "closure.sql_file",
                unreadable("is not a file".to_string()),
            );
            return None;
        }
        Err(e) => {
            report(
                findings,
                "closure.sql_file",
                unreadable(format!("cannot be read: {e}")),
            );
            return None;
        }
    };
    if meta.len() > MAX_SQL_FILE_BYTES {
        report(
            findings,
            "closure.sql_file",
            unreadable(format!(
                "is {} bytes; a SQL file may be at most {MAX_SQL_FILE_BYTES}",
                meta.len()
            )),
        );
        return None;
    }
    let source = match std::fs::read(&file).map(String::from_utf8) {
        Ok(Ok(source)) => source,
        Ok(Err(_)) => {
            report(
                findings,
                "closure.sql_file",
                unreadable("is not UTF-8".to_string()),
            );
            return None;
        }
        Err(e) => {
            report(
                findings,
                "closure.sql_file",
                unreadable(format!("cannot be read: {e}")),
            );
            return None;
        }
    };
    match crate::sql_lex::normalize(&source) {
        Ok(normalized) if normalized.text.is_empty() => {
            report(
                findings,
                "shared.sql_empty",
                format!("'{target}' holds no statement (only comments or whitespace)"),
            );
            None
        }
        Ok(normalized) => Some((shown, normalized.text)),
        Err(e) => {
            let (line, col) = crate::sql_lex::line_col(&source, e.offset);
            // Located in the `.sql` file, where the fix is; the entity names
            // the document that referenced it.
            findings.push(
                Diagnostic::error("shared.sql_lex", cx.origin, e.kind.to_string()).with_location(
                    &shown,
                    None,
                    Some((line, col)),
                ),
            );
            None
        }
    }
}

/// A relative path to a `.sql` file: not empty, not absolute, and ending in
/// `.sql`.
pub(super) fn is_relative_sql_path(target: &str) -> bool {
    !target.is_empty()
        && !Path::new(target).is_absolute()
        && !target.starts_with('/')
        && !target.starts_with('\\')
        && Path::new(target)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("sql"))
}

/// `a/./b/../c` → `a/c`, without touching the filesystem. A `..` that would
/// climb above a relative start is kept, so containment still sees it.
pub(super) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out: Vec<Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(component),
            },
            other => out.push(other),
        }
    }
    if out.is_empty() {
        return PathBuf::from(".");
    }
    out.iter().collect()
}

/// `to` expressed relative to `from_dir`, both lexically normalised — what
/// a `$sql` path copied out of a fragment or a constant is rewritten to, so
/// it reads relative to the document it landed in.
pub(super) fn relative(from_dir: &Path, to: &Path) -> PathBuf {
    let from = lexical_normalize(from_dir);
    let to = lexical_normalize(to);
    let from: Vec<Component<'_>> = from
        .components()
        .filter(|c| *c != Component::CurDir)
        .collect();
    let to_parts: Vec<Component<'_>> = to
        .components()
        .filter(|c| *c != Component::CurDir)
        .collect();
    let common = from
        .iter()
        .zip(&to_parts)
        .take_while(|(a, b)| a == b)
        .count();
    let mut out = PathBuf::new();
    for _ in common..from.len() {
        out.push("..");
    }
    for part in &to_parts[common..] {
        out.push(part.as_os_str());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn catalog() -> SharedDefinitions {
        let mut shared = SharedDefinitions::default();
        let mut findings = Vec::new();
        shared.merge(
            &json!({
                "constants": { "db": { "connector": "mongo", "database": "app" } },
                "errors": { "NOT_FOUND": { "status": 404, "body": "nope" } },
                "fragments": { "guard": {
                    "params": { "msg": { "default": "denied" } },
                    "tasks": [ { "id": "deny", "name": "Deny", "function": { "name": "map",
                        "input": { "mappings": [
                            { "path": "data.msg", "logic": { "$param": "msg" } } ] } } } ] } }
            }),
            "catalog.json",
            &mut findings,
        );
        assert!(findings.is_empty(), "{findings:?}");
        shared
    }

    fn sugared() -> Value {
        json!({
            "workflow_id": "w", "name": "w",
            "tasks": [
                { "id": "_g", "use": "guard", "with": { "msg": "no" } },
                { "id": "read", "name": "Read", "function": { "name": "mongo_read",
                    "input": { "$from": "constants.db", "collection": "users" } } },
                { "id": "group", "condition": true, "tasks": [
                    { "id": "_g2", "use": "guard" },
                    { "id": "err", "name": "Err", "function": { "name": "map",
                        "input": { "mappings": [
                            { "path": "data.out", "logic": { "$from": "errors.NOT_FOUND" } } ] } } } ] }
            ]
        })
    }

    #[test]
    fn residue_names_every_reference_with_its_coordinate() {
        let found = residue(&sugared(), "");
        let seen: Vec<(&str, String)> = found
            .iter()
            .map(|r| (r.key, r.path.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            seen,
            vec![
                ("use", "tasks[0]".to_string()),
                ("use", "tasks[2].tasks[0]".to_string()),
                ("$from", "tasks[1].function.input".to_string()),
                (
                    "$from",
                    "tasks[2].tasks[1].function.input.mappings[0].logic".to_string()
                ),
            ],
            "fragments are reported before values, each at the coordinate the author typed"
        );
    }

    #[test]
    fn a_task_array_held_on_its_own_roots_at_tasks() {
        let doc = sugared();
        let found = residue(&doc["tasks"], "tasks");
        assert_eq!(
            found.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            vec![
                "tasks[0]",
                "tasks[2].tasks[0]",
                "tasks[1].function.input",
                "tasks[2].tasks[1].function.input.mappings[0].logic",
            ],
            "the admin API holds `tasks` alone and must get the same coordinates"
        );
    }

    #[test]
    fn use_outside_a_step_is_an_ordinary_field() {
        // A payload field named `use`, and a `tasks` array that is a function
        // input rather than a step list. The expander touches neither, so
        // neither may be reported — a pass that detects more than it expands
        // would refuse documents the compiler accepts.
        let doc = json!({
            "name": "w",
            "tasks": [ { "id": "t", "name": "T", "function": { "name": "http_call",
                "input": { "body": { "use": "cache", "tasks": [ { "use": "nested" } ] } } } } ]
        });
        assert_eq!(residue(&doc, ""), vec![]);
    }

    #[test]
    fn a_non_string_from_is_not_a_reference() {
        // The splicer requires a string, so this walk must too.
        let doc = json!({ "tasks": [ { "function": { "input": { "$from": 5 } } } ] });
        assert_eq!(residue(&doc, ""), vec![]);
    }

    #[test]
    fn compiling_reports_the_passes_that_fired_and_leaves_nothing_behind() {
        let shared = catalog();
        let mut doc = sugared();
        let mut findings = Vec::new();
        let applied = compile(&mut doc, &Cx::detached(&shared, "wf.json"), &mut findings);
        assert!(findings.is_empty(), "{findings:?}");
        assert_eq!(applied, vec!["shared.fragments", "shared.values"]);
        assert_eq!(
            passes().iter().map(|p| p.id()).collect::<Vec<_>>(),
            vec!["shared.fragments", "shared.values", "shared.sql"]
        );
        assert_eq!(
            residue(&doc, ""),
            vec![],
            "a compiled document is canonical — this is what the runtime relies on"
        );
        // The splice merged, and the fragment's ids were namespaced.
        assert_eq!(doc["tasks"][1]["function"]["input"]["connector"], "mongo");
        assert_eq!(doc["tasks"][1]["function"]["input"]["collection"], "users");
        assert_eq!(doc["tasks"][0]["id"], "_g.deny");
    }

    #[test]
    fn compiling_is_idempotent() {
        let shared = catalog();
        let cx = Cx::detached(&shared, "wf.json");
        let mut once = sugared();
        let mut findings = Vec::new();
        compile(&mut once, &cx, &mut findings);
        let mut twice = once.clone();
        let applied = compile(&mut twice, &cx, &mut findings);
        assert_eq!(once, twice);
        assert!(
            applied.is_empty(),
            "a canonical document must fire no pass at all"
        );
    }

    #[test]
    fn nothing_survives_a_reference_that_does_not_resolve() {
        // The failure paths matter as much as the happy one: a pass that
        // refused a reference and left its syntax in place would have the
        // admin API report something the compiler had already rejected.
        let shared = SharedDefinitions::default();
        let mut doc = sugared();
        let mut findings = Vec::new();
        compile(&mut doc, &Cx::detached(&shared, "wf.json"), &mut findings);
        assert!(findings.iter().any(|f| f.is_error()));
        assert_eq!(residue(&doc, ""), vec![]);
    }

    /// A scratch set on disk: `wf/` beside `sql/`.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("orion-sql-pass-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(root.join("wf")).expect("wf");
            std::fs::create_dir_all(root.join("sql")).expect("sql");
            Self(root)
        }
        fn write(&self, rel: &str, text: &str) {
            std::fs::write(self.0.join(rel), text).expect("write");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn with_sql(target: Value) -> Value {
        json!({"workflow_id": "w", "name": "w", "tasks": [
            {"id": "r", "name": "r", "function": {"name": "db_read",
                "input": {"connector": "db", "query": target}}}]})
    }

    fn compile_in(scratch: &Scratch, doc: &mut Value) -> (Vec<Diagnostic>, SourceMap) {
        let shared = SharedDefinitions::default();
        let base = scratch.0.join("wf");
        let mut findings = Vec::new();
        let mut map = SourceMap::default();
        compile_with_map(
            doc,
            &Cx {
                shared: &shared,
                origin: "wf/w.json",
                base_dir: Some(&base),
                root: Some(&scratch.0),
            },
            &mut findings,
            &mut map,
        );
        (findings, map)
    }

    #[test]
    fn a_sql_reference_compiles_to_the_normalised_statement() {
        let scratch = Scratch::new();
        scratch.write(
            "sql/read.sql",
            "-- orders\nSELECT id\n  FROM orders -- all\n WHERE a = $1;\n",
        );
        let mut doc = with_sql(json!({"$sql": "../sql/read.sql"}));
        assert_eq!(residue(&doc, "")[0].key, "$sql");
        assert_eq!(
            residue(&doc, "")[0].describe(),
            "a reference to SQL file '../sql/read.sql'"
        );
        let (findings, map) = compile_in(&scratch, &mut doc);
        assert!(findings.is_empty(), "{findings:?}");
        assert_eq!(
            doc["tasks"][0]["function"]["input"]["query"],
            "SELECT id FROM orders WHERE a = $1"
        );
        assert_eq!(residue(&doc, ""), vec![]);
        let source = map.resolve("wf/w.json", "tasks[0].function.input.query");
        assert!(source.file.ends_with("sql/read.sql"), "{source:?}");
        assert!(!source.is_authored_here());
    }

    #[test]
    fn nothing_survives_a_sql_reference_that_does_not_resolve() {
        let scratch = Scratch::new();
        scratch.write("sql/empty.sql", "-- nothing but a comment\n");
        scratch.write("sql/ambiguous.sql", "SELECT 'it\\'s'");
        for (target, check) in [
            (json!({"$sql": "../sql/missing.sql"}), "closure.sql_file"),
            (json!({"$sql": "../sql/empty.sql"}), "shared.sql_empty"),
            (json!({"$sql": "../sql/ambiguous.sql"}), "shared.sql_lex"),
            (json!({"$sql": "/etc/passwd.sql"}), "shared.sql_path"),
            (json!({"$sql": "../sql/read.txt"}), "shared.sql_path"),
            (json!({"$sql": "../../elsewhere.sql"}), "shared.sql_path"),
            (
                json!({"$sql": "../sql/empty.sql", "x": 1}),
                "shared.sql_shape",
            ),
        ] {
            let mut doc = with_sql(target.clone());
            let (findings, _) = compile_in(&scratch, &mut doc);
            assert!(
                findings.iter().any(|f| f.check == check),
                "{target}: {findings:?}"
            );
            assert_eq!(residue(&doc, ""), vec![], "{target}");
            assert_eq!(doc["tasks"][0]["function"]["input"]["query"], "");
        }
        // With no file behind the document, nothing file-relative resolves.
        let mut doc = with_sql(json!({"$sql": "sql/read.sql"}));
        let mut findings = Vec::new();
        compile(
            &mut doc,
            &Cx::detached(&SharedDefinitions::default(), "workflows[0]"),
            &mut findings,
        );
        assert_eq!(findings[0].check, "closure.sql_file");
    }

    #[test]
    fn a_non_string_sql_is_not_a_reference() {
        let doc = json!({ "tasks": [ { "function": { "input": { "body": { "$sql": 5 } } } } ] });
        assert_eq!(residue(&doc, ""), vec![]);
    }

    #[test]
    fn paths_are_normalised_and_made_relative_lexically() {
        assert_eq!(
            lexical_normalize(Path::new("a/./b/../c")),
            PathBuf::from("a/c")
        );
        assert_eq!(
            lexical_normalize(Path::new("../a/../b")),
            PathBuf::from("../b")
        );
        assert_eq!(lexical_normalize(Path::new("./")), PathBuf::from("."));
        assert_eq!(
            relative(
                Path::new("defs/wf"),
                Path::new("defs/fragments/../sql/x.sql")
            ),
            PathBuf::from("../sql/x.sql")
        );
        assert_eq!(
            relative(Path::new("defs"), Path::new("defs/sql/x.sql")),
            PathBuf::from("sql/x.sql")
        );
        assert!(is_relative_sql_path("sql/x.sql"));
        assert!(is_relative_sql_path("../x.SQL"));
        assert!(!is_relative_sql_path("/x.sql"));
        assert!(!is_relative_sql_path("x.txt"));
        assert!(!is_relative_sql_path(""));
    }

    #[test]
    fn every_pass_has_a_distinct_stable_id() {
        let ids: Vec<&str> = passes().iter().map(|p| p.id()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "pass ids must be unique: {ids:?}");
        assert!(passes().iter().all(|p| !p.noun().is_empty()));
    }
}
