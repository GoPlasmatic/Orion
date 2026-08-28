//! Rules about saying a thing more than once. They read the **source**
//! form of the set — `use` and `$from` as written — because after expansion
//! every fragment call site is, by construction, a repeated sequence.
//!
//! Each finding is a fact of structural identity: byte-identical after the
//! labels are stripped. What to do about it is offered as a suggestion, in
//! the author's own vocabulary (`fragments`, `constants`, `errors`).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use crate::definitions::analysis::Analysis;
use crate::definitions::analysis::keys::{step_key, value_key};
use crate::definitions::clippy::{Diagnostic, Group, Level, Rule, Scope};
use crate::definitions::{Entity, Fragment};

/// Where a repeated thing was seen.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Site {
    origin: String,
    path: String,
}

impl Site {
    fn render(sites: &[Site]) -> String {
        const SHOWN: usize = 5;
        let mut out: Vec<String> = sites
            .iter()
            .take(SHOWN)
            .map(|s| format!("{} at {}", s.origin, s.path))
            .collect();
        if sites.len() > SHOWN {
            out.push(format!("and {} more", sites.len() - SHOWN));
        }
        out.join("; ")
    }
}

/// Every step list in a source workflow — the top-level `tasks` and each
/// group's — with its path.
fn step_lists<'a>(tasks: &'a Value, path: &str, out: &mut Vec<(String, &'a [Value])>) {
    let Some(items) = tasks.as_array() else {
        return;
    };
    out.push((path.to_string(), items));
    for (i, step) in items.iter().enumerate() {
        if let Some(inner) = step.get("tasks") {
            step_lists(inner, &format!("{path}[{i}].tasks"), out);
        }
    }
}

fn is_use_step(step: &Value) -> bool {
    step.get("use").is_some()
}

// ============================================================
// duplication.fragment_available
// ============================================================

pub struct FragmentAvailable;

impl Rule for FragmentAvailable {
    fn id(&self) -> &'static str {
        "duplication.fragment_available"
    }
    fn group(&self) -> Group {
        Group::Duplication
    }
    fn level(&self) -> Level {
        Level::Warn
    }
    fn scope(&self) -> Scope {
        Scope::Set
    }
    fn summary(&self) -> &'static str {
        "a run of steps is exactly what an existing fragment expands to; a `use` would say it once"
    }
    fn explain(&self) -> &'static str {
        "The set declares a fragment whose `tasks` are, step for step, these steps — \
         exactly, or exactly once the fragment's `$param` holes are bound to the values \
         written here. A `use` step with those values as `with` expands to the same thing.\n\n\
         Proof: structural identity on the source form with step ids ignored (expansion \
         namespaces them); a `$param` hole matches any leaf and the same leaf everywhere \
         the hole recurs; a parameter with no default must be bound.\n\n\
         Silent when: the set has no fragments; the steps already come from a `use`. \
         A suggestion, not a rewrite: the expanded ids become `<use id>.<inner id>`, which \
         traces and `expect_tasks` would then name."
    }

    fn check(&self, cx: &Analysis<'_>, out: &mut Vec<Diagnostic>) {
        if cx.shared.fragments.is_empty() {
            return;
        }
        for def in cx.source.iter(Entity::Workflow) {
            let name = def
                .doc
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(&def.origin);
            let Some(tasks) = def.doc.get("tasks") else {
                continue;
            };
            let mut lists = Vec::new();
            step_lists(tasks, "tasks", &mut lists);
            for (list_path, steps) in lists {
                for (fragment_name, fragment) in &cx.shared.fragments {
                    let n = fragment.tasks.len();
                    if n == 0 || steps.len() < n {
                        continue;
                    }
                    let mut i = 0;
                    while i + n <= steps.len() {
                        let window = &steps[i..i + n];
                        if let Some(with) = matches_fragment(window, fragment) {
                            let call = use_step(&window[0], fragment_name, &with);
                            out.push(
                                Diagnostic::at(
                                    self,
                                    cx,
                                    format!("workflow '{name}'"),
                                    &def.origin,
                                    Some(&format!("{list_path}[{i}]")),
                                    format!(
                                        "these {n} step(s) are exactly fragment '{fragment_name}'"
                                    ),
                                )
                                .with_remedy(format!("replace them with {call}")),
                            );
                            i += n;
                        } else {
                            i += 1;
                        }
                    }
                }
            }
        }
    }
}

/// `Some(with)` when `window` is what `fragment` expands to, with the
/// `with` arguments the call site would need.
fn matches_fragment(window: &[Value], fragment: &Fragment) -> Option<Map<String, Value>> {
    if window.iter().any(is_use_step) {
        return None;
    }
    let mut bound: BTreeMap<String, Value> = BTreeMap::new();
    for (step, template) in window.iter().zip(&fragment.tasks) {
        if !match_step(step, template, &mut bound) {
            return None;
        }
    }
    let mut with = Map::new();
    for (param, default) in &fragment.params {
        match (bound.get(param), default) {
            (Some(value), Some(d)) if value == d => {}
            (Some(value), _) => {
                with.insert(param.clone(), value.clone());
            }
            (None, Some(_)) => {}
            // Required and unbound: the hole must appear somewhere the
            // template did not, so this is not a match we can vouch for.
            (None, None) => return None,
        }
    }
    // A hole naming a parameter the fragment does not declare would fail
    // to expand.
    if bound.keys().any(|k| !fragment.params.contains_key(k)) {
        return None;
    }
    Some(with)
}

/// A step against a template step: ids are ignored, members compared, a
/// group's members compared as steps.
fn match_step(step: &Value, template: &Value, bound: &mut BTreeMap<String, Value>) -> bool {
    let (Some(s), Some(t)) = (step.as_object(), template.as_object()) else {
        return match_value(step, template, bound);
    };
    let keys = |o: &Map<String, Value>| -> BTreeSet<String> {
        o.keys().filter(|k| *k != "id").cloned().collect()
    };
    if keys(s) != keys(t) {
        return false;
    }
    for (key, tv) in t {
        if key == "id" {
            continue;
        }
        let sv = &s[key];
        let ok = if key == "tasks" {
            match (sv.as_array(), tv.as_array()) {
                (Some(a), Some(b)) => {
                    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| match_step(x, y, bound))
                }
                _ => match_value(sv, tv, bound),
            }
        } else {
            match_value(sv, tv, bound)
        };
        if !ok {
            return false;
        }
    }
    true
}

fn match_value(value: &Value, template: &Value, bound: &mut BTreeMap<String, Value>) -> bool {
    if let Some(name) = template
        .as_object()
        .filter(|o| o.len() == 1)
        .and_then(|o| o.get("$param"))
        .and_then(Value::as_str)
    {
        return match bound.get(name) {
            Some(already) => already == value,
            None => {
                bound.insert(name.to_string(), value.clone());
                true
            }
        };
    }
    match (value, template) {
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && b.iter()
                    .all(|(k, tv)| a.get(k).is_some_and(|av| match_value(av, tv, bound)))
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| match_value(x, y, bound))
        }
        _ => value == template,
    }
}

fn use_step(first: &Value, fragment: &str, with: &Map<String, Value>) -> String {
    let mut call = Map::new();
    call.insert(
        "id".into(),
        first
            .get("id")
            .cloned()
            .unwrap_or_else(|| Value::from(format!("_{fragment}"))),
    );
    call.insert("use".into(), Value::from(fragment));
    if !with.is_empty() {
        call.insert("with".into(), Value::Object(with.clone()));
    }
    Value::Object(call).to_string()
}

// ============================================================
// duplication.repeated_task_sequence
// ============================================================

pub struct RepeatedTaskSequence;

impl RepeatedTaskSequence {
    pub const MIN_TASKS: usize = 2;
    pub const MIN_OCCURRENCES: usize = 3;
}

impl Rule for RepeatedTaskSequence {
    fn id(&self) -> &'static str {
        "duplication.repeated_task_sequence"
    }
    fn group(&self) -> Group {
        Group::Duplication
    }
    fn level(&self) -> Level {
        Level::Warn
    }
    fn scope(&self) -> Scope {
        Scope::Set
    }
    fn summary(&self) -> &'static str {
        "the same run of two or more steps appears three or more times across the set"
    }
    fn explain(&self) -> &'static str {
        "A run of at least two consecutive steps that is byte-identical — ids and names \
         aside — in at least three places. Reported once per distinct run, for the longest \
         run at those places, with every location.\n\n\
         Proof: structural identity of the steps' `function`, `condition`, `terminal` and \
         `continue_on_error` after `id` and `name` are stripped, recursively through groups.\n\n\
         Silent when: fewer than three occurrences, or a run shorter than two steps; any \
         run that includes a `use` step. The fact is certain; whether it should be a \
         fragment is the author's call — the message says what it found, and no more."
    }

    fn check(&self, cx: &Analysis<'_>, out: &mut Vec<Diagnostic>) {
        // window key → sites, for every window length.
        let mut windows: BTreeMap<Vec<String>, Vec<Site>> = BTreeMap::new();
        // (origin, list path) → the list's step keys, to test extensions.
        let mut keyed: BTreeMap<(String, String), Vec<Option<String>>> = BTreeMap::new();

        for def in cx.source.iter(Entity::Workflow) {
            let Some(tasks) = def.doc.get("tasks") else {
                continue;
            };
            let mut lists = Vec::new();
            step_lists(tasks, "tasks", &mut lists);
            for (list_path, steps) in lists {
                let keys: Vec<Option<String>> = steps
                    .iter()
                    .map(|s| (!is_use_step(s)).then(|| step_key(s)))
                    .collect();
                for start in 0..keys.len() {
                    for len in Self::MIN_TASKS..=keys.len() - start {
                        let Some(window) = keys[start..start + len]
                            .iter()
                            .cloned()
                            .collect::<Option<Vec<String>>>()
                        else {
                            break;
                        };
                        windows.entry(window).or_default().push(Site {
                            origin: def.origin.clone(),
                            path: format!("{list_path}[{start}]"),
                        });
                    }
                }
                keyed.insert((def.origin.clone(), list_path), keys);
            }
        }

        let repeated: BTreeMap<&Vec<String>, &Vec<Site>> = windows
            .iter()
            .filter(|(_, sites)| sites.len() >= Self::MIN_OCCURRENCES)
            .collect();
        for (window, sites) in &repeated {
            // Maximal: not every occurrence extends to a longer repeated run.
            let extends = |direction: i64| {
                let mut extended: Option<Vec<String>> = None;
                for site in *sites {
                    let (list_path, index) = split_index(&site.path);
                    let Some(keys) = keyed.get(&(site.origin.clone(), list_path)) else {
                        return false;
                    };
                    let (from, to) = if direction < 0 {
                        (index as i64 - 1, index as i64 + window.len() as i64)
                    } else {
                        (index as i64, index as i64 + window.len() as i64 + 1)
                    };
                    if from < 0 || to as usize > keys.len() {
                        return false;
                    }
                    let Some(longer) = keys[from as usize..to as usize]
                        .iter()
                        .cloned()
                        .collect::<Option<Vec<String>>>()
                    else {
                        return false;
                    };
                    match &extended {
                        Some(e) if *e != longer => return false,
                        _ => extended = Some(longer),
                    }
                }
                extended.is_some_and(|e| repeated.contains_key(&e))
            };
            if extends(-1) || extends(1) {
                continue;
            }
            let first = &sites[0];
            out.push(
                Diagnostic::at(
                    self,
                    cx,
                    "the set",
                    &first.origin,
                    Some(&first.path),
                    format!(
                        "this run of {} steps appears {} times: {}",
                        window.len(),
                        sites.len(),
                        Site::render(sites)
                    ),
                )
                .with_remedy(
                    "if these are one thing, declare it once under `fragments` and replace each \
                     occurrence with a `use` step",
                ),
            );
        }
    }
}

/// `tasks[2].tasks[4]` → (`tasks[2].tasks`, 4).
fn split_index(path: &str) -> (String, usize) {
    let open = path.rfind('[').unwrap_or(0);
    let index = path[open + 1..path.len() - 1].parse().unwrap_or(0);
    (path[..open].to_string(), index)
}

// ============================================================
// duplication.repeated_value
// ============================================================

pub struct RepeatedValue;

impl RepeatedValue {
    pub const MIN_KEYS: usize = 2;
    pub const MIN_OCCURRENCES: usize = 3;
}

impl Rule for RepeatedValue {
    fn id(&self) -> &'static str {
        "duplication.repeated_value"
    }
    fn group(&self) -> Group {
        Group::Duplication
    }
    fn level(&self) -> Level {
        Level::Warn
    }
    fn scope(&self) -> Scope {
        Scope::Set
    }
    fn summary(&self) -> &'static str {
        "the same object literal appears three or more times across the set"
    }
    fn explain(&self) -> &'static str {
        "An object with at least two keys that is byte-identical in at least three places \
         — a connector target, an error body, a header block. A shared `constants` or \
         `errors` entry with `$from` at each site says it once; the splicer merges the \
         entry's fields and lets siblings override.\n\n\
         Proof: canonical-JSON identity of the object.\n\n\
         Silent when: fewer than three occurrences, or fewer than two keys; the object is \
         structure rather than data — a step, a `function` header, a mapping or rule entry, \
         an operator node, an entity root; it is the input of an engine built-in \
         (`parse_json`'s `{\"source\", \"target\"}` is the idiom, not a value); it is a \
         `use` step's `with` block (arguments, repeated because the call is); it contains a \
         `$from` at any depth (already shared); every occurrence sits inside a larger object \
         that is itself reported."
    }

    fn check(&self, cx: &Analysis<'_>, out: &mut Vec<Diagnostic>) {
        let mut seen: BTreeMap<String, (Value, Vec<Site>)> = BTreeMap::new();
        for def in &cx.source.definitions {
            let mut hits = Vec::new();
            collect_values(&def.doc, "", None, false, &mut hits);
            for (path, value) in hits {
                seen.entry(value_key(value))
                    .or_insert_with(|| (value.clone(), Vec::new()))
                    .1
                    .push(Site {
                        origin: def.origin.clone(),
                        path,
                    });
            }
        }
        let repeated: Vec<(&Value, &Vec<Site>)> = seen
            .values()
            .filter(|(_, sites)| sites.len() >= Self::MIN_OCCURRENCES)
            .map(|(v, s)| (v, s))
            .collect();
        for (value, sites) in &repeated {
            let dominated = repeated.iter().any(|(other, other_sites)| {
                !std::ptr::eq(*other, *value)
                    && sites.iter().all(|s| {
                        other_sites.iter().any(|o| {
                            o.origin == s.origin
                                && s.path.len() > o.path.len()
                                && s.path.starts_with(&o.path)
                                && s.path[o.path.len()..].starts_with(['.', '['])
                        })
                    })
            });
            if dominated {
                continue;
            }
            let first = &sites[0];
            let suggested = suggest_name(value);
            out.push(
                Diagnostic::at(
                    self,
                    cx,
                    "the set",
                    &first.origin,
                    Some(&first.path),
                    format!(
                        "this object appears {} times: {}",
                        sites.len(),
                        Site::render(sites)
                    ),
                )
                .with_remedy(format!(
                    "declare it once, e.g. `{suggested}`, and write {{ \"$from\": \"{suggested}\" }} at \
                     each site — siblings override the spliced fields"
                )),
            );
        }
    }
}

/// Data objects worth sharing, with their paths. Structure — steps, function
/// headers, mapping and rule entries, operator nodes — is skipped, and so is
/// anything already spliced from a shared value, a `with` block (arguments
/// to a fragment, repeated because the call is), and the input of an engine
/// built-in (`{"source": "payload", "target": …}` is the documented idiom,
/// not a value).
fn collect_values<'a>(
    value: &'a Value,
    path: &str,
    parent_key: Option<&str>,
    builtin_input: bool,
    out: &mut Vec<(String, &'a Value)>,
) {
    match value {
        Value::Object(map) => {
            let keys: BTreeSet<&str> = map.keys().map(String::as_str).collect();
            let is_step = keys.contains("id")
                && (keys.contains("function") || keys.contains("tasks") || keys.contains("use"));
            let is_function = parent_key == Some("function") && keys.contains("name");
            let is_entry = keys == BTreeSet::from(["path", "logic"])
                || keys == BTreeSet::from(["logic", "message"]);
            let structural = path.is_empty()
                || is_step
                || is_function
                || is_entry
                || builtin_input
                || parent_key == Some("with")
                || contains_from(value)
                || map.len() < RepeatedValue::MIN_KEYS;
            if !structural {
                out.push((path.to_string(), value));
            }
            let builtin = is_function
                && map.get("name").and_then(Value::as_str).is_some_and(|f| {
                    crate::definitions::fmt::style::BUILTIN_INPUT_KEYS
                        .iter()
                        .any(|(name, _)| *name == f)
                });
            for (k, v) in map {
                let at = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                collect_values(v, &at, Some(k), builtin && k == "input", out);
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                collect_values(v, &format!("{path}[{i}]"), parent_key, false, out);
            }
        }
        _ => {}
    }
}

/// Whether a `$from` appears anywhere inside `value`: the object is already
/// partly shared, and a constant built on a constant is not a suggestion
/// this rule can vouch for.
fn contains_from(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.contains_key("$from") || map.values().any(contains_from),
        Value::Array(items) => items.iter().any(contains_from),
        _ => false,
    }
}

/// `errors.NOT_FOUND` for something shaped like an error body, otherwise
/// `constants.<first two keys>`. A placeholder the author renames.
fn suggest_name(value: &Value) -> String {
    let map = value.as_object().expect("objects only");
    let text = map
        .get("body")
        .or_else(|| map.get("message"))
        .and_then(Value::as_str);
    if map.contains_key("status") && text.is_some() {
        let slug: String = text
            .unwrap_or("")
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect::<String>()
            .split('_')
            .filter(|s| !s.is_empty())
            .take(3)
            .collect::<Vec<_>>()
            .join("_");
        if !slug.is_empty() {
            return format!("errors.{slug}");
        }
    }
    format!(
        "constants.{}",
        map.keys().take(2).cloned().collect::<Vec<_>>().join("_")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fragment(tasks: Value, params: &[(&str, Option<Value>)]) -> Fragment {
        Fragment {
            params: params
                .iter()
                .map(|(k, d)| (k.to_string(), d.clone()))
                .collect(),
            tasks: tasks.as_array().expect("array").clone(),
        }
    }

    #[test]
    fn a_window_matches_a_fragment_up_to_ids_and_params() {
        let frag = fragment(
            json!([{"id": "deny", "name": "Deny", "function": {"name": "map", "input": {"mappings": [
                {"path": "data.msg", "logic": {"$param": "msg"}}]}}}]),
            &[("msg", Some(json!("denied")))],
        );
        let window = [
            json!({"id": "x", "name": "Deny", "function": {"name": "map", "input": {"mappings": [
            {"path": "data.msg", "logic": "no"}]}}}),
        ];
        assert_eq!(
            matches_fragment(&window, &frag),
            Some(Map::from_iter([("msg".to_string(), json!("no"))]))
        );
        let default = [
            json!({"id": "x", "name": "Deny", "function": {"name": "map", "input": {"mappings": [
            {"path": "data.msg", "logic": "denied"}]}}}),
        ];
        assert_eq!(
            matches_fragment(&default, &frag),
            Some(Map::new()),
            "a default needs no `with`"
        );
        let other_name = [
            json!({"id": "x", "name": "Refuse", "function": {"name": "map", "input": {"mappings": [
            {"path": "data.msg", "logic": "no"}]}}}),
        ];
        assert_eq!(
            matches_fragment(&other_name, &frag),
            None,
            "names are content"
        );
    }

    #[test]
    fn a_hole_binds_consistently() {
        let frag = fragment(
            json!([{"id": "a", "name": "A", "function": {"name": "log", "input": {"message": {"$param": "m"}, "level": {"$param": "m"}}}}]),
            &[("m", None)],
        );
        let same = [
            json!({"id": "z", "name": "A", "function": {"name": "log", "input": {"message": "x", "level": "x"}}}),
        ];
        assert!(matches_fragment(&same, &frag).is_some());
        let differ = [
            json!({"id": "z", "name": "A", "function": {"name": "log", "input": {"message": "x", "level": "y"}}}),
        ];
        assert!(matches_fragment(&differ, &frag).is_none());
    }

    #[test]
    fn structural_objects_are_not_values() {
        let doc = json!({"name": "w", "tasks": [
            {"id": "t", "name": "T", "function": {"name": "mongo_read", "input": {"connector": "m", "database": "app"}}},
            {"id": "u", "name": "U", "function": {"name": "map", "input": {"mappings": [{"path": "data.x", "logic": {"status": 404, "body": "nope"}}]}}}
        ]});
        let mut hits = Vec::new();
        collect_values(&doc, "", None, false, &mut hits);
        let paths: Vec<&str> = hits.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            [
                "tasks[0].function.input",
                "tasks[1].function.input.mappings[0].logic"
            ],
            "a map's input has one key and is not a value"
        );
        let idiom = json!({"name": "w", "tasks": [
            {"id": "p", "name": "P", "function": {"name": "parse_json", "input": {"source": "payload", "target": "req"}}},
            {"id": "u", "name": "U", "use": "guard", "with": {"status": 400, "out": "x"}},
            {"id": "c", "name": "C", "function": {"name": "http_call", "input": {"connector": "c", "body": {"$from": "constants.x", "extra": 1}}}}
        ]});
        let mut hits = Vec::new();
        collect_values(&idiom, "", None, false, &mut hits);
        assert!(
            hits.is_empty(),
            "a built-in's input, a `with` block and a `$from`-bearing object are not values: {:?}",
            hits.iter().map(|(p, _)| p).collect::<Vec<_>>()
        );
    }

    #[test]
    fn names_are_derived_from_the_shape() {
        assert_eq!(
            suggest_name(&json!({"status": 404, "body": "User Not Found !"})),
            "errors.USER_NOT_FOUND"
        );
        assert_eq!(
            suggest_name(&json!({"connector": "m", "database": "app"})),
            "constants.connector_database"
        );
    }
}
