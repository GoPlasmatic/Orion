//! What an expression reads and what a task writes, as context paths.
//!
//! Lifted from the admin `/validate` route, which grew these for its
//! unwritten-read advisory and kept them private. They are exact where they
//! are exact — a literal `{"var": "data.x"}` reads `data.x`, a `parse_json`
//! writes `data.<target>` — and [`Reads`] says so explicitly when they are
//! not: a computed `val`, or a `var` inside an element-scoped argument, is
//! a read of *something* this walk cannot name, and every rule built on it
//! must then stay silent rather than guess.

use crate::engine::FunctionRegistry;
use crate::engine::functions::schema::WriteShape;
use serde_json::Value;

use super::operators;

/// The context paths an expression reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reads {
    /// Literal paths, as written: `data.order.total`, `metadata.vars.topic`.
    pub paths: Vec<String>,
    /// A `val` with a computed segment or a scope jump — a path this walk
    /// cannot name.
    pub computed: bool,
    /// A `var`/`val` inside an element-scoped argument (`map`, `filter`, …),
    /// which reads the element rather than the context — and, depending on
    /// the engine's fallback rules, possibly the context too.
    pub scoped: bool,
}

impl Reads {
    /// Whether `paths` is the complete list. When it is not, a rule that
    /// depends on knowing every read has no proof and must not fire.
    pub fn uncertain(&self) -> bool {
        self.computed || self.scoped
    }

    /// Whether any literal read touches `path` — the path itself, something
    /// inside it, or something it is inside.
    pub fn touches(&self, path: &str) -> bool {
        self.paths.iter().any(|read| overlaps(read, path))
    }
}

/// Every read in `value`.
pub fn reads(value: &Value) -> Reads {
    let mut out = Reads::default();
    collect_reads(value, false, &mut out);
    out
}

fn collect_reads(value: &Value, scoped: bool, out: &mut Reads) {
    match value {
        Value::Array(items) => items.iter().for_each(|v| collect_reads(v, scoped, out)),
        Value::Object(map) => {
            if map.len() == 1 {
                let (key, arg) = map.iter().next().expect("one member");
                match key.as_str() {
                    "var" => {
                        read_var(arg, scoped, out);
                        // `{"var": ["path", default]}` — the default may read too.
                        if let Some(rest) = arg.as_array().and_then(|a| a.get(1..)) {
                            rest.iter().for_each(|v| collect_reads(v, scoped, out));
                        }
                        return;
                    }
                    "val" => {
                        read_val(arg, scoped, out);
                        return;
                    }
                    op if operators::is_scoping(op) => {
                        // First argument in the enclosing scope, the rest per
                        // element.
                        match arg {
                            Value::Array(args) => {
                                if let Some(first) = args.first() {
                                    collect_reads(first, scoped, out);
                                }
                                for later in args.iter().skip(1) {
                                    collect_reads(later, true, out);
                                }
                            }
                            other => collect_reads(other, scoped, out),
                        }
                        return;
                    }
                    _ => {}
                }
            }
            map.values().for_each(|v| collect_reads(v, scoped, out));
        }
        _ => {}
    }
}

fn read_var(arg: &Value, scoped: bool, out: &mut Reads) {
    let path = match arg {
        Value::String(s) => Some(s.as_str()),
        Value::Array(items) => match items.first() {
            Some(Value::String(s)) => Some(s.as_str()),
            // `{"var": []}` / `{"var": ""}` is the whole context.
            None => Some(""),
            Some(_) => None,
        },
        Value::Null => Some(""),
        _ => None,
    };
    match (path, scoped) {
        (Some(_), true) => out.scoped = true,
        (Some(p), false) => out.paths.push(p.to_string()),
        (None, _) => out.computed = true,
    }
}

/// `{"val": ["data", "items", 0]}` is `data.items.0`; anything that is not a
/// chain of literal strings and integers is computed.
fn read_val(arg: &Value, scoped: bool, out: &mut Reads) {
    let segments: Option<Vec<String>> = match arg {
        Value::String(s) => Some(vec![s.clone()]),
        Value::Array(items) => items
            .iter()
            .map(|seg| match seg {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .collect(),
        _ => None,
    };
    match (segments, scoped) {
        (Some(_), true) => out.scoped = true,
        (Some(segs), false) => out.paths.push(segs.join(".")),
        (None, _) => out.computed = true,
    }
}

/// Every `data.*` path a JSON subtree reads through a literal `var`/`val` —
/// the shape the admin route's unwritten-read advisory consumes. Reads it
/// cannot name are simply absent, which is the right answer for an advisory
/// that only ever *warns* about a read.
pub fn data_reads(value: &Value) -> Vec<String> {
    reads(value)
        .paths
        .into_iter()
        .filter(|p| p == "data" || p.starts_with("data."))
        .collect()
}

/// Context paths a task writes, for every function that writes one.
///
/// The *shape* comes from the function registry
/// ([`FunctionRegistry::write_shape`]), not from a `match`
/// here. This function used to carry its own copy of every handler's output
/// semantics — in a different file from the handlers, pinned by no test, which
/// is the drift class the rest of this repo turns into build failures. Now the
/// handler declares where it writes and this reads the declaration; extracting
/// the path from the task's input is all that is left.
///
/// An unknown function name yields no writes. The previous catch-all applied
/// the `output`/`response_path` rule to any unrecognised name, so a typoed
/// function contributed a phantom write to the analysis — a function that does
/// not exist is already a lint error, and guessing on top of it only made the
/// clippy rules reason about a pipeline that cannot run.
/// Where a task writes, and whether that list is complete.
///
/// The mirror of [`Reads`], and for the same reason: a destination can be an
/// expression now — `{"output": {"cat": ["data.by_tenant.", {"var": …}]}}` —
/// and a path this walk cannot name is not the same as no path at all. A rule
/// that reasons about overwrites has to know the difference, or it concludes
/// "nothing later writes this" from a step that might write anything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Writes {
    /// Literal destinations, as written.
    pub paths: Vec<String>,
    /// A destination authored as an expression, so it names nothing until a
    /// message arrives. When true, `paths` is not the complete list.
    pub computed: bool,
}

impl Writes {
    /// Whether `paths` is the complete list. When it is not, a rule that
    /// depends on knowing every write has no proof and must not fire.
    pub fn uncertain(&self) -> bool {
        self.computed
    }
}

/// The literal destinations only — [`task_write_facts`] without the certainty.
pub fn task_writes(task: &Value, functions: &FunctionRegistry) -> Vec<String> {
    task_write_facts(task, functions).paths
}

/// Where a task writes, with the certainty flag.
pub fn task_write_facts(task: &Value, functions: &FunctionRegistry) -> Writes {
    let Some(function) = task.get("function") else {
        return Writes::default();
    };
    let name = function.get("name").and_then(Value::as_str).unwrap_or("");
    let Some(input) = function.get("input") else {
        return Writes::default();
    };
    let Some(shape) = functions.write_shape(name) else {
        return Writes::default();
    };

    // A destination authored as a string is that path; authored as anything
    // else it is an expression naming nothing until a message arrives. The same
    // line `ConnectorName` draws between a static and a computed connector.
    fn destination(value: Option<&Value>, out: &mut Writes) -> Option<String> {
        match value {
            None => None,
            Some(Value::String(path)) => Some(path.clone()),
            Some(_) => {
                out.computed = true;
                None
            }
        }
    }

    let mut out = Writes::default();
    match shape {
        WriteShape::Target => {
            if let Some(target) = destination(input.get("target"), &mut out) {
                out.paths.push(format!("data.{target}"));
            }
        }
        WriteShape::Mappings => {
            if let Some(mappings) = input.get("mappings").and_then(Value::as_array) {
                for mapping in mappings {
                    if let Some(path) = destination(mapping.get("path"), &mut out) {
                        out.paths.push(path);
                    }
                }
            }
        }
        WriteShape::OutputPath { default_root } => {
            let authored = input.get("output").or_else(|| input.get("response_path"));
            let is_absent = authored.is_none();
            match destination(authored, &mut out) {
                Some(path) => out.paths.push(path),
                // Absent means the default; computed means unknown, and the
                // default must not stand in for it.
                None if is_absent => out.paths.extend(default_root.map(str::to_string)),
                None => {}
            }
        }
        WriteShape::Nothing => {}
    }
    out
}

/// Whether `path` is covered by something already written, by prefix in
/// either direction. A bare `data` write is the whole context and covers
/// everything under it.
pub fn is_written(path: &str, written: &[String]) -> bool {
    written.iter().any(|w| w == "data" || overlaps(w, path))
}

/// `a` and `b` name the same node, or one is inside the other.
pub fn overlaps(a: &str, b: &str) -> bool {
    a == b
        || a.is_empty()
        || b.is_empty()
        || a.starts_with(&format!("{b}."))
        || b.starts_with(&format!("{a}."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn literal_reads_are_collected_with_their_full_paths() {
        let r = reads(
            &json!({"and": [{">": [{"var": "data.a"}, 1]}, {"val": ["metadata", "vars", "x"]}]}),
        );
        assert_eq!(r.paths, ["data.a", "metadata.vars.x"]);
        assert!(!r.uncertain());
    }

    #[test]
    fn a_default_argument_is_read_too() {
        let r = reads(&json!({"var": ["data.nick", {"var": "data.name"}]}));
        assert_eq!(r.paths, ["data.nick", "data.name"]);
    }

    #[test]
    fn a_read_under_a_scoped_argument_is_not_a_context_read() {
        let r = reads(&json!({"some": [{"var": "data.items"}, {">": [{"var": "qty"}, 0]}]}));
        assert_eq!(
            r.paths,
            ["data.items"],
            "only the array is read from the context"
        );
        assert!(r.scoped && r.uncertain());
    }

    #[test]
    fn a_computed_val_is_uncertain() {
        let r = reads(&json!({"val": ["data", "items", {"val": ["temp_data", "i"]}]}));
        assert!(r.computed && r.uncertain());
        assert!(r.paths.is_empty());
    }

    #[test]
    fn writes_by_function() {
        let parse = json!({"function": {"name": "parse_json", "input": {"source": "payload", "target": "order"}}});
        assert_eq!(
            task_writes(&parse, FunctionRegistry::builtin()),
            ["data.order"]
        );
        let map = json!({"function": {"name": "map", "input": {"mappings": [{"path": "data.a", "logic": 1}, {"path": "temp_data.b", "logic": 2}]}}});
        assert_eq!(
            task_writes(&map, FunctionRegistry::builtin()),
            ["data.a", "temp_data.b"]
        );
        let call = json!({"function": {"name": "http_call", "input": {"connector": "c", "output": "data.resp"}}});
        assert_eq!(
            task_writes(&call, FunctionRegistry::builtin()),
            ["data.resp"]
        );
        let query = json!({"function": {"name": "data_query", "input": {"connector": "c"}}});
        assert_eq!(task_writes(&query, FunctionRegistry::builtin()), ["data"]);
    }

    /// A computed destination is an *unknown* write, not the absence of one.
    ///
    /// The distinction is what keeps the overwrite rules honest: a step whose
    /// destination is an expression might write anything, so a rule that
    /// concludes "nothing later writes this path" from an empty list would be
    /// reasoning from a gap it cannot see.
    #[test]
    fn a_computed_destination_is_uncertain_rather_than_absent() {
        let computed = serde_json::json!({
            "function": {"name": "cache_read", "input": {
                "connector": "c", "key": "k",
                "output": {"cat": ["data.by.", {"var": "data.t"}]}}}
        });
        let facts = task_write_facts(&computed, FunctionRegistry::builtin());
        assert!(facts.paths.is_empty(), "{facts:?}");
        assert!(facts.uncertain(), "{facts:?}");

        // An *omitted* destination is certain — `cache_read` declares no
        // default root, so it contributes no write and the analysis knows that
        // for a fact. Absence and computation must not collapse into one answer.
        let omitted = serde_json::json!({
            "function": {"name": "cache_read", "input": {"connector": "c", "key": "k"}}
        });
        let facts = task_write_facts(&omitted, FunctionRegistry::builtin());
        assert!(facts.paths.is_empty());
        assert!(
            !facts.uncertain(),
            "an omitted destination is not an unknown one"
        );

        // A computed `map` mapping path is the same story.
        let mapping = serde_json::json!({
            "function": {"name": "map", "input": {"mappings": [
                {"path": {"var": "data.where"}, "logic": 1},
                {"path": "data.known", "logic": 2}]}}
        });
        let facts = task_write_facts(&mapping, FunctionRegistry::builtin());
        assert_eq!(facts.paths, ["data.known"]);
        assert!(facts.uncertain(), "{facts:?}");
    }

    #[test]
    fn overlap_is_prefix_in_either_direction() {
        assert!(overlaps("data.order", "data.order.total"));
        assert!(overlaps("data.order.total", "data.order"));
        assert!(!overlaps("data.order", "data.orders"));
        assert!(is_written("data.x.y", &["data".to_string()]));
    }
}
