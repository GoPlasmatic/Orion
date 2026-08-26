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

use super::finding::Finding;

/// The reserved top-level keys that mark a document as shared definitions
/// rather than an entity.
///
/// `fragments` is a namespace like any other at the file level but is read by
/// the task expander rather than the value splicer, so it is held separately.
const SHARED_KEYS: [&str; 3] = ["constants", "errors", "fragments"];

/// A named, parameterised task sequence.
#[derive(Debug, Clone, Default)]
pub struct Fragment {
    /// Parameter name → default. A parameter with no default is required at
    /// every call site.
    pub params: BTreeMap<String, Option<Value>>,
    pub tasks: Vec<Value>,
}

/// Everything a set shares: value namespaces, and fragments.
#[derive(Debug, Clone, Default)]
pub struct SharedDefinitions {
    /// namespace → key → value. Open: `constants` and `errors` are two
    /// entries, not two fields.
    pub namespaces: BTreeMap<String, BTreeMap<String, Value>>,
    pub fragments: BTreeMap<String, Fragment>,
}

impl SharedDefinitions {
    /// Collect just the shared documents under `dir`, ignoring entities.
    ///
    /// The single-file commands (`lint <file>`, `dry-run`, `test`) need the
    /// catalog a set declares without linting the set: an author dry-running
    /// one workflow wants that workflow's references resolved, not a report on
    /// the sixty files beside it.
    pub fn from_directory(dir: &std::path::Path) -> Result<(Self, Vec<Finding>), String> {
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
        super::Entity::classify(doc).is_none() && SHARED_KEYS.iter().any(|k| obj.contains_key(*k))
    }

    /// Merge one shared document into this one.
    ///
    /// Split across files on purpose: a set may keep `errors.json` beside
    /// `constants.json` beside a `fragments/` tree. A name defined twice is a
    /// finding rather than a last-write-wins, because which file won would
    /// depend on directory order.
    pub fn merge(&mut self, doc: &Value, origin: &str, findings: &mut Vec<Finding>) {
        let Some(obj) = doc.as_object() else {
            return;
        };
        for (key, value) in obj {
            if key == "fragments" {
                self.merge_fragments(value, origin, findings);
                continue;
            }
            let Some(entries) = value.as_object() else {
                findings.push(Finding::error(
                    "shared.namespace",
                    origin,
                    format!("'{key}' must be an object of named values"),
                ));
                continue;
            };
            let ns = self.namespaces.entry(key.clone()).or_default();
            for (name, val) in entries {
                if ns.contains_key(name) {
                    findings.push(Finding::error(
                        "shared.duplicate",
                        origin,
                        format!("'{key}.{name}' is already defined elsewhere in the set"),
                    ));
                    continue;
                }
                ns.insert(name.clone(), val.clone());
            }
        }
    }

    fn merge_fragments(&mut self, value: &Value, origin: &str, findings: &mut Vec<Finding>) {
        let Some(entries) = value.as_object() else {
            findings.push(Finding::error(
                "shared.namespace",
                origin,
                "'fragments' must be an object of named task sequences",
            ));
            return;
        };
        for (name, spec) in entries {
            if self.fragments.contains_key(name) {
                findings.push(Finding::error(
                    "shared.duplicate",
                    origin,
                    format!("fragment '{name}' is already defined elsewhere in the set"),
                ));
                continue;
            }
            let Some(tasks) = spec.get("tasks").and_then(Value::as_array) else {
                findings.push(Finding::error(
                    "shared.fragment",
                    origin,
                    format!("fragment '{name}' has no 'tasks' array"),
                ));
                continue;
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
                    tasks: tasks.clone(),
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
    pub fn expand(&self, doc: &mut Value, origin: &str, findings: &mut Vec<Finding>) {
        super::compile::compile(
            doc,
            &super::compile::Cx {
                shared: self,
                origin,
            },
            findings,
        );
    }

    /// Replace every `{"use": ..}` entry with the named fragment's tasks.
    ///
    /// Not recursive: a fragment that includes a fragment needs cycle
    /// detection and a depth cap, and nothing has asked for it. Refused with a
    /// message rather than silently ignored, so the restriction is visible at
    /// the point it bites — at every depth of the fragment, not just its top
    /// level, which is where [`namespace_fragment_step`] enforces it.
    pub(super) fn expand_tasks(
        &self,
        tasks: &[Value],
        origin: &str,
        findings: &mut Vec<Finding>,
    ) -> Vec<Value> {
        let mut out = Vec::with_capacity(tasks.len());
        for task in tasks {
            let Some(name) = task.get("use").and_then(Value::as_str) else {
                // A task group holds steps of its own, and a fragment is as
                // usable inside a guard clause as outside one. The group test
                // is the engine's own, so a step this expander declines to
                // descend into is exactly one the flattener calls a task —
                // a malformed group is left alone here and reported as the
                // broken step it is by validation, rather than being quietly
                // reshaped on the way through.
                if crate::engine::is_group(task) {
                    let mut group = task.clone();
                    if let Some(inner) = task.get("tasks").and_then(Value::as_array) {
                        group["tasks"] = Value::Array(self.expand_tasks(inner, origin, findings));
                    }
                    out.push(group);
                    continue;
                }
                out.push(task.clone());
                continue;
            };
            let instance = task.get("id").and_then(Value::as_str).unwrap_or(name);
            let Some(fragment) = self.fragments.get(name) else {
                findings.push(Finding::error(
                    "closure.fragment",
                    format!("{origin} task '{instance}'"),
                    format!("fragment '{name}' is not defined in the set"),
                ));
                continue;
            };

            let args = self.fragment_args(fragment, task, name, instance, origin, findings);

            for inner in &fragment.tasks {
                let mut expanded = inner.clone();
                substitute_params(&mut expanded, &args);
                if namespace_fragment_step(&mut expanded, instance, name, findings) {
                    out.push(expanded);
                }
            }
        }
        out
    }

    /// A call site's arguments: declared defaults, overridden by `with`, with
    /// both an unsatisfied parameter and an unknown one reported.
    fn fragment_args(
        &self,
        fragment: &Fragment,
        task: &Value,
        name: &str,
        instance: &str,
        origin: &str,
        findings: &mut Vec<Finding>,
    ) -> BTreeMap<String, Value> {
        let supplied = task
            .get("with")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let declared: BTreeSet<&String> = fragment.params.keys().collect();

        for key in supplied.keys() {
            if !declared.contains(key) {
                findings.push(Finding::error(
                    "shared.fragment_param",
                    format!("{origin} task '{instance}'"),
                    format!("fragment '{name}' declares no parameter '{key}'"),
                ));
            }
        }

        let mut args = BTreeMap::new();
        for (param, default) in &fragment.params {
            match supplied.get(param).or(default.as_ref()) {
                Some(value) => {
                    args.insert(param.clone(), value.clone());
                }
                None => findings.push(Finding::error(
                    "shared.fragment_param",
                    format!("{origin} task '{instance}'"),
                    format!("fragment '{name}' requires parameter '{param}', which has no default"),
                )),
            }
        }
        args
    }

    /// Walk a value, splicing every `$from` against the namespaces.
    pub(super) fn splice(&self, value: &mut Value, origin: &str, findings: &mut Vec<Finding>) {
        match value {
            Value::Array(items) => {
                for item in items {
                    self.splice(item, origin, findings);
                }
            }
            Value::Object(map) => {
                for v in map.values_mut() {
                    self.splice(v, origin, findings);
                }
                let Some(path) = map.get("$from").and_then(Value::as_str).map(str::to_string)
                else {
                    return;
                };
                let replacement = match self.lookup(&path) {
                    Some(target) => apply_splice(map, target),
                    None => {
                        findings.push(Finding::error(
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
    fn lookup(&self, path: &str) -> Option<&Value> {
        let (namespace, key) = path.split_once('.')?;
        self.namespaces.get(namespace)?.get(key)
    }
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
    findings: &mut Vec<Finding>,
) -> Result<(), String> {
    super::set::walk_json_files(dir, &mut |path, parsed| match parsed {
        Ok(doc) => {
            if SharedDefinitions::is_shared_document(&doc) {
                out.push((path.display().to_string(), doc));
            }
        }
        Err(e) => findings.push(Finding::warning(
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

/// Prefix every id one fragment step contributes with the call-site id — at
/// **every** depth — and refuse a nested `use` wherever it sits. Returns
/// `false` when the step must be dropped.
///
/// Both halves used to stop at the fragment's top level, which is what made
/// the namespacing contract — "a fragment cannot collide with the including
/// workflow, or with a second instance of itself" — false for any fragment
/// holding a task group (#294). Only the group's own `id` was rewritten, so
/// the tasks inside it landed in the host workflow's namespace: using such a
/// fragment twice produced duplicate ids, and using it once collided with any
/// host task sharing a name with one of its nested tasks. The author could not
/// see either coming, because the colliding name is private to the fragment.
///
/// The group test is the engine's own, so a step this walk descends into is
/// exactly one the flattener calls a group. That is already the rule the
/// non-`use` branch of [`SharedDefinitions::expand_tasks`] follows, and the
/// discrepancy between the two branches *was* the bug.
///
/// Ids are prefixed flat — `{call-site}.{id}` regardless of depth — rather
/// than accumulating one segment per enclosing group. One rule, and ids stay
/// short: a step id is a metric label, a trace step id and a
/// `metadata.progress` key, and groups nest up to
/// [`MAX_STEP_DEPTH`](crate::engine::MAX_STEP_DEPTH). Flat prefixing also
/// keeps "a fragment is authored exactly like a workflow" true — a fragment
/// that reuses one id across two of its own groups still surfaces as a
/// duplicate, as it would if you inlined it by hand, where per-group
/// prefixing would silently mask it.
fn namespace_fragment_step(
    step: &mut Value,
    instance: &str,
    fragment: &str,
    findings: &mut Vec<Finding>,
) -> bool {
    // Checked after `substitute_params` rather than before it, which is
    // equivalent: both test for the key, so a `{"use": {"$param": ..}}` is
    // refused either way.
    if step.get("use").is_some() {
        findings.push(Finding::error(
            "shared.fragment_nested",
            format!("fragment '{fragment}'"),
            "a fragment cannot include another fragment",
        ));
        return false;
    }
    if let Some(id) = step.get("id").and_then(Value::as_str) {
        step["id"] = Value::String(format!("{instance}.{id}"));
    }
    if crate::engine::is_group(step)
        && let Some(members) = step.get_mut("tasks").and_then(Value::as_array_mut)
    {
        members.retain_mut(|member| namespace_fragment_step(member, instance, fragment, findings));
    }
    true
}

/// Replace `{"$param": "name"}` nodes with the call site's argument.
fn substitute_params(value: &mut Value, args: &BTreeMap<String, Value>) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(|v| substitute_params(v, args)),
        Value::Object(map) => {
            if map.len() == 1
                && let Some(name) = map.get("$param").and_then(Value::as_str)
                && let Some(arg) = args.get(name)
            {
                *value = arg.clone();
                return;
            }
            map.values_mut().for_each(|v| substitute_params(v, args));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn shared() -> (SharedDefinitions, Vec<Finding>) {
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

    /// The other half of #294: a fragment's *own* task list is not the only
    /// place a `use` can hide. Nested inside a group it escaped the
    /// no-nested-fragments refusal entirely and survived expansion, reaching
    /// the host workflow as a step the engine cannot parse.
    #[test]
    fn a_fragment_including_a_fragment_inside_a_group_is_refused() {
        let mut s = SharedDefinitions::default();
        let mut f = Vec::new();
        s.merge(
            &json!({ "fragments": {
                "outer": { "tasks": [
                    { "id": "span", "condition": true, "tasks": [
                        { "id": "i", "use": "inner" }] }] },
                "inner": { "tasks": [{ "id": "t", "name": "t",
                    "function": { "name": "map", "input": { "mappings": [] } } }] } } }),
            "common.json",
            &mut f,
        );
        let mut doc = json!({ "tasks": [{ "id": "o", "use": "outer" }] });
        s.expand(&mut doc, "wf.json", &mut f);
        assert!(
            f.iter().any(|x| x.check == "shared.fragment_nested"),
            "the restriction must be reported where it bites, not left to \
             surface as an uncompiled reference the set can actually resolve: {f:?}"
        );
        // Dropped, not carried through: a step that survived here would leave
        // source form in a compiled document.
        assert_eq!(doc["tasks"][0]["tasks"].as_array().map(Vec::len), Some(0));
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

    /// v1 refuses nesting rather than looping forever on a cycle.
    #[test]
    fn a_fragment_including_a_fragment_is_refused() {
        let mut s = SharedDefinitions::default();
        let mut f = Vec::new();
        s.merge(
            &json!({ "fragments": {
                "outer": { "tasks": [{ "id": "i", "use": "inner" }] },
                "inner": { "tasks": [{ "id": "t", "name": "t",
                    "function": { "name": "map", "input": { "mappings": [] } } }] } } }),
            "common.json",
            &mut f,
        );
        let mut doc = json!({ "tasks": [{ "id": "o", "use": "outer" }] });
        s.expand(&mut doc, "wf.json", &mut f);
        assert!(
            f.iter().any(|x| x.check == "shared.fragment_nested"),
            "{f:?}"
        );
    }
}
