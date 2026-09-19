//! Shared definition sources: named values a workflow splices in, and named
//! task sequences it includes (#285).
//!
//! `definitions/` had no way to say a thing once. A shared guard, a shared
//! connector target and a shared error string were each copied per workflow
//! and kept in sync by hand — in the deployment that motivated this, 44% of
//! tasks were byte-identical copies of another task's `function` block, and
//! one error had already drifted into three spellings (`User Not Found !`,
//! `User Not Found!`, `User not found !`) depending on which workflow you hit.
//!
//! ## One primitive, not three
//!
//! The proposal named three mechanisms — `use`/`with` for task sequences,
//! `$const` for connector coordinates, `$error` for the error catalog. Two of
//! those are the same operation: splice the fields of a named shared value
//! into the object you are standing in. So there is one value operator over a
//! document with **open namespaces**, and a future `timeouts` or `headers`
//! catalog costs no code here:
//!
//! ```json
//! { "constants": { "db": { "connector": "sias-mongo", "database": "app" } },
//!   "errors":    { "USER_NOT_FOUND": { "status": 400, "body": "User Not Found !" } } }
//! ```
//!
//! ```json
//! { "$from": "constants.db", "collection": "users" }
//! { "$from": "errors.USER_NOT_FOUND" }
//! ```
//!
//! ## Splice, not substitute
//!
//! `{"$from": "constants.db", "collection": "users"}` resolves to *three*
//! keys, not one — it merges the target's fields into the object around it.
//! **Siblings win**, so a call site can override one field of a shared value
//! without copying the rest. Named explicitly because the neighbouring
//! mechanism guesses differently: a `map` mapping template writes set-at-path
//! per key it names, replacing whole subtrees, and the difference between the
//! two is not something an author should have to discover.
//!
//! A `$from` alone in its object, pointing at a scalar or array, replaces the
//! whole node — the same rule with no siblings to lose to.
//!
//! ## Fragments, and what composes
//!
//! A fragment is a task sequence (`tasks`, included by a `use` step) or a
//! value (`value`, spliced by `$use` under the same rule as `$from`), either
//! with parameters. Fragments may use fragments, constants may reference
//! constants and value fragments, and an `$each` repeats one element over a
//! list — the expansion itself is [`super::expand`]. Constants are grounded
//! once, when the catalog is complete ([`SharedDefinitions::finish`]), so
//! the value splicer only ever copies closed values.
//!
//! ## Where this runs
//!
//! Strictly in the authoring and deploy path, never in the engine. Expansion
//! happens on the raw JSON before `CreateWorkflowRequest` parses, so `lint`,
//! `dry-run` and `test` all see the expanded form and the stored shape is
//! unchanged — the runtime, the admin API, traces and the UI never meet a
//! `$from` or a `use`. `package export` needs no inlining step for the same
//! reason: it exports what a server stored, and a server is only ever sent
//! expanded JSON.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use super::diagnostic::Diagnostic;

/// The reserved top-level keys that mark a document as shared definitions
/// rather than an entity.
///
/// `fragments` is a namespace like any other at the file level but is read by
/// the task expander rather than the value splicer, so it is held separately.
const SHARED_KEYS: [&str; 3] = ["constants", "errors", "fragments"];

/// What a fragment expands to.
#[derive(Debug, Clone)]
pub enum FragmentBody {
    /// A task fragment: steps, included by a `use` step.
    Tasks(Vec<Value>),
    /// A value fragment: one value, spliced by `$use`.
    Value(Value),
}

/// A named, parameterised task sequence or value.
#[derive(Debug, Clone)]
pub struct Fragment {
    /// Parameter name → default. A parameter with no default is required at
    /// every call site.
    pub params: BTreeMap<String, Option<Value>>,
    pub body: FragmentBody,
    /// The shared document that declared it — what a relative file reference
    /// inside it (`$sql`) was written against.
    pub origin: String,
}

impl Fragment {
    /// The steps of a task fragment; `None` for a value fragment.
    pub fn tasks(&self) -> Option<&[Value]> {
        match &self.body {
            FragmentBody::Tasks(tasks) => Some(tasks),
            FragmentBody::Value(_) => None,
        }
    }

    /// `task` or `value`, for messages.
    pub fn kind(&self) -> &'static str {
        match self.body {
            FragmentBody::Tasks(_) => "task",
            FragmentBody::Value(_) => "value",
        }
    }
}

/// The reserved key of the set's package declaration — see
/// [`SharedDefinitions::is_package_declaration`].
pub const PACKAGE_KEY: &str = "package";

/// What a set declares about itself: `{"package": {"name": "orders",
/// "requires": {"orion": ">=1.8.2, <2"}}}`. At most one per set.
#[derive(Debug, Clone, Default)]
pub struct PackageDecl {
    /// The document that declared it, for messages.
    pub origin: String,
    pub name: Option<String>,
    /// As written; parsed where it is checked, so a malformed range is a
    /// finding on the surface that reads it rather than a load failure.
    pub requires_orion: Option<String>,
}

impl PackageDecl {
    /// The declared `requires.orion` range against this binary — the check
    /// every offline command runs first, so a set this binary is too old for
    /// is reported as that, in one line, rather than as the schema errors of
    /// the features it predates.
    ///
    /// # Errors
    ///
    /// The range does not admit this binary, or is not a range.
    pub fn check_this_binary(&self) -> Result<(), String> {
        let Some(range) = &self.requires_orion else {
            return Ok(());
        };
        crate::version::OrionRequirement::parse(range)
            .map_err(|e| format!("{}: package.requires.orion {e}", self.origin))?
            .check_this_binary(&format!("the definition set ({})", self.origin))
    }
}

/// Everything a set shares: value namespaces, fragments, and the package
/// declaration.
#[derive(Debug, Clone, Default)]
pub struct SharedDefinitions {
    /// namespace → key → value. Open: `constants` and `errors` are two
    /// entries, not two fields.
    pub namespaces: BTreeMap<String, BTreeMap<String, Value>>,
    pub fragments: BTreeMap<String, Fragment>,
    /// The set's `package` document, when it has one. Not a `$from`
    /// namespace: `{"$from": "package.name"}` resolves nothing.
    pub package: Option<PackageDecl>,
    /// `(namespace, key)` → the shared document that declared the value, so
    /// a `$sql` inside a spliced constant resolves against its own file.
    pub value_origins: BTreeMap<(String, String), String>,
    /// Values [`Self::finish`] could not ground — members of a reference
    /// cycle, or nested too deep. Reported once, there; a reference to one
    /// is dropped without a second finding.
    pub poisoned: BTreeSet<(String, String)>,
}

impl SharedDefinitions {
    /// Collect just the shared documents under `dir`, ignoring entities.
    ///
    /// The single-file commands (`lint <file>`, `dry-run`, `test`) need the
    /// catalog a set declares without linting the set: an author dry-running
    /// one workflow wants that workflow's references resolved, not a report on
    /// the sixty files beside it.
    pub fn from_directory(dir: &std::path::Path) -> Result<(Self, Vec<Diagnostic>), String> {
        let mut shared = SharedDefinitions::default();
        let mut findings = Vec::new();
        let mut docs: Vec<(String, Value)> = Vec::new();
        collect(dir, &mut docs, &mut findings)?;
        // Sorted so a name defined twice is reported against the same file on
        // every machine.
        docs.sort_by(|a, b| a.0.cmp(&b.0));
        for (origin, doc) in &docs {
            shared.merge(doc, origin, &mut findings);
        }
        shared.finish(&mut findings);
        Ok((shared, findings))
    }

    pub fn is_empty(&self) -> bool {
        self.namespaces.is_empty() && self.fragments.is_empty()
    }

    /// Whether a document is shared definitions rather than an entity.
    ///
    /// Shape, like [`super::Entity::classify`], and for the same reason: the
    /// layout belongs to whoever authored the directory. A document carrying
    /// one of the reserved keys and no entity discriminator is unambiguous —
    /// no channel, workflow or connector has a top-level `constants`,
    /// `errors` or `fragments`.
    pub fn is_shared_document(doc: &Value) -> bool {
        let Some(obj) = doc.as_object() else {
            return false;
        };
        super::Entity::classify(doc).is_none()
            && (SHARED_KEYS.iter().any(|k| obj.contains_key(*k))
                || Self::is_package_declaration(doc))
    }

    /// Whether a document carries the set's package declaration, told apart
    /// from a promotion artifact, which also has a top-level `package`: an
    /// artifact's always carries `content_hash`, and its root always has
    /// `workflows`. An artifact lying inside a definitions tree therefore
    /// stays what it was — a file that is not part of the set.
    pub fn is_package_declaration(doc: &Value) -> bool {
        let Some(obj) = doc.as_object() else {
            return false;
        };
        obj.get(PACKAGE_KEY)
            .and_then(Value::as_object)
            .is_some_and(|package| !package.contains_key("content_hash"))
            && !obj.contains_key("workflows")
    }

    /// Merge one shared document into this one.
    ///
    /// Split across files on purpose: a set may keep `errors.json` beside
    /// `constants.json` beside a `fragments/` tree. A name defined twice is a
    /// finding rather than a last-write-wins, because which file won would
    /// depend on directory order.
    pub fn merge(&mut self, doc: &Value, origin: &str, findings: &mut Vec<Diagnostic>) {
        let Some(obj) = doc.as_object() else {
            return;
        };
        // A `$sql` path is re-read relative to whichever document the value
        // lands in, which an absolute path would defeat — and after copying,
        // one would be indistinguishable from the author's own. Reported here,
        // against the shared file that holds it.
        for bad in absolute_sql_paths(doc) {
            findings.push(Diagnostic::error(
                "shared.sql_path",
                origin,
                format!("'{bad}' must be a relative path to a .sql file"),
            ));
        }
        for (key, value) in obj {
            if key == "fragments" {
                self.merge_fragments(value, origin, findings);
                continue;
            }
            if key == PACKAGE_KEY {
                self.merge_package(value, origin, findings);
                continue;
            }
            let Some(entries) = value.as_object() else {
                findings.push(Diagnostic::error(
                    "shared.namespace",
                    origin,
                    format!("'{key}' must be an object of named values"),
                ));
                continue;
            };
            let ns = self.namespaces.entry(key.clone()).or_default();
            for (name, val) in entries {
                if ns.contains_key(name) {
                    findings.push(Diagnostic::error(
                        "shared.duplicate",
                        origin,
                        format!("'{key}.{name}' is already defined elsewhere in the set"),
                    ));
                    continue;
                }
                ns.insert(name.clone(), val.clone());
                self.value_origins
                    .insert((key.clone(), name.clone()), origin.to_string());
            }
        }
    }

    /// The package declaration: one per set, with a closed shape — a typo
    /// like `require` must not silently switch the version gate off.
    fn merge_package(&mut self, value: &Value, origin: &str, findings: &mut Vec<Diagnostic>) {
        if let Some(existing) = &self.package {
            findings.push(Diagnostic::error(
                "package.duplicate",
                origin,
                format!("'package' is already declared in {}", existing.origin),
            ));
            return;
        }
        let Some(obj) = value.as_object() else {
            findings.push(Diagnostic::error(
                "package.shape",
                origin,
                "'package' must be an object: {\"name\": …, \"requires\": {\"orion\": …}}",
            ));
            return;
        };
        let mut decl = PackageDecl {
            origin: origin.to_string(),
            ..PackageDecl::default()
        };
        for (key, member) in obj {
            match key.as_str() {
                "name" => match member.as_str() {
                    Some(name) => {
                        if let Err(e) = crate::validation::package_key(
                            "package.name",
                            name,
                            crate::validation::MAX_PACKAGE_NAME_LEN,
                        ) {
                            findings.push(Diagnostic::error("package.shape", origin, e));
                        } else {
                            decl.name = Some(name.to_string());
                        }
                    }
                    None => findings.push(Diagnostic::error(
                        "package.shape",
                        origin,
                        "'package.name' must be a string",
                    )),
                },
                "requires" => {
                    let Some(requires) = member.as_object() else {
                        findings.push(Diagnostic::error(
                            "package.shape",
                            origin,
                            "'package.requires' must be an object",
                        ));
                        continue;
                    };
                    for (requirement, range) in requires {
                        match (requirement.as_str(), range.as_str()) {
                            ("orion", Some(range)) => decl.requires_orion = Some(range.to_string()),
                            ("orion", None) => findings.push(Diagnostic::error(
                                "package.shape",
                                origin,
                                "'package.requires.orion' must be a version range string, like \
                                 \">=1.8.2, <2\"",
                            )),
                            (other, _) => findings.push(Diagnostic::error(
                                "package.shape",
                                origin,
                                format!(
                                    "'package.requires.{other}' is not a requirement this \
                                     version understands (only 'orion')"
                                ),
                            )),
                        }
                    }
                }
                other => findings.push(Diagnostic::error(
                    "package.shape",
                    origin,
                    format!(
                        "'package.{other}' is not a package field (expected 'name', 'requires')"
                    ),
                )),
            }
        }
        self.package = Some(decl);
    }

    fn merge_fragments(&mut self, value: &Value, origin: &str, findings: &mut Vec<Diagnostic>) {
        let Some(entries) = value.as_object() else {
            findings.push(Diagnostic::error(
                "shared.namespace",
                origin,
                "'fragments' must be an object of named task sequences and values",
            ));
            return;
        };
        for (name, spec) in entries {
            if self.fragments.contains_key(name) {
                findings.push(Diagnostic::error(
                    "shared.duplicate",
                    origin,
                    format!("fragment '{name}' is already defined elsewhere in the set"),
                ));
                continue;
            }
            let body = match (spec.get("tasks"), spec.get("value")) {
                (Some(Value::Array(tasks)), None) => FragmentBody::Tasks(tasks.clone()),
                (None, Some(value)) => FragmentBody::Value(value.clone()),
                (Some(_), None) => {
                    findings.push(Diagnostic::error(
                        "shared.fragment",
                        origin,
                        format!("fragment '{name}': 'tasks' must be an array of steps"),
                    ));
                    continue;
                }
                _ => {
                    findings.push(Diagnostic::error(
                        "shared.fragment",
                        origin,
                        format!(
                            "fragment '{name}' must declare exactly one of 'tasks' (a task \
                             fragment, included with `use`) or 'value' (a value fragment, \
                             spliced with `$use`)"
                        ),
                    ));
                    continue;
                }
            };
            let mut params = BTreeMap::new();
            if let Some(declared) = spec.get("params").and_then(Value::as_object) {
                for (param, decl) in declared {
                    params.insert(param.clone(), decl.get("default").cloned());
                }
            }
            self.fragments.insert(
                name.clone(),
                Fragment {
                    params,
                    body,
                    origin: origin.to_string(),
                },
            );
        }
    }

    /// Compile one authored document against this catalog, in place.
    ///
    /// The two rewrites below are the first two passes of the authoring
    /// pipeline, and this runs it: the ordering rule — fragments before
    /// values, so a fragment's own `$from` is spliced after it is inlined and
    /// a fragment is written exactly the way a workflow is — lives in
    /// [`super::compile::passes`] with everything else the pipeline
    /// guarantees.
    pub fn expand(&self, doc: &mut Value, origin: &str, findings: &mut Vec<Diagnostic>) {
        super::compile::compile(doc, &super::compile::Cx::detached(self, origin), findings);
    }

    /// Walk a value, splicing every `$from` against the namespaces.
    pub(super) fn splice(
        &self,
        value: &mut Value,
        cx: &super::compile::Cx<'_>,
        findings: &mut Vec<Diagnostic>,
    ) {
        let origin = cx.origin;
        match value {
            Value::Array(items) => {
                for item in items {
                    self.splice(item, cx, findings);
                }
            }
            Value::Object(map) => {
                for v in map.values_mut() {
                    self.splice(v, cx, findings);
                }
                let Some(path) = map.get("$from").and_then(Value::as_str).map(str::to_string)
                else {
                    return;
                };
                let replacement = match self.lookup(&path) {
                    // Reported once, where the catalog was grounded.
                    None if self.is_poisoned(&path) => {
                        map.remove("$from");
                        None
                    }
                    Some(target) => {
                        let mut target = target.clone();
                        if let Some(declared_in) = path.split_once('.').and_then(|(ns, key)| {
                            self.value_origins.get(&(ns.to_string(), key.to_string()))
                        }) {
                            reanchor_sql(&mut target, declared_in, cx.base_dir);
                        }
                        apply_splice(map, &target)
                    }
                    None => {
                        findings.push(Diagnostic::error(
                            "closure.shared_value",
                            origin,
                            format!("'{path}' is not defined in the set"),
                        ));
                        map.remove("$from");
                        None
                    }
                };
                // Written after the borrow of `map` ends: a scalar target with
                // no siblings replaces the node rather than merging into it.
                if let Some(replacement) = replacement {
                    *value = replacement;
                }
            }
            _ => {}
        }
    }

    /// `namespace.key` — one dot, because a namespace is a flat catalog and a
    /// deeper path would make the reference ambiguous with a key containing a
    /// dot.
    pub(super) fn lookup(&self, path: &str) -> Option<&Value> {
        let (namespace, key) = path.split_once('.')?;
        if self
            .poisoned
            .contains(&(namespace.to_string(), key.to_string()))
        {
            return None;
        }
        self.namespaces.get(namespace)?.get(key)
    }

    fn is_poisoned(&self, path: &str) -> bool {
        path.split_once('.')
            .is_some_and(|(ns, key)| self.poisoned.contains(&(ns.to_string(), key.to_string())))
    }

    /// Ground every shared value, once, after the last [`Self::merge`]: a
    /// constant that references a constant, uses a value fragment or holds
    /// an `$each` becomes the closed value it stands for, so the value
    /// splicer copies it in one step and compiling stays idempotent.
    ///
    /// A reference cycle — `constants.a → constants.b → constants.a`, or
    /// through a fragment — is reported once, at the file that declares
    /// it, and its members are poisoned.
    pub fn finish(&mut self, findings: &mut Vec<Diagnostic>) {
        let keys: Vec<(String, String)> = self
            .namespaces
            .iter()
            .flat_map(|(ns, entries)| entries.keys().map(move |key| (ns.clone(), key.clone())))
            .collect();
        let mut state = Grounding::default();
        for key in &keys {
            self.ground(key, &mut state, findings);
        }
        for (key, value) in state.done {
            match value {
                Some(value) => {
                    if let Some(slot) = self
                        .namespaces
                        .get_mut(&key.0)
                        .and_then(|ns| ns.get_mut(&key.1))
                    {
                        *slot = value;
                    }
                }
                None => {
                    self.poisoned.insert(key);
                }
            }
        }
    }

    fn ground(
        &self,
        key: &(String, String),
        state: &mut Grounding,
        findings: &mut Vec<Diagnostic>,
    ) -> Option<Value> {
        if let Some(done) = state.done.get(key) {
            return done.clone();
        }
        let label = format!("{}.{}", key.0, key.1);
        let origin = self.value_origins.get(key).cloned().unwrap_or_default();
        if let Some(pos) = state.stack.iter().position(|s| *s == label) {
            let chain: Vec<&str> = state.stack[pos..]
                .iter()
                .map(String::as_str)
                .chain(std::iter::once(label.as_str()))
                .collect();
            findings.push(Diagnostic::error(
                "shared.cycle",
                &origin,
                format!("'{label}' refers to itself: {}", chain.join(" → ")),
            ));
            for member in &state.stack[pos..] {
                state.cyclic.insert(member.clone());
            }
            return None;
        }
        if state.stack.len() >= super::expand::MAX_EXPANSION_DEPTH {
            findings.push(Diagnostic::error(
                "shared.depth",
                &origin,
                format!(
                    "'{label}' references values nested more than {} deep: {}",
                    super::expand::MAX_EXPANSION_DEPTH,
                    state.stack.join(" → ")
                ),
            ));
            state.done.insert(key.clone(), None);
            return None;
        }
        let mut value = self.namespaces.get(&key.0)?.get(&key.1)?.clone();
        state.stack.push(label.clone());
        // `$use`, `$each` and `{{name}}` first, at the constant's own file.
        let base_dir = std::path::Path::new(&origin).parent();
        let cx = super::compile::Cx {
            shared: self,
            origin: &origin,
            base_dir,
            root: None,
        };
        let mut map = super::provenance::SourceMap::default();
        super::expand::Expander::new(&cx, findings, &mut map).closed_value(&mut value, &label);
        // Then every `$from`, each target grounded first.
        self.ground_from(&mut value, &origin, state, findings);
        state.stack.pop();
        let grounded = if state.cyclic.contains(&label) {
            None
        } else {
            Some(value)
        };
        state.done.insert(key.clone(), grounded.clone());
        grounded
    }

    fn ground_from(
        &self,
        value: &mut Value,
        origin: &str,
        state: &mut Grounding,
        findings: &mut Vec<Diagnostic>,
    ) {
        match value {
            Value::Array(items) => {
                for item in items {
                    self.ground_from(item, origin, state, findings);
                }
            }
            Value::Object(map) => {
                for member in map.values_mut() {
                    self.ground_from(member, origin, state, findings);
                }
                let Some(path) = map.get("$from").and_then(Value::as_str).map(str::to_string)
                else {
                    return;
                };
                let key = path
                    .split_once('.')
                    .map(|(ns, key)| (ns.to_string(), key.to_string()))
                    .filter(|(ns, key)| {
                        self.namespaces
                            .get(ns)
                            .is_some_and(|entries| entries.contains_key(key))
                    });
                let Some(key) = key else {
                    findings.push(Diagnostic::error(
                        "closure.shared_value",
                        origin,
                        format!("'{path}' is not defined in the set"),
                    ));
                    map.remove("$from");
                    return;
                };
                let replacement = match self.ground(&key, state, findings) {
                    Some(mut target) => {
                        if let Some(declared_in) = self.value_origins.get(&key) {
                            reanchor_sql(
                                &mut target,
                                declared_in,
                                std::path::Path::new(origin).parent(),
                            );
                        }
                        apply_splice(map, &target)
                    }
                    None => {
                        map.remove("$from");
                        None
                    }
                };
                if let Some(replacement) = replacement {
                    *value = replacement;
                }
            }
            _ => {}
        }
    }
}

/// [`SharedDefinitions::finish`]'s working state.
#[derive(Default)]
struct Grounding {
    done: BTreeMap<(String, String), Option<Value>>,
    stack: Vec<String>,
    cyclic: BTreeSet<String>,
}

/// Rewrite every relative `$sql` path in `value` — written relative to the
/// shared document `declared_in` — so it reads relative to `to_dir`, the
/// directory of the document it is being copied into. With no `to_dir`
/// nothing is rewritten, and the `$sql` pass reports that the reference
/// cannot be resolved.
pub(super) fn reanchor_sql(value: &mut Value, declared_in: &str, to_dir: Option<&std::path::Path>) {
    let Some(to_dir) = to_dir else {
        return;
    };
    let from_dir = std::path::Path::new(declared_in)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    match value {
        Value::Array(items) => {
            for item in items {
                reanchor_sql(item, declared_in, Some(to_dir));
            }
        }
        Value::Object(map) => {
            if let Some(Value::String(target)) = map.get_mut("$sql") {
                if super::compile::is_relative_sql_path(target) {
                    let rewritten = super::compile::relative(to_dir, &from_dir.join(&*target));
                    *target = rewritten.to_string_lossy().replace('\\', "/");
                }
                return;
            }
            for v in map.values_mut() {
                reanchor_sql(v, declared_in, Some(to_dir));
            }
        }
        _ => {}
    }
}

/// Every `$sql` path in `value` that is absolute.
fn absolute_sql_paths(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![value];
    while let Some(v) = stack.pop() {
        match v {
            Value::Array(items) => stack.extend(items),
            Value::Object(map) => {
                if let Some(target) = map.get("$sql").and_then(Value::as_str)
                    && !target.is_empty()
                    && (std::path::Path::new(target).is_absolute() || target.starts_with('/'))
                {
                    out.push(target.to_string());
                }
                stack.extend(map.values());
            }
            _ => {}
        }
    }
    out
}

/// The first shared reference in a document, described for an error message,
/// or `None` if it has none.
///
/// Exists so a command asked to handle a document it cannot resolve can name
/// the cause rather than letting validation report the symptom — an
/// unexpanded `use` task looks to the validator like a task missing its
/// `name` and `function`, which sends the reader to the wrong place.
///
/// Reads the pipeline's own residue rather than walking again, so "what the
/// single-file commands refuse" and "what `compile` would have consumed" stay
/// one statement, and a pass added later is named here without being taught
/// about.
pub fn first_reference(doc: &Value) -> Option<String> {
    super::compile::residue(doc, "")
        .first()
        .map(super::compile::Residue::describe)
}

/// Keep the shared documents from the set walk, and report what would not
/// parse.
///
/// A file that cannot be read is *not* silently skipped, even though this
/// pass wants only the shared half: the file it could not parse may have been
/// the catalog, and dropping it in silence turned a syntax error in
/// `constants.json` into "'constants.db' is not defined in the set" — the
/// symptom, reported against the file that was written correctly. A warning
/// rather than an error because most files under the directory are entities
/// this pass has no need of, and a single-file `dry-run` should not be
/// blocked by a broken workflow it was never going to read.
fn collect(
    dir: &std::path::Path,
    out: &mut Vec<(String, Value)>,
    findings: &mut Vec<Diagnostic>,
) -> Result<(), String> {
    super::set::walk_json_files(dir, &mut |path, parsed, _spans| match parsed {
        Ok(doc) => {
            if SharedDefinitions::is_shared_document(&doc) {
                out.push((path.display().to_string(), doc));
            }
        }
        Err(e) => findings.push(Diagnostic::warning(
            "shared.unparseable",
            path.display().to_string(),
            format!(
                "could not be read as JSON ({e}), so any shared value or fragment \
                 it declares is missing from this catalog"
            ),
        )),
    })
}

/// Merge `target` into the object the `$from` sat in.
///
/// Returns `Some(value)` when the node should be *replaced* wholesale rather
/// than merged into — a scalar or array target with no siblings to merge
/// alongside.
///
/// Siblings win: a call site that names a key the shared value also names is
/// overriding it deliberately, which is what makes a shared value usable
/// without copying it to change one field.
fn apply_splice(map: &mut Map<String, Value>, target: &Value) -> Option<Value> {
    map.remove("$from");
    match target {
        Value::Object(fields) => {
            for (key, value) in fields {
                map.entry(key.clone()).or_insert_with(|| value.clone());
            }
            None
        }
        // A scalar or array target has no fields to merge. With siblings
        // present there is nowhere sensible to put it, so the siblings stand
        // alone and the reference is dropped; without them the node *is* the
        // value.
        other if map.is_empty() => Some(other.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A `$sql` path inside a fragment or a constant was written against the
    /// shared document's own directory; once copied into a workflow it must
    /// read relative to the workflow's.
    #[test]
    fn a_sql_reference_is_reanchored_to_the_document_it_lands_in() {
        let mut shared = SharedDefinitions::default();
        let mut findings = Vec::new();
        shared.merge(
            &json!({
                "constants": {"lookup": {"connector": "db", "query": {"$sql": "../sql/lookup.sql"}}},
                "fragments": {"read": {"tasks": [
                    {"id": "r", "name": "r", "function": {"name": "db_read",
                     "input": {"connector": "db", "query": {"$sql": "../sql/read.sql"}}}}]}}
            }),
            "defs/shared/catalog.json",
            &mut findings,
        );
        assert!(findings.is_empty(), "{findings:?}");
        let mut doc = json!({"workflow_id": "w", "name": "w", "tasks": [
            {"id": "_r", "use": "read"},
            {"id": "l", "name": "l", "function": {"name": "db_read",
             "input": {"$from": "constants.lookup"}}}]});
        let base = std::path::Path::new("defs/services/orders");
        let cx = crate::definitions::compile::Cx {
            shared: &shared,
            origin: "defs/services/orders/w.json",
            base_dir: Some(base),
            root: None,
        };
        let mut map = crate::definitions::provenance::SourceMap::default();
        crate::definitions::expand::Expander::new(&cx, &mut findings, &mut map).document(&mut doc);
        shared.splice(&mut doc, &cx, &mut findings);
        assert!(findings.is_empty(), "{findings:?}");
        assert_eq!(
            doc["tasks"][0]["function"]["input"]["query"]["$sql"],
            "../../sql/read.sql"
        );
        assert_eq!(
            doc["tasks"][1]["function"]["input"]["query"]["$sql"],
            "../../sql/lookup.sql"
        );
    }

    #[test]
    fn an_absolute_sql_path_in_a_shared_document_is_reported_at_merge() {
        let mut shared = SharedDefinitions::default();
        let mut findings = Vec::new();
        shared.merge(
            &json!({"constants": {"q": {"$sql": "/srv/sql/q.sql"}}}),
            "catalog.json",
            &mut findings,
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].check, "shared.sql_path");
    }

    fn declaration() -> Value {
        json!({"package": {"name": "orders", "requires": {"orion": ">=1.8.2, <2"}}})
    }

    #[test]
    fn a_package_only_document_is_shared() {
        assert!(SharedDefinitions::is_shared_document(&declaration()));
        let mut s = SharedDefinitions::default();
        let mut findings = Vec::new();
        s.merge(&declaration(), "package.json", &mut findings);
        assert!(findings.is_empty(), "{findings:?}");
        let decl = s.package.expect("declared");
        assert_eq!(decl.name.as_deref(), Some("orders"));
        assert_eq!(decl.requires_orion.as_deref(), Some(">=1.8.2, <2"));
        assert_eq!(decl.origin, "package.json");
        assert!(
            s.namespaces.is_empty(),
            "'package' is not a value namespace"
        );
    }

    /// An artifact also has a top-level `package`; it must stay what it was.
    #[test]
    fn a_promotion_artifact_is_not_a_package_declaration() {
        let artifact = json!({
            "package": {"name": "orders", "version": "1.0.0", "content_hash": "sha256:x"},
            "requires": {}, "connectors": [], "workflows": [], "channels": [],
        });
        assert!(!SharedDefinitions::is_package_declaration(&artifact));
        assert!(!SharedDefinitions::is_shared_document(&artifact));
        // Even with the hash missing, an artifact's `workflows` gives it away.
        let mut hashless = artifact.clone();
        hashless["package"]
            .as_object_mut()
            .expect("object")
            .remove("content_hash");
        assert!(!SharedDefinitions::is_package_declaration(&hashless));
    }

    #[test]
    fn package_is_not_a_from_namespace() {
        let mut s = SharedDefinitions::default();
        s.merge(&declaration(), "package.json", &mut Vec::new());
        let mut doc = json!({"x": {"$from": "package.name"}});
        let mut findings = Vec::new();
        s.expand(&mut doc, "wf.json", &mut findings);
        assert!(
            findings.iter().any(Diagnostic::is_error),
            "`package.name` must not resolve: {findings:?}"
        );
    }

    #[test]
    fn two_package_documents_are_a_duplicate() {
        let mut s = SharedDefinitions::default();
        let mut findings = Vec::new();
        s.merge(&declaration(), "a/package.json", &mut findings);
        s.merge(&declaration(), "b/package.json", &mut findings);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].check, "package.duplicate");
        assert!(
            findings[0].message.contains("a/package.json"),
            "{findings:?}"
        );
    }

    #[test]
    fn an_unknown_key_under_package_is_named() {
        let mut s = SharedDefinitions::default();
        let mut findings = Vec::new();
        s.merge(
            &json!({"package": {"name": "orders", "require": {"orion": ">=1"},
                                "requires": {"orion": ">=1", "dataflow": ">=3"}}}),
            "package.json",
            &mut findings,
        );
        let messages: Vec<&str> = findings.iter().map(|f| f.message.as_str()).collect();
        assert!(
            messages.iter().any(|m| m.contains("'package.require'")),
            "{messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|m| m.contains("'package.requires.dataflow'")),
            "{messages:?}"
        );
        assert!(findings.iter().all(|f| f.check == "package.shape"));
    }

    fn shared() -> (SharedDefinitions, Vec<Diagnostic>) {
        let mut s = SharedDefinitions::default();
        let mut f = Vec::new();
        s.merge(
            &json!({
                "constants": { "db": { "connector": "sias-mongo", "database": "app" },
                               "timeout": 30000 },
                "errors": { "USER_NOT_FOUND": { "status": 400, "body": "User Not Found !" } },
                // The fragment holds a **task group**, deliberately: while this
                // fixture was flat, every test below passed over a fragment
                // whose nested ids were never namespaced, which is how #294
                // survived a full suite. A guard clause is also the shape 1.2.0
                // encourages, so it is the realistic fragment to test with.
                "fragments": { "require-session": {
                    "params": { "deny_message": { "default": "Session expired." },
                                "realm": {} },
                    "tasks": [
                        { "id": "check", "name": "Check",
                          "function": { "name": "map", "input": { "mappings": [
                            { "path": "data.msg", "logic": { "$param": "deny_message" } },
                            { "path": "data.realm", "logic": { "$param": "realm" } } ] } } },
                        { "id": "refused", "condition": true, "tasks": [
                            { "id": "deny", "name": "Deny",
                              "function": { "name": "map", "input": { "mappings": [
                                { "path": "data.denied", "logic": { "$param": "realm" } } ] } } } ] },
                        { "id": "halt", "name": "Halt",
                          "function": { "name": "map", "input": { "mappings": [] } } }
                    ] } }
            }),
            "common.json",
            &mut f,
        );
        (s, f)
    }

    /// The connector-coordinates case: three keys from one reference, with the
    /// call site's own key alongside.
    #[test]
    fn a_from_reference_splices_fields_into_its_object() {
        let (s, mut f) = shared();
        let mut doc = json!({ "input": { "$from": "constants.db", "collection": "users" } });
        s.expand(&mut doc, "wf.json", &mut f);
        assert_eq!(
            doc["input"],
            json!({ "connector": "sias-mongo", "database": "app", "collection": "users" })
        );
        assert!(f.is_empty(), "{f:?}");
    }

    /// Siblings win, so a call site overrides one field without copying the
    /// rest — the reason the merge direction matters.
    #[test]
    fn a_sibling_key_overrides_the_shared_value() {
        let (s, mut f) = shared();
        let mut doc = json!({ "input": { "$from": "constants.db", "database": "other" } });
        s.expand(&mut doc, "wf.json", &mut f);
        assert_eq!(doc["input"]["database"], "other");
        assert_eq!(doc["input"]["connector"], "sias-mongo");
        assert!(f.is_empty(), "{f:?}");
    }

    /// A scalar target with no siblings replaces the node outright.
    #[test]
    fn a_lone_reference_to_a_scalar_becomes_that_scalar() {
        let (s, mut f) = shared();
        let mut doc = json!({ "timeout_ms": { "$from": "constants.timeout" } });
        s.expand(&mut doc, "wf.json", &mut f);
        assert_eq!(doc["timeout_ms"], 30000);
        assert!(f.is_empty(), "{f:?}");
    }

    /// The drift this exists to prevent: one catalog entry, one spelling.
    #[test]
    fn an_error_catalog_entry_expands_to_its_fields() {
        let (s, mut f) = shared();
        let mut doc = json!({ "input": { "$from": "errors.USER_NOT_FOUND" } });
        s.expand(&mut doc, "wf.json", &mut f);
        assert_eq!(doc["input"]["body"], "User Not Found !");
        assert_eq!(doc["input"]["status"], 400);
    }

    /// A typo'd reference is a finding, not a silently empty object — the
    /// whole reason set lint is the prerequisite for this feature.
    #[test]
    fn an_unresolvable_reference_is_reported() {
        let (s, mut f) = shared();
        let mut doc = json!({ "input": { "$from": "constants.nope" } });
        s.expand(&mut doc, "wf.json", &mut f);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].check, "closure.shared_value");
        assert!(f[0].message.contains("constants.nope"), "{:?}", f[0]);
    }

    #[test]
    fn a_fragment_expands_with_namespaced_ids_and_arguments() {
        let (s, mut f) = shared();
        let mut doc = json!({ "name": "w", "tasks": [
            { "id": "_session", "use": "require-session",
              "with": { "deny_message": "Please sign in again.", "realm": "app" } },
            { "id": "own", "name": "Own", "function": { "name": "map", "input": {"mappings": []} } }
        ] });
        s.expand(&mut doc, "wf.json", &mut f);
        assert!(f.is_empty(), "{f:?}");

        let tasks = doc["tasks"].as_array().expect("array");
        assert_eq!(
            tasks.len(),
            4,
            "three fragment steps plus the workflow's own"
        );
        assert_eq!(tasks[0]["id"], "_session.check", "ids are namespaced");
        assert_eq!(
            tasks[1]["id"], "_session.refused",
            "including a group's own id"
        );
        assert_eq!(
            tasks[1]["tasks"][0]["id"], "_session.deny",
            "and the ids inside that group, which is what #294 was"
        );
        assert_eq!(tasks[2]["id"], "_session.halt");
        assert_eq!(
            tasks[3]["id"], "own",
            "the workflow's own task is untouched"
        );
        assert_eq!(
            tasks[0]["function"]["input"]["mappings"][0]["logic"], "Please sign in again.",
            "the call site's argument wins over the default"
        );
        assert_eq!(tasks[0]["function"]["input"]["mappings"][1]["logic"], "app");
        assert_eq!(
            tasks[1]["tasks"][0]["function"]["input"]["mappings"][0]["logic"], "app",
            "parameters reach a nested task too"
        );
    }

    /// Two instances of one fragment must not collide, which is the whole
    /// point of prefixing by the call-site id.
    #[test]
    fn two_instances_of_one_fragment_do_not_collide() {
        let (s, mut f) = shared();
        let mut doc = json!({ "tasks": [
            { "id": "a", "use": "require-session", "with": { "realm": "x" } },
            { "id": "b", "use": "require-session", "with": { "realm": "y" } }
        ] });
        s.expand(&mut doc, "wf.json", &mut f);
        let tasks = doc["tasks"].as_array().expect("array");
        let ids: Vec<&str> = tasks.iter().filter_map(|t| t["id"].as_str()).collect();
        assert_eq!(
            ids,
            [
                "a.check",
                "a.refused",
                "a.halt",
                "b.check",
                "b.refused",
                "b.halt"
            ]
        );
        // The nested ids are the ones that used to collide: both instances
        // emitted a bare `deny`, and the workflow was refused at validation
        // with a DUPLICATE_TASK_ID the author had no way to predict, because
        // the name is private to the fragment (#294).
        assert_eq!(tasks[1]["tasks"][0]["id"], "a.deny");
        assert_eq!(tasks[4]["tasks"][0]["id"], "b.deny");
        assert!(f.is_empty(), "{f:?}");
    }

    /// A fragment may use a fragment (#333), inside a group as at its top
    /// level; the inner steps carry both call sites, `outer.inner.id`, so
    /// neither expansion can collide with the host or with itself.
    #[test]
    fn a_fragment_may_use_a_fragment_and_carries_both_prefixes() {
        let mut s = SharedDefinitions::default();
        let mut f = Vec::new();
        s.merge(
            &json!({ "fragments": {
                "outer": { "tasks": [
                    { "id": "i", "use": "inner" },
                    { "id": "span", "condition": true, "tasks": [
                        { "id": "g", "use": "inner" }] }] },
                "inner": { "tasks": [{ "id": "t", "name": "t",
                    "function": { "name": "map", "input": { "mappings": [] } } }] } } }),
            "common.json",
            &mut f,
        );
        let mut doc = json!({ "tasks": [{ "id": "o", "use": "outer" }] });
        s.expand(&mut doc, "wf.json", &mut f);
        assert!(f.is_empty(), "{f:?}");
        assert_eq!(doc["tasks"][0]["id"], "o.i.t");
        assert_eq!(doc["tasks"][1]["id"], "o.span");
        assert_eq!(doc["tasks"][1]["tasks"][0]["id"], "o.g.t");
        assert!(crate::definitions::compile::residue(&doc, "").is_empty());
    }

    /// A parameter with a default may be omitted; one without cannot.
    #[test]
    fn a_required_parameter_must_be_supplied() {
        let (s, mut f) = shared();
        let mut doc = json!({ "tasks": [{ "id": "x", "use": "require-session" }] });
        s.expand(&mut doc, "wf.json", &mut f);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].message.contains("'realm'"), "{:?}", f[0]);
        assert!(f[0].message.contains("no default"), "{:?}", f[0]);
    }

    #[test]
    fn an_unknown_argument_and_an_unknown_fragment_are_reported() {
        let (s, mut f) = shared();
        let mut doc = json!({ "tasks": [
            { "id": "x", "use": "require-session", "with": { "realm": "r", "typo": 1 } },
            { "id": "y", "use": "no-such-fragment" }
        ] });
        s.expand(&mut doc, "wf.json", &mut f);
        let checks: Vec<&str> = f.iter().map(|x| x.check).collect();
        assert!(checks.contains(&"shared.fragment_param"), "{f:?}");
        assert!(checks.contains(&"closure.fragment"), "{f:?}");
    }

    /// A name defined in two files is ambiguous — which one wins would depend
    /// on directory order, so neither does.
    #[test]
    fn a_name_defined_twice_is_reported() {
        let (mut s, mut f) = shared();
        s.merge(
            &json!({ "constants": { "db": { "connector": "other" } } }),
            "second.json",
            &mut f,
        );
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].check, "shared.duplicate");
        assert_eq!(
            s.namespaces["constants"]["db"]["connector"], "sias-mongo",
            "the first definition stands rather than being silently replaced"
        );
    }

    /// Shared documents are told apart from entities by shape, and must not
    /// swallow one.
    #[test]
    fn a_shared_document_is_not_an_entity() {
        assert!(SharedDefinitions::is_shared_document(
            &json!({"constants": {}})
        ));
        assert!(SharedDefinitions::is_shared_document(
            &json!({"fragments": {}})
        ));
        assert!(!SharedDefinitions::is_shared_document(
            &json!({"name": "w", "tasks": []})
        ));
        assert!(!SharedDefinitions::is_shared_document(&json!({"data": {}})));
    }

    /// A fragment that includes itself, directly or through another, is
    /// named with its chain and dropped rather than expanded for ever.
    #[test]
    fn a_fragment_cycle_is_named() {
        let mut s = SharedDefinitions::default();
        let mut f = Vec::new();
        s.merge(
            &json!({ "fragments": {
                "a": { "tasks": [{ "id": "b", "use": "b" }] },
                "b": { "tasks": [{ "id": "a", "use": "a" }] } } }),
            "common.json",
            &mut f,
        );
        let mut doc = json!({ "tasks": [{ "id": "x", "use": "a" }] });
        s.expand(&mut doc, "wf.json", &mut f);
        let cycle: Vec<_> = f.iter().filter(|x| x.check == "shared.cycle").collect();
        assert_eq!(cycle.len(), 1, "{f:?}");
        assert!(
            cycle[0].message.contains("'a' → 'b' → 'a'"),
            "{:?}",
            cycle[0]
        );
        assert!(crate::definitions::compile::residue(&doc, "").is_empty());
    }
}
