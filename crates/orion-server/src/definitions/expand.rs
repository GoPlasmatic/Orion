//! The scoped walk behind the `shared.fragments` pass (#333): `use` steps,
//! `$use` value fragments, `$each` repetition, `$param` and `{{name}}`.
//!
//! One walk because each can contain the others — a fragment may use a
//! fragment, an `$each` may repeat a `use` step, a value fragment's body may
//! hold an `$each` — and splitting them into ordered passes would leave
//! whichever ran first unable to see what the later ones produce.
//!
//! ## Scope
//!
//! Lexical and closed. A fragment body sees only its own parameters: the
//! arguments were expanded in the caller's scope before they were bound, so
//! a caller's `$each` variable reaches a fragment only by being passed
//! through `with`. An `$each`'s `do` sees everything around it plus its own
//! binding, and may not rebind a name already in scope.
//!
//! ## What a binding is
//!
//! A value, already expanded, bound once. `{"$param": "p"}` is replaced by a
//! clone of it, keeping its type; `"{{p}}"` inside a string by its text — a
//! string as is, a number or a boolean as JSON. An argument written as
//! `{"$from": "constants.x"}` is looked up as it is bound, so a constant can
//! be a list an `$each` repeats over, or a scalar a `{{name}}` interpolates.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use super::compile::Cx;
use super::diagnostic::Diagnostic;
use super::provenance::{SourceMap, Via, trail_suffix};
use super::shared::{Fragment, FragmentBody, SharedDefinitions, reanchor_sql};

/// How deep fragments, `$each` and constants may nest inside one another.
pub const MAX_EXPANSION_DEPTH: usize = 16;
/// How many copies every `$each` in one document may produce, together.
pub const MAX_EACH_COPIES: usize = 4096;

/// A step that includes a task fragment: a string `use`. Only a step-list
/// element is one — a `use` key anywhere else is an ordinary field.
pub(super) fn as_use_step(step: &Value) -> Option<&str> {
    step.get("use")?.as_str()
}

/// An object that splices a value fragment: a string `$use`.
pub(super) fn as_value_use(obj: &Map<String, Value>) -> Option<&str> {
    obj.get("$use")?.as_str()
}

/// An object that repeats: an object-valued `$each`.
pub(super) fn as_each(obj: &Map<String, Value>) -> Option<&Map<String, Value>> {
    obj.get("$each")?.as_object()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// An element of a step list: a `use` step and a task group are steps.
    Steps,
    Value,
}

#[derive(Clone, Default)]
struct Scope {
    bindings: BTreeMap<String, Value>,
    /// Inside a fragment body or an `$each` — where an unbound `$param` is a
    /// typo rather than payload that happens to look like one.
    open: bool,
    /// How this scope was entered, outermost first.
    trail: Vec<Via>,
    /// The fragments being expanded around this point, for cycle detection.
    fragments: Vec<String>,
    /// Parameters the fragment declares that the call site left unbound —
    /// already reported, so a `$param` naming one is not a second finding.
    missing: BTreeSet<String>,
}

/// Where a node was authored: a file, the coordinate in it, and how it got
/// to where it lands.
#[derive(Clone)]
struct Src {
    file: String,
    path: Option<String>,
    via: Vec<Via>,
}

impl Src {
    fn key(&self, key: &str) -> Src {
        Src {
            file: self.file.clone(),
            path: self.path.as_deref().map(|p| join(p, key)),
            via: self.via.clone(),
        }
    }

    fn index(&self, i: usize) -> Src {
        Src {
            file: self.file.clone(),
            path: self.path.as_deref().map(|p| format!("{p}[{i}]")),
            via: self.via.clone(),
        }
    }
}

/// A node's compiled coordinate and its source.
#[derive(Clone)]
struct Coord {
    compiled: String,
    src: Src,
}

impl Coord {
    fn key(&self, key: &str) -> Coord {
        Coord {
            compiled: join(&self.compiled, key),
            src: self.src.key(key),
        }
    }
}

fn join(base: &str, key: &str) -> String {
    if base.is_empty() {
        key.to_string()
    } else {
        format!("{base}.{key}")
    }
}

/// One document's expansion.
pub(super) struct Expander<'a, 'o> {
    shared: &'a SharedDefinitions,
    cx: &'a Cx<'a>,
    findings: &'o mut Vec<Diagnostic>,
    map: &'o mut SourceMap,
    /// `$each` copies made so far in this document.
    copies: usize,
    /// While above zero, nothing is recorded in `map`: arguments are walked
    /// at coordinates that do not exist in the compiled document.
    muted: usize,
}

impl<'a, 'o> Expander<'a, 'o> {
    pub(super) fn new(
        cx: &'a Cx<'a>,
        findings: &'o mut Vec<Diagnostic>,
        map: &'o mut SourceMap,
    ) -> Self {
        Self {
            shared: cx.shared,
            cx,
            findings,
            map,
            copies: 0,
            muted: 0,
        }
    }

    /// Expand a whole authored document: its top-level `tasks` as a step
    /// list, everything else — a condition, a loop, a connector's or a
    /// channel's config — as values.
    pub(super) fn document(&mut self, doc: &mut Value) {
        let at = Coord {
            compiled: String::new(),
            src: Src {
                file: self.cx.origin.to_string(),
                path: Some(String::new()),
                via: Vec::new(),
            },
        };
        let scope = Scope::default();
        let Value::Object(members) = doc else {
            self.value(doc, &scope, &at);
            return;
        };
        for (key, member) in members.iter_mut() {
            let child = at.key(key);
            match member {
                Value::Array(items) if key == "tasks" => {
                    *items = self.list(std::mem::take(items), Mode::Steps, &scope, &child);
                }
                _ => self.value(member, &scope, &child),
            }
        }
    }

    /// Expand a value with nothing in scope — a shared constant as the
    /// catalog grounds it, at its own file.
    pub(super) fn closed_value(&mut self, value: &mut Value, path: &str) {
        let at = Coord {
            compiled: path.to_string(),
            src: Src {
                file: self.cx.origin.to_string(),
                path: Some(path.to_string()),
                via: Vec::new(),
            },
        };
        self.muted += 1;
        self.value(value, &Scope::default(), &at);
        self.muted -= 1;
    }

    fn list(&mut self, items: Vec<Value>, mode: Mode, scope: &Scope, at: &Coord) -> Vec<Value> {
        let mut out = Vec::with_capacity(items.len());
        for (i, item) in items.into_iter().enumerate() {
            self.element(item, mode, scope, &at.compiled, at.src.index(i), &mut out);
        }
        out
    }

    /// One element of the array at `list`, expanded into `out` — nothing,
    /// one element, or many.
    fn element(
        &mut self,
        item: Value,
        mode: Mode,
        scope: &Scope,
        list: &str,
        src: Src,
        out: &mut Vec<Value>,
    ) {
        if let Value::Object(obj) = &item {
            if as_each(obj).is_some() {
                return self.each(item, mode, scope, list, src, out);
            }
            if mode == Mode::Steps {
                if let Some(name) = as_use_step(&item).map(str::to_string) {
                    return self.use_step(item, &name, scope, list, out);
                }
                if as_value_use(obj).is_some() {
                    return self.step_use(item, scope, list, src, out);
                }
            }
        }
        let at = Coord {
            compiled: format!("{list}[{}]", out.len()),
            src,
        };
        let mut item = item;
        if mode == Mode::Steps && crate::engine::is_group(&item) {
            self.group(&mut item, scope, &at);
        } else {
            self.value(&mut item, scope, &at);
        }
        self.note(&at);
        out.push(item);
    }

    /// A task group: its `tasks` a step list, every other member a value.
    fn group(&mut self, group: &mut Value, scope: &Scope, at: &Coord) {
        let Value::Object(members) = group else {
            return;
        };
        for (key, member) in members.iter_mut() {
            let child = at.key(key);
            match member {
                Value::Array(items) if key == "tasks" => {
                    *items = self.list(std::mem::take(items), Mode::Steps, scope, &child);
                }
                _ => self.value(member, scope, &child),
            }
        }
    }

    fn value(&mut self, value: &mut Value, scope: &Scope, at: &Coord) {
        match value {
            Value::String(text) => {
                if let Some(interpolated) = self.interpolate(text, scope) {
                    *text = interpolated;
                }
            }
            Value::Array(items) => {
                *items = self.list(std::mem::take(items), Mode::Value, scope, at);
            }
            Value::Object(map) => {
                if map.len() == 1
                    && let Some(name) = map.get("$param").and_then(Value::as_str)
                {
                    match scope.bindings.get(name) {
                        Some(bound) => *value = bound.clone(),
                        None if scope.open && !scope.missing.contains(name) => {
                            self.findings.push(Diagnostic::warning(
                                "shared.param_unbound",
                                self.entity(scope, "value"),
                                format!(
                                    "{{\"$param\": \"{name}\"}} names no parameter in scope, so it \
                                 is left as it is — a typo writes this object into the data"
                                ),
                            ))
                        }
                        None => {}
                    }
                    return;
                }
                if as_each(map).is_some() {
                    self.findings.push(Diagnostic::error(
                        "shared.each_position",
                        self.entity(scope, "value"),
                        format!(
                            "`$each` must be an element of an array (at '{}')",
                            at.compiled
                        ),
                    ));
                    *value = Value::Null;
                    return;
                }
                if as_value_use(map).is_some() {
                    let map = std::mem::take(map);
                    *value = self.value_use(map, scope, at);
                    return;
                }
                for (key, member) in map.iter_mut() {
                    let child = at.key(key);
                    self.value(member, scope, &child);
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------
    // use / $use
    // ------------------------------------------------------------

    /// A `use` step: the task fragment's steps, expanded in a scope holding
    /// only its parameters, with every id they carry prefixed by the call
    /// site — `{instance}.{id}`, flat at every depth.
    fn use_step(
        &mut self,
        step: Value,
        name: &str,
        scope: &Scope,
        list: &str,
        out: &mut Vec<Value>,
    ) {
        let name = self
            .interpolate(name, scope)
            .unwrap_or_else(|| name.to_string());
        let instance = step
            .get("id")
            .and_then(Value::as_str)
            .map(|id| {
                self.interpolate(id, scope)
                    .unwrap_or_else(|| id.to_string())
            })
            .unwrap_or_else(|| name.clone());
        let entity = self.entity(scope, &format!("task '{instance}'"));
        let Some(fragment) = self.fragment(&name, &entity, "task", scope) else {
            return;
        };
        let FragmentBody::Tasks(body) = &fragment.body else {
            self.findings.push(Diagnostic::error(
                "shared.fragment_kind",
                entity,
                format!("fragment '{name}' is a value fragment — splice it with `$use`, not `use`"),
            ));
            return;
        };
        let with = step.get("with").cloned();
        let args = self.bind(fragment, with, &name, &entity, scope);
        let inner = enter(
            scope,
            fragment,
            args,
            Via::Use {
                fragment: name.clone(),
            },
        );
        let start = out.len();
        for (k, member) in body.iter().enumerate() {
            let mut member = member.clone();
            reanchor_sql(&mut member, &fragment.origin, self.cx.base_dir);
            let at = Src {
                file: fragment.origin.clone(),
                path: Some(format!("fragments.{name}.tasks[{k}]")),
                via: inner.trail.clone(),
            };
            self.element(member, Mode::Steps, &inner, list, at, out);
        }
        for member in &mut out[start..] {
            prefix_ids(member, &instance);
        }
    }

    /// A `$use` in a step list: the value fragment's body expanded as a step
    /// in its own scope, the call site's other keys merged over it.
    fn step_use(&mut self, step: Value, scope: &Scope, list: &str, src: Src, out: &mut Vec<Value>) {
        let Value::Object(mut map) = step else {
            return;
        };
        let raw = map
            .remove("$use")
            .and_then(|v| v.as_str().map(str::to_string));
        let with = map.remove("with");
        let name = raw
            .as_deref()
            .map(|n| self.interpolate(n, scope).unwrap_or_else(|| n.to_string()))
            .unwrap_or_default();
        let entity = self.entity(scope, &format!("step `$use: {name}`"));
        let resolved = self
            .fragment(&name, &entity, "value", scope)
            .and_then(|fragment| match &fragment.body {
                FragmentBody::Value(body) => Some((fragment, body)),
                FragmentBody::Tasks(_) => {
                    self.findings.push(Diagnostic::error(
                        "shared.fragment_kind",
                        entity.clone(),
                        format!(
                            "fragment '{name}' is a task fragment — include it with a `use` step, \
                             not `$use`"
                        ),
                    ));
                    None
                }
            });
        let Some((fragment, body)) = resolved else {
            // The siblings stand, as around an unresolved `$from`.
            if !map.is_empty() {
                self.element(Value::Object(map), Mode::Steps, scope, list, src, out);
            }
            return;
        };
        let args = self.bind(fragment, with, &name, &entity, scope);
        let inner = enter(
            scope,
            fragment,
            args,
            Via::Use {
                fragment: name.clone(),
            },
        );
        let mut body = body.clone();
        reanchor_sql(&mut body, &fragment.origin, self.cx.base_dir);
        let start = out.len();
        let at = Src {
            file: fragment.origin.clone(),
            path: Some(format!("fragments.{name}.value")),
            via: inner.trail.clone(),
        };
        self.element(body, Mode::Steps, &inner, list, at, out);
        if map.is_empty() {
            return;
        }
        if out.len() != start + 1 || !out[start].is_object() {
            self.findings.push(Diagnostic::error(
                "shared.fragment_kind",
                entity,
                format!(
                    "value fragment '{name}' must produce exactly one step when the call site \
                     adds keys of its own"
                ),
            ));
            return;
        }
        // Siblings win, walked in the caller's scope where they were typed.
        let here = Coord {
            compiled: format!("{list}[{start}]"),
            src,
        };
        for (key, mut member) in map {
            let child = here.key(&key);
            self.value(&mut member, scope, &child);
            self.note(&child);
            if let Some(step) = out[start].as_object_mut() {
                step.insert(key, member);
            }
        }
    }

    /// A `$use` as a value: the body expanded in the fragment's scope, then
    /// spliced — an object merges into the call site with **siblings
    /// winning**; anything else replaces a call site with no siblings, and
    /// is dropped beside siblings, as a `$from` is.
    fn value_use(&mut self, mut map: Map<String, Value>, scope: &Scope, at: &Coord) -> Value {
        let raw = map
            .remove("$use")
            .and_then(|v| v.as_str().map(str::to_string));
        let with = map.remove("with");
        for (key, member) in map.iter_mut() {
            let child = at.key(key);
            self.value(member, scope, &child);
        }
        let name = raw
            .as_deref()
            .map(|n| self.interpolate(n, scope).unwrap_or_else(|| n.to_string()))
            .unwrap_or_default();
        let entity = self.entity(scope, &format!("value `$use: {name}`"));
        let Some(fragment) = self.fragment(&name, &entity, "value", scope) else {
            return Value::Object(map);
        };
        let FragmentBody::Value(body) = &fragment.body else {
            self.findings.push(Diagnostic::error(
                "shared.fragment_kind",
                entity,
                format!(
                    "fragment '{name}' is a task fragment — include it with a `use` step, not \
                     `$use`"
                ),
            ));
            return Value::Object(map);
        };
        let args = self.bind(fragment, with, &name, &entity, scope);
        let inner = enter(
            scope,
            fragment,
            args,
            Via::Use {
                fragment: name.clone(),
            },
        );
        let mut body = body.clone();
        reanchor_sql(&mut body, &fragment.origin, self.cx.base_dir);
        let body_at = Coord {
            compiled: at.compiled.clone(),
            src: Src {
                file: fragment.origin.clone(),
                path: Some(format!("fragments.{name}.value")),
                via: inner.trail.clone(),
            },
        };
        self.value(&mut body, &inner, &body_at);
        match body {
            Value::Object(fields) => {
                for (key, member) in fields {
                    if !map.contains_key(&key) {
                        self.note(&body_at.key(&key));
                        map.insert(key, member);
                    }
                }
                Value::Object(map)
            }
            other if map.is_empty() => {
                self.note(&body_at);
                other
            }
            _ => Value::Object(map),
        }
    }

    /// The fragment `name`, or a finding: undefined, a cycle, or too deep.
    fn fragment(
        &mut self,
        name: &str,
        entity: &str,
        wanted: &str,
        scope: &Scope,
    ) -> Option<&'a Fragment> {
        let Some(fragment) = self.shared.fragments.get(name) else {
            self.findings.push(Diagnostic::error(
                "closure.fragment",
                entity,
                format!("{wanted} fragment '{name}' is not defined in the set"),
            ));
            return None;
        };
        if scope.fragments.iter().any(|f| f == name) {
            let chain: Vec<String> = scope
                .fragments
                .iter()
                .chain(std::iter::once(&name.to_string()))
                .map(|f| format!("'{f}'"))
                .collect();
            self.findings.push(Diagnostic::error(
                "shared.cycle",
                entity,
                format!("fragment '{name}' includes itself: {}", chain.join(" → ")),
            ));
            return None;
        }
        if scope.trail.len() >= MAX_EXPANSION_DEPTH {
            self.findings.push(Diagnostic::error(
                "shared.depth",
                entity,
                format!(
                    "fragments and `$each` nest more than {MAX_EXPANSION_DEPTH} deep here: {}",
                    trail_suffix(&scope.trail)
                ),
            ));
            return None;
        }
        Some(fragment)
    }

    /// A call site's arguments: `with` expanded in the caller's scope,
    /// declared defaults for the rest, each `$from` looked up as it binds.
    /// An unknown argument and an unsatisfied parameter are both findings.
    fn bind(
        &mut self,
        fragment: &Fragment,
        with: Option<Value>,
        name: &str,
        entity: &str,
        scope: &Scope,
    ) -> BTreeMap<String, Value> {
        let muted = Coord {
            compiled: String::new(),
            src: Src {
                file: String::new(),
                path: None,
                via: Vec::new(),
            },
        };
        let mut supplied = match with {
            Some(Value::Object(map)) => map,
            _ => Map::new(),
        };
        let declared: BTreeSet<&String> = fragment.params.keys().collect();
        for key in supplied.keys() {
            if !declared.contains(key) {
                self.findings.push(Diagnostic::error(
                    "shared.fragment_param",
                    entity,
                    format!("fragment '{name}' declares no parameter '{key}'"),
                ));
            }
        }
        let mut args = BTreeMap::new();
        self.muted += 1;
        for (param, default) in &fragment.params {
            let value = match supplied.remove(param) {
                Some(mut value) => {
                    self.value(&mut value, scope, &muted);
                    Some(value)
                }
                // A default belongs to the fragment, not the caller: it sees
                // nothing the call site has bound.
                None => default.clone().map(|mut value| {
                    self.value(&mut value, &Scope::default(), &muted);
                    value
                }),
            };
            match value {
                Some(mut value) => {
                    self.lookup_from(&mut value);
                    args.insert(param.clone(), value);
                }
                None => self.findings.push(Diagnostic::error(
                    "shared.fragment_param",
                    entity,
                    format!("fragment '{name}' requires parameter '{param}', which has no default"),
                )),
            }
        }
        self.muted -= 1;
        args
    }

    /// A binding written as `{"$from": "ns.key"}` is the constant itself, so
    /// an `$each` can repeat over it and `{{name}}` interpolate it.
    fn lookup_from(&self, value: &mut Value) {
        let Some(map) = value.as_object() else {
            return;
        };
        if map.len() != 1 {
            return;
        }
        let Some(path) = map.get("$from").and_then(Value::as_str) else {
            return;
        };
        if let Some(target) = self.shared.lookup(path) {
            let mut target = target.clone();
            if let Some(declared_in) = path.split_once('.').and_then(|(ns, key)| {
                self.shared
                    .value_origins
                    .get(&(ns.to_string(), key.to_string()))
            }) {
                reanchor_sql(&mut target, declared_in, self.cx.base_dir);
            }
            *value = target;
        }
    }

    // ------------------------------------------------------------
    // $each
    // ------------------------------------------------------------

    /// `{"$each": {"p": [..]}, "do": <element>}`: one copy of `do` per
    /// value, each expanded with `p` bound, in the mode of the array the
    /// element sits in — a `do` that is itself an `$each` is a product, in
    /// written order.
    fn each(
        &mut self,
        item: Value,
        mode: Mode,
        scope: &Scope,
        list: &str,
        src: Src,
        out: &mut Vec<Value>,
    ) {
        let entity = self.entity(scope, "`$each`");
        let Value::Object(mut obj) = item else {
            return;
        };
        let (Some(Value::Object(binding)), Some(body)) = (obj.remove("$each"), obj.remove("do"))
        else {
            self.findings.push(Diagnostic::error(
                "shared.each_shape",
                entity,
                "an `$each` element holds exactly `$each` (one binding) and `do` (the element \
                 to repeat)",
            ));
            return;
        };
        if !obj.is_empty() || binding.len() != 1 {
            self.findings.push(Diagnostic::error(
                "shared.each_shape",
                entity,
                if binding.len() != 1 {
                    "`$each` binds exactly one name — nest one `$each` inside another's `do` for a \
                     product"
                        .to_string()
                } else {
                    format!(
                        "an `$each` element holds only `$each` and `do`, not '{}'",
                        obj.keys().cloned().collect::<Vec<_>>().join("', '")
                    )
                },
            ));
            return;
        }
        let Some((name, mut values)) = binding.into_iter().next() else {
            return;
        };
        if scope.bindings.contains_key(&name) {
            self.findings.push(Diagnostic::error(
                "shared.binding_shadowed",
                entity,
                format!("'{name}' is already bound here — choose another name"),
            ));
            return;
        }
        let muted = Coord {
            compiled: String::new(),
            src: Src {
                file: String::new(),
                path: None,
                via: Vec::new(),
            },
        };
        self.muted += 1;
        self.value(&mut values, scope, &muted);
        self.muted -= 1;
        self.lookup_from(&mut values);
        let Value::Array(values) = values else {
            self.findings.push(Diagnostic::error(
                "shared.each_list",
                entity,
                format!(
                    "`$each.{name}` must be an array: a literal, a `$from` constant or a \
                     `$param`"
                ),
            ));
            return;
        };
        self.copies += values.len();
        if self.copies > MAX_EACH_COPIES {
            self.findings.push(Diagnostic::error(
                "shared.each_limit",
                entity,
                format!("`$each` would make more than {MAX_EACH_COPIES} copies in one document"),
            ));
            return;
        }
        if scope.trail.len() >= MAX_EXPANSION_DEPTH {
            self.findings.push(Diagnostic::error(
                "shared.depth",
                entity,
                format!(
                    "fragments and `$each` nest more than {MAX_EXPANSION_DEPTH} deep here: {}",
                    trail_suffix(&scope.trail)
                ),
            ));
            return;
        }
        for value in values {
            let via = Via::Each {
                name: name.clone(),
                value: render_binding(&value),
            };
            let mut bindings = scope.bindings.clone();
            bindings.insert(name.clone(), value);
            let mut inner = scope.clone();
            inner.bindings = bindings;
            inner.open = true;
            inner.trail.push(via.clone());
            let mut at = src.key("do");
            at.via.push(via);
            self.element(body.clone(), mode, &inner, list, at, out);
        }
    }

    // ------------------------------------------------------------
    // {{name}}
    // ------------------------------------------------------------

    /// `text` with every `{{name}}` whose name is bound replaced by the
    /// binding's text, or `None` when nothing was replaced. A placeholder
    /// naming nothing in scope is text — `'{{1,2}}'` is a PostgreSQL array
    /// literal, not a reference.
    fn interpolate(&mut self, text: &str, scope: &Scope) -> Option<String> {
        if scope.bindings.is_empty() || !text.contains("{{") {
            return None;
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        let mut changed = false;
        while let Some(start) = rest.find("{{") {
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else {
                break;
            };
            let name = &after[..end];
            match scope.bindings.get(name) {
                Some(bound) => {
                    out.push_str(&rest[..start]);
                    match bound {
                        Value::String(s) => out.push_str(s),
                        Value::Number(_) | Value::Bool(_) => out.push_str(&bound.to_string()),
                        _ => self.findings.push(Diagnostic::error(
                            "shared.interpolate_non_scalar",
                            self.entity(scope, "value"),
                            format!(
                                "'{{{{{name}}}}}' is bound to {}, which has no text — pass a \
                                 string, a number or a boolean, or use {{\"$param\": \"{name}\"}} \
                                 to insert the value itself",
                                kind_of(bound)
                            ),
                        )),
                    }
                    rest = &after[end + 2..];
                    changed = true;
                }
                None => {
                    out.push_str(&rest[..start + 2]);
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        changed.then_some(out)
    }

    // ------------------------------------------------------------

    /// How a finding names where it happened: the document, what, and the
    /// expansion trail around it.
    fn entity(&self, scope: &Scope, what: &str) -> String {
        let mut entity = format!("{} {what}", self.cx.origin);
        if !scope.trail.is_empty() {
            entity.push_str(&format!(" ({})", trail_suffix(&scope.trail)));
        }
        entity
    }

    /// Record where the node at `at` was authored, when that is not the
    /// same coordinate of this document.
    fn note(&mut self, at: &Coord) {
        if self.muted > 0 {
            return;
        }
        let own = at.src.via.is_empty()
            && at.src.file == self.cx.origin
            && at.src.path.as_deref() == Some(at.compiled.as_str());
        if own {
            return;
        }
        self.map.record(
            &at.compiled,
            &at.src.file,
            at.src.path.as_deref(),
            at.src.via.clone(),
        );
    }
}

/// The scope a fragment body sees: its own parameters and nothing else.
fn enter(scope: &Scope, fragment: &Fragment, bindings: BTreeMap<String, Value>, via: Via) -> Scope {
    let mut trail = scope.trail.clone();
    let mut fragments = scope.fragments.clone();
    if let Via::Use { fragment } = &via {
        fragments.push(fragment.clone());
    }
    trail.push(via);
    let missing = fragment
        .params
        .keys()
        .filter(|p| !bindings.contains_key(*p))
        .cloned()
        .collect();
    Scope {
        bindings,
        open: true,
        trail,
        fragments,
        missing,
    }
}

/// Prefix every id one fragment step contributes with the call-site id —
/// at **every** depth, flat: `{instance}.{id}`.
///
/// Flat rather than one segment per enclosing group, so ids stay short: a
/// step id is a metric label, a trace step id and a `metadata.progress`
/// key. A fragment used inside a fragment carries both call sites,
/// `outer.inner.id`, because the inner expansion is prefixed first.
fn prefix_ids(step: &mut Value, instance: &str) {
    if let Some(id) = step.get("id").and_then(Value::as_str) {
        step["id"] = Value::String(format!("{instance}.{id}"));
    }
    if crate::engine::is_group(step)
        && let Some(members) = step.get_mut("tasks").and_then(Value::as_array_mut)
    {
        for member in members {
            prefix_ids(member, instance);
        }
    }
}

/// A binding as a trail names it: `3`, `"fold"`.
fn render_binding(value: &Value) -> String {
    let text = value.to_string();
    if text.chars().count() > 40 {
        format!("{}…", text.chars().take(40).collect::<String>())
    } else {
        text
    }
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
        _ => "a scalar",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definitions::compile::{compile_with_map, residue};
    use serde_json::json;

    fn catalog(doc: Value) -> (SharedDefinitions, Vec<Diagnostic>) {
        let mut shared = SharedDefinitions::default();
        let mut findings = Vec::new();
        shared.merge(&doc, "shared.json", &mut findings);
        shared.finish(&mut findings);
        (shared, findings)
    }

    fn compiled(shared: &SharedDefinitions, mut doc: Value) -> (Value, Vec<Diagnostic>, SourceMap) {
        let mut findings = Vec::new();
        let mut map = SourceMap::default();
        compile_with_map(
            &mut doc,
            &Cx::detached(shared, "wf.json"),
            &mut findings,
            &mut map,
        );
        (doc, findings, map)
    }

    fn checks(findings: &[Diagnostic]) -> Vec<&str> {
        findings.iter().map(|f| f.check).collect()
    }

    /// The issue's first example: a value fragment with a parameter, used
    /// from inside a task fragment, the argument passed through.
    #[test]
    fn a_value_fragment_composes_inside_a_task_fragment() {
        let (shared, f) = catalog(json!({"fragments": {
            "wrote": {"params": {"slot": {}},
                      "value": {">": [{"var": "{{slot}}.rows_affected"}, 0]}},
            "halt-unless": {"params": {"slot": {}}, "tasks": [
                {"id": "halt", "name": "Halt unless {{slot}} wrote",
                 "condition": {"!": {"$use": "wrote", "with": {"slot": {"$param": "slot"}}}},
                 "terminal": true,
                 "function": {"name": "map", "input": {"mappings": []}}}]}}}));
        assert!(f.is_empty(), "{f:?}");
        let (doc, f, _) = compiled(
            &shared,
            json!({"tasks": [{"id": "fold", "use": "halt-unless", "with": {"slot": "temp_data.fold"}}]}),
        );
        assert!(f.is_empty(), "{f:?}");
        let step = &doc["tasks"][0];
        assert_eq!(step["id"], "fold.halt");
        assert_eq!(step["name"], "Halt unless temp_data.fold wrote");
        assert_eq!(
            step["condition"],
            json!({"!": {">": [{"var": "temp_data.fold.rows_affected"}, 0]}})
        );
        assert!(residue(&doc, "").is_empty());
    }

    /// The issue's second example: one task repeated per participant, the
    /// binding typed where `$param` places it and text where `{{p}}` does.
    #[test]
    fn each_unrolls_a_step_with_typed_and_interpolated_bindings() {
        let (shared, _) = catalog(json!({"constants": {"participants": [0, 1, 2, 3, 4, 5, 6, 7]}}));
        let (doc, f, _) = compiled(
            &shared,
            json!({"tasks": [
                {"id": "first", "name": "first", "function": {"name": "map", "input": {"mappings": []}}},
                {"$each": {"p": {"$from": "constants.participants"}},
                 "do": {"id": "infer{{p}}", "name": "infer {{p}}",
                        "function": {"name": "map", "input": {"mappings": [
                            {"path": "data.out{{p}}", "logic": {"val": ["seats", {"$param": "p"}]}}]}}}}
            ]}),
        );
        assert!(f.is_empty(), "{f:?}");
        let tasks = doc["tasks"].as_array().expect("tasks");
        assert_eq!(tasks.len(), 9);
        let ids: Vec<&str> = tasks.iter().filter_map(|t| t["id"].as_str()).collect();
        assert_eq!(
            ids[1..],
            [
                "infer0", "infer1", "infer2", "infer3", "infer4", "infer5", "infer6", "infer7"
            ]
        );
        let mapping = &tasks[4]["function"]["input"]["mappings"][0];
        assert_eq!(mapping["path"], "data.out3");
        assert_eq!(mapping["logic"], json!({"val": ["seats", 3]}));
        assert!(residue(&doc, "").is_empty());
    }

    #[test]
    fn a_value_fragment_splices_like_from() {
        let (shared, _) = catalog(json!({"fragments": {
            "db": {"value": {"connector": "orders-db", "database": "app"}},
            "zero": {"value": 0}}}));
        let (doc, f, _) = compiled(
            &shared,
            json!({"condition": {"$use": "zero"},
                   "config": {"$use": "db", "database": "override"}}),
        );
        assert!(f.is_empty(), "{f:?}");
        assert_eq!(
            doc["condition"],
            json!(0),
            "a lone scalar replaces the node"
        );
        assert_eq!(
            doc["config"],
            json!({"connector": "orders-db", "database": "override"}),
            "siblings win"
        );
    }

    #[test]
    fn each_in_an_argument_list_splices_in_order_and_nests_as_a_product() {
        let (shared, _) = catalog(json!({}));
        let (doc, f, _) = compiled(
            &shared,
            json!({"condition": {"cat": [
                {"$each": {"a": ["x", "y"]}, "do": {"$each": {"b": [1, 2]}, "do": "{{a}}{{b}}"}},
                {"$each": {"none": []}, "do": "never"}
            ]}}),
        );
        assert!(f.is_empty(), "{f:?}");
        assert_eq!(doc["condition"], json!({"cat": ["x1", "x2", "y1", "y2"]}));
    }

    #[test]
    fn a_fragment_sees_its_parameters_and_not_its_callers_bindings() {
        let (shared, _) = catalog(json!({"fragments": {
            "peek": {"tasks": [{"id": "t", "name": "{{p}}",
                                "function": {"name": "map", "input": {"mappings": []}}}]}}}));
        let (doc, _, _) = compiled(
            &shared,
            json!({"tasks": [{"$each": {"p": [1]}, "do": {"id": "u{{p}}", "use": "peek"}}]}),
        );
        assert_eq!(doc["tasks"][0]["id"], "u1.t");
        assert_eq!(
            doc["tasks"][0]["name"], "{{p}}",
            "the caller's `p` is not in scope"
        );
    }

    #[test]
    fn misuse_is_named() {
        let (shared, _) = catalog(json!({"fragments": {
            "steps": {"tasks": []},
            "val": {"value": 1},
            "obj": {"params": {"o": {}}, "value": "{{o}}"},
            "loose": {"tasks": [{"id": "t", "name": "t", "input": {"$param": "typo"},
                                 "function": {"name": "map", "input": {"mappings": []}}}]}}}));
        let (doc, f, _) = compiled(
            &shared,
            json!({"tasks": [
                {"id": "a", "use": "val"},
                {"id": "b", "use": "loose"},
                {"$each": {"p": [1]}, "do": {"$each": {"p": [2]}, "do": {"id": "x"}}},
                {"$each": {"p": "not-a-list"}, "do": {"id": "y"}},
                {"$each": {"p": [1], "q": [2]}, "do": {"id": "z"}}
            ],
            "condition": {"and": [{"$use": "steps"}, {"$use": "obj", "with": {"o": {"k": 1}}},
                                  {"$each": {"p": [1]}, "do": true}]},
            "config": {"$each": {"p": [1]}, "do": 1},
            "payload": {"$param": "outside-any-scope"},
            "sql": "SELECT '{{1,2}}'::int[]"}),
        );
        let seen = checks(&f);
        for expected in [
            "shared.fragment_kind",
            "shared.param_unbound",
            "shared.binding_shadowed",
            "shared.each_list",
            "shared.each_shape",
            "shared.interpolate_non_scalar",
            "shared.each_position",
        ] {
            assert!(seen.contains(&expected), "{expected} missing from {seen:?}");
        }
        assert_eq!(
            f.iter()
                .filter(|x| x.check == "shared.param_unbound")
                .count(),
            1,
            "a `$param` outside any scope may be payload: {f:?}"
        );
        assert_eq!(
            doc["sql"], "SELECT '{{1,2}}'::int[]",
            "nothing bound, nothing replaced"
        );
        assert!(doc["config"].is_null());
        assert!(residue(&doc, "").is_empty(), "{:?}", residue(&doc, ""));
    }

    #[test]
    fn each_is_bounded() {
        let (shared, _) = catalog(json!({}));
        let list: Vec<usize> = (0..=MAX_EACH_COPIES).collect();
        let (doc, f, _) = compiled(&shared, json!({"xs": [{"$each": {"p": list}, "do": 1}]}));
        assert_eq!(checks(&f), ["shared.each_limit"]);
        assert_eq!(doc["xs"], json!([]));
    }

    /// Constants ground once: a constant built from a constant or from a
    /// value fragment splices in one run, and a cycle is reported once.
    #[test]
    fn constants_compose_and_a_cycle_is_reported_once() {
        let (shared, f) = catalog(json!({
            "constants": {
                "db": {"connector": "orders-db"},
                "orders": {"$from": "constants.db", "collection": "orders"},
                "guard": {"$use": "positive", "with": {"field": "total"}},
                "a": {"$from": "constants.b"},
                "b": {"$from": "constants.a"}},
            "fragments": {"positive": {"params": {"field": {}}, "value": {">": [{"var": "{{field}}"}, 0]}}}}));
        assert_eq!(checks(&f), ["shared.cycle"], "{f:?}");
        let (doc, f, _) = compiled(
            &shared,
            json!({"x": {"$from": "constants.orders"}, "y": {"$from": "constants.guard"},
                   "z": {"$from": "constants.a"}}),
        );
        assert!(f.is_empty(), "the cycle is not reported again: {f:?}");
        assert_eq!(
            doc["x"],
            json!({"connector": "orders-db", "collection": "orders"})
        );
        assert_eq!(doc["y"], json!({">": [{"var": "total"}, 0]}));
        assert!(residue(&doc, "").is_empty());
    }

    /// A step an expansion placed is traced back to where it was written,
    /// and a step after it to its own, shifted, source index.
    #[test]
    fn expanded_steps_are_located() {
        let (shared, _) = catalog(json!({"fragments": {"pair": {"tasks": [
            {"id": "one", "name": "one", "function": {"name": "map", "input": {"mappings": []}}},
            {"id": "two", "name": "two", "function": {"name": "map", "input": {"mappings": []}}}]}}}));
        let (doc, _, map) = compiled(
            &shared,
            json!({"tasks": [
                {"$each": {"p": [1, 2]}, "do": {"id": "e{{p}}", "name": "e",
                    "function": {"name": "map", "input": {"mappings": []}}}},
                {"id": "u", "use": "pair"},
                {"id": "last", "name": "last", "function": {"name": "map", "input": {"mappings": []}}}
            ]}),
        );
        assert_eq!(doc["tasks"].as_array().map(Vec::len), Some(5));
        let copy = map.resolve("wf.json", "tasks[1].name");
        assert_eq!(copy.file, "wf.json");
        assert_eq!(copy.path.as_deref(), Some("tasks[0].do.name"));
        assert_eq!(copy.describe().as_deref(), Some("$each p = 2"));
        assert!(!copy.is_authored_here());
        let used = map.resolve("wf.json", "tasks[3]");
        assert_eq!(used.file, "shared.json");
        assert_eq!(used.path.as_deref(), Some("fragments.pair.tasks[1]"));
        let last = map.resolve("wf.json", "tasks[4].name");
        assert_eq!(last.path.as_deref(), Some("tasks[2].name"));
        assert!(last.is_authored_here());
    }
}
