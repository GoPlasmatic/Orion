use super::*;

// ============================================================
// Advisory: JSONLogic in a field that folds only `{"var": ..}`
// ============================================================

use serde_json::Value;

/// Every informational finding about a task array: what the engine reports and
/// does not refuse, plus the half of the escape story only Orion can see.
///
/// One pass. `check_workflow` answers all three codes at once, and the three
/// surfaces that ask — `lint`, the definition-set check, `preflight` — each
/// want the whole set, so asking twice built two engines and walked the
/// workflow twice to split one answer in half.
///
/// - `logic.escaped_template_key` — a `$`-prefixed key in a template position.
///   dataflow-rs 3.9 made the escape unconditional: one `$` comes off *every*
///   key in such a position, whether or not the name collides with an
///   operator. So `{"$set": …}` composed in a `map` — a MongoDB update
///   document, the commonest way this appears — goes out as `{"set": …}` and
///   the write replaces the document instead of updating it; `$ref` and
///   `$schema` in a composed JSON Schema are the same story. Nothing fails at
///   any gate. The fix is mechanical — double the prefix, `$$set` — which is
///   why it is worth reporting mechanically, and why the doubled spelling is
///   kept silent here: doubling is also how an author
///   *deliberately* emits a `$` key, and an author who has done the right
///   thing must not be left with a warning they cannot clear. This one has a
///   second half: a custom handler's `input` is a config document to
///   dataflow-rs, not a template — which of its fields are templates is the
///   handler's business — so `check_workflow` skips it, and
///   [`FunctionRegistry::template_paths`], where that business
///   is declared, is what closes the gap.
/// - `engine.unguarded_validation` — a `validation` task whose failure changes
///   nothing, because a failing rule records `400` and the executor's 4xx
///   branch carries on; `continue_on_error` governs `5xx` and `Err` only. This
///   is the shape that shipped a decorative CSRF check
///   ([#308](https://github.com/GoPlasmatic/Orion/issues/308)): the check read
///   correct, did nothing, and nothing said so. dataflow-rs 3.10 added
///   `"halt_on": "failure"` as the fix and this code as the warning.
/// - `engine.group_continue_on_error` — `continue_on_error` on a task *group*,
///   which parses and is then dropped. The key is real on a task and on a
///   workflow, which is what makes a group the one place it looks like it
///   should work.
///
/// Every one fires on a definition that builds and runs — that is the engine's
/// classification, not a choice made here — so what a caller does with the
/// severity is the caller's. Two of them are advisories everywhere;
/// `preflight` raises the escaped key to a break, because a stored workflow
/// carrying one changes the shape of what it writes the moment the instance is
/// upgraded. `engine::loader::screen_workflow` asks the same question on the
/// serving side, where the job is the opposite one — not to quarantine a
/// channel over any of them.
///
/// A bare builder: every finding here is structural, so no handler registry
/// and no secret store changes the answer, and the other issues such a builder
/// reports — an unregistered function, an undeclared secret — belong to checks
/// that own them and would be reported twice with worse wording.
pub fn engine_advisories(
    tasks: &Value,
    loop_config: Option<&Value>,
    functions: &FunctionRegistry,
) -> Vec<EngineAdvisory> {
    // The synthetic wrapper `validate_workflow_tasks_schema` uses, loop
    // included: the engine reads a loop's `setup` steps and its `over` as it
    // reads the body.
    let loop_config = loop_config.filter(|l| !l.is_null());
    let mut synthetic = serde_json::json!({
        "id": "__shape_check__", "name": "__shape_check__",
        "condition": true, "tasks": tasks,
    });
    if let Some(loop_config) = loop_config {
        synthetic["loop"] = loop_config.clone();
    }
    let Ok(workflow) = dataflow_rs_v3::Workflow::from_json(&synthetic.to_string()) else {
        // Unparseable tasks are reported with a better message by the schema
        // check; there is nothing to say about the shape of a document that is
        // not a workflow.
        return Vec::new();
    };
    let issues = dataflow_rs_v3::Engine::builder().check_workflow(&workflow);

    // Escaped keys first, then the walk that finds the ones the engine cannot,
    // then the shape findings — the order the three callers reported these in
    // when they were two functions called one after the other.
    let mut out: Vec<EngineAdvisory> = issues
        .iter()
        .filter(|issue| issue.code == dataflow_rs_v3::IssueCode::EscapedTemplateKey)
        .filter(|issue| issue.path.as_deref().is_none_or(is_accidental_escape))
        .map(|issue| EngineAdvisory {
            check: EngineAdvisory::ESCAPED_TEMPLATE_KEY,
            path: issue.path.clone().unwrap_or_else(|| "tasks".to_string()),
            message: issue.message.clone(),
        })
        .collect();

    for_each_input_field(tasks, loop_config, |function, field, path, value| {
        // Only for a name the engine treats as custom: a built-in's parameters
        // were already walked, and reporting them twice would be worse than
        // not reporting them at all.
        if dataflow_rs_v3::is_builtin_function(function) {
            return;
        }
        if !functions.template_paths(function, field).contains(&"") {
            return;
        }
        out.extend(
            escaped_keys_in(value)
                .into_iter()
                .map(|(suffix, message)| EngineAdvisory {
                    check: EngineAdvisory::ESCAPED_TEMPLATE_KEY,
                    path: format!("{path}{suffix}"),
                    message,
                }),
        );
    });

    // The keys the tensor family took in 1.8, where an author almost
    // certainly meant data: the constant ones. A dynamic one is a call an
    // author wrote on purpose on this surface; `preflight` asks for those
    // separately, because on a stored estate written before 1.8 it cannot
    // have been.
    out.extend(tensor_operator_key_advisories(
        tasks,
        loop_config,
        functions,
        TensorKeyScope::Constant,
    ));

    out.extend(issues.into_iter().filter_map(|issue| {
        // Everything else the engine calls advisory. Selected by severity
        // rather than by naming the codes, so this asks the same question
        // `screen_workflow` asks and cannot answer it differently. The escape
        // is already reported above, with a path of its own.
        if issue.severity() != dataflow_rs_v3::Severity::Advisory
            || issue.code == dataflow_rs_v3::IssueCode::EscapedTemplateKey
        {
            return None;
        }
        // The noun rides with the code because `task_id` carries a group's
        // id for `GROUP_CONTINUE_ON_ERROR` — the engine records a group on
        // the same field, having nowhere else to put it. Calling that step
        // a task contradicts the message beside it, which names the group.
        //
        // The catch-all is the point of selecting by severity: an advisory
        // code a later dataflow-rs adds is reported under a generic id rather
        // than dropped for want of a specific one. `task` is the safe noun —
        // every code but `GROUP_CONTINUE_ON_ERROR` records a task in
        // `task_id`. Unreachable until such a code exists, so there is nothing
        // to write a fixture against; the arm is the fixture.
        let (check, noun) = match issue.code {
            dataflow_rs_v3::IssueCode::UnguardedValidation => {
                (EngineAdvisory::UNGUARDED_VALIDATION, "task")
            }
            dataflow_rs_v3::IssueCode::GroupContinueOnError => {
                (EngineAdvisory::GROUP_CONTINUE_ON_ERROR, "group")
            }
            _ => (EngineAdvisory::UNCLASSIFIED, "task"),
        };
        // The engine's `path` for these two is the offending *field*
        // (`halt_on`, `continue_on_error`), not an authored coordinate —
        // the step is named by `task_id` instead. Joined here so one line
        // says which step and which key, which is what the other warning
        // reporters give and what a pipeline greps.
        let path = match (&issue.task_id, &issue.path) {
            (Some(id), Some(field)) => format!("{noun} '{id}'.{field}"),
            (Some(id), None) => format!("{noun} '{id}'"),
            (None, Some(field)) => field.clone(),
            (None, None) => "tasks".to_string(),
        };
        Some(EngineAdvisory {
            check,
            path,
            message: issue.message,
        })
    }));

    out
}

/// One informational finding from [`engine_advisories`].
///
/// Carries its own `check` id rather than sharing one, for the reason
/// `definitions::check` gives every finding one: a pipeline grandfathering a
/// single rule should not have to reach for `--deny-warnings` and silence the
/// rest. The ids are consts because a caller has to branch on one —
/// `preflight` treats the escaped key differently from the other two — and a
/// string literal repeated at a call site is how that stops matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineAdvisory {
    pub check: &'static str,
    pub path: String,
    pub message: String,
}

impl EngineAdvisory {
    /// A `$`-prefixed key the engine emits with one `$` stripped.
    pub const ESCAPED_TEMPLATE_KEY: &'static str = "logic.escaped_template_key";
    /// A `validation` whose failure stops nothing.
    pub const UNGUARDED_VALIDATION: &'static str = "engine.unguarded_validation";
    /// `continue_on_error` on a group, which the engine drops.
    pub const GROUP_CONTINUE_ON_ERROR: &'static str = "engine.group_continue_on_error";
    /// A single-key object in a template position whose key names a tensor
    /// operator (1.8), so it is evaluated as a call rather than emitted as
    /// the literal it was before the family existed.
    pub const TENSOR_OPERATOR_KEY: &'static str = "logic.tensor_operator_key";
    /// An advisory this version of Orion has no specific id for — a code a
    /// later dataflow-rs added. Reported rather than dropped: the serving
    /// screen already knows not to quarantine it, and an author should still
    /// hear what the engine said.
    pub const UNCLASSIFIED: &'static str = "engine.advisory";
}

/// Which tensor-named keys [`tensor_operator_key_advisories`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TensorKeyScope {
    /// Objects with no evaluated node inside them, whose evaluation as the
    /// operator call they now are fails — `{"shape": [6, 7]}`, `{"full": true}`.
    /// A constant that *does* evaluate (`{"zeros": [[2], "i64"]}`) is a
    /// working call and is not reported. This is `lint`'s scope, on every
    /// definition set for as long as the family exists.
    Constant,
    /// Objects that read the context somewhere inside — `{"shape": {"var":
    /// "data.dims"}}`. Before 1.8 this emitted `{"shape": <dims>}`; after it,
    /// `shape(dims)`. On a set an author is writing today it is a call; on
    /// a stored estate written before 1.8 it cannot be, which is why only
    /// `preflight` asks for these.
    Dynamic,
}

/// The keys the tensor family took in 1.8, in every position the engine
/// evaluates as a template — a `map` mapping's `logic`, and the fields a
/// custom function's table marks `template_at` — as advisories with the
/// `$`-escape remedy in the message.
///
/// A single-key object in a template position is an operator call in
/// templating mode, so a stored `{"shape": [6, 7]}` that was data before 1.8
/// is `shape([6, 7])` after it. The engine cannot report this the way it
/// reports `ESCAPED_TEMPLATE_KEY`: to it the key *is* an operator, and a call
/// with the wrong arguments is a run-time error, not an authoring issue. So
/// the walk is Orion's, over the same positions, naming the same remedy the
/// escape exists for. Multi-key objects are never operator calls and are left
/// alone; a key already escaped (`$shape`) is the fix and is not reported.
///
/// Conditions are not walked: they are compiled strictly, so a tensor-named
/// key there raised "Invalid operator" before 1.8 and could not have been
/// serving. Nor is a field the registry folds rather than evaluates (a
/// MongoDB `filter`, a cached `value`): a literal there is stored or sent as
/// written, tensor-named key and all. Unlike the escaped-key walk above, this
/// one does not defer built-in functions to the engine — the engine has
/// nothing to say about a key that is, to it, simply an operator — so
/// `http_call.body` and `map`'s mappings are walked by the same rule.
pub fn tensor_operator_key_advisories(
    tasks: &Value,
    loop_config: Option<&Value>,
    functions: &FunctionRegistry,
    scope: TensorKeyScope,
) -> Vec<EngineAdvisory> {
    let mut out = Vec::new();
    for_each_input_field(tasks, loop_config, |function, field, path, value| {
        if function == "map" {
            if field != "mappings" {
                return;
            }
            let Some(mappings) = value.as_array() else {
                return;
            };
            for (i, mapping) in mappings.iter().enumerate() {
                if let Some(logic) = mapping.get("logic") {
                    collect_tensor_keys(logic, &format!("{path}[{i}].logic"), scope, &mut out);
                }
            }
            return;
        }
        let template_paths = functions.template_paths(function, field);
        if template_paths.contains(&"") {
            collect_tensor_keys(value, path, scope, &mut out);
        } else if template_paths.contains(&"*")
            && let Some(members) = value.as_object()
        {
            for (member, v) in members {
                collect_tensor_keys(v, &format!("{path}.{member}"), scope, &mut out);
            }
        }
    });
    out
}

pub(super) fn collect_tensor_keys(
    value: &Value,
    path: &str,
    scope: TensorKeyScope,
    out: &mut Vec<EngineAdvisory>,
) {
    match value {
        Value::Object(map) => {
            if map.len() == 1 {
                let (key, inner) = map.iter().next().expect("one entry");
                if crate::engine::operators::is_tensor_operator(key) {
                    let dynamic = reads_the_context(inner);
                    let report = match scope {
                        TensorKeyScope::Constant => !dynamic && !evaluates_as_a_call(value),
                        TensorKeyScope::Dynamic => dynamic,
                    };
                    if report {
                        out.push(EngineAdvisory {
                            check: EngineAdvisory::TENSOR_OPERATOR_KEY,
                            path: format!("{path}.{key}"),
                            message: format!(
                                "`{key}` names a tensor operator since 1.8, so this object is \
                                 evaluated as a call to it rather than emitted as data; spell \
                                 the key `${key}` to keep the literal"
                            ),
                        });
                    }
                    // Whichever it is, what sits inside is the call's own
                    // argument list — or a literal the author will escape as
                    // a whole — so the walk stops here rather than reporting
                    // the arguments of a genuine call.
                    return;
                }
                collect_tensor_keys(inner, &format!("{path}.{key}"), scope, out);
                return;
            }
            for (key, inner) in map {
                collect_tensor_keys(inner, &format!("{path}.{key}"), scope, out);
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                collect_tensor_keys(item, &format!("{path}[{i}]"), scope, out);
            }
        }
        _ => {}
    }
}

/// Whether any node inside `value` is one the engine evaluates — a `var`, an
/// operator call — rather than data. The same question `is_operator` answers
/// for one key, asked of a whole tree.
pub(super) fn reads_the_context(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            if map.len() == 1
                && let Some(key) = map.keys().next()
                && crate::engine::operators::is_operator(key)
            {
                return true;
            }
            map.values().any(reads_the_context)
        }
        Value::Array(items) => items.iter().any(reads_the_context),
        _ => false,
    }
}

/// Whether a constant single-key object, read as the operator call it now is,
/// evaluates at all. `{"zeros": [[2], "i64"]}` does and is a working call;
/// `{"shape": [6, 7]}` does not — `shape` wants one tensor, not two numbers —
/// and so was certainly data. Evaluated on a bare, strict datalogic engine
/// carrying Orion's operators, against an empty context; a constant needs no
/// other.
pub(super) fn evaluates_as_a_call(value: &Value) -> bool {
    use dataflow_rs_v3::datalogic_rs as datalogic;
    static ENGINE: std::sync::OnceLock<datalogic::Engine> = std::sync::OnceLock::new();
    let engine = ENGINE.get_or_init(|| {
        crate::engine::operators::add_to_datalogic(datalogic::Engine::builder()).build()
    });
    let Ok(compiled) = engine.compile(value) else {
        return false;
    };
    engine
        .session()
        .eval_into::<Value, _>(&compiled, &Value::Object(Default::default()))
        .is_ok()
}

/// Ask the engine which keys in one arbitrary value it would strip a `$` from,
/// as `(path suffix relative to the value, message)`.
///
/// The walk that answers this is dataflow-rs's and is not public on its own, so
/// the question is put the only way it can be: as a workflow with a single
/// `map` mapping whose `logic` *is* the value. That is a real template position,
/// so the answer is the engine's own — no second implementation of the rule to
/// drift from it — and the fixed shape of the wrapper is what makes the
/// reported path mechanically strippable back to a suffix.
pub(super) fn escaped_keys_in(value: &Value) -> Vec<(String, String)> {
    const PREFIX: &str = "function.input.mappings[0].logic";
    let synthetic = serde_json::json!({
        "id": "__template_key_check__", "name": "__template_key_check__",
        "condition": true,
        "tasks": [{
            "id": "t", "name": "t",
            "function": { "name": "map", "input": { "mappings": [
                { "path": "data.__probe__", "logic": value }
            ] } }
        }],
    });
    let Ok(workflow) = dataflow_rs_v3::Workflow::from_json(&synthetic.to_string()) else {
        return Vec::new();
    };
    let builder = dataflow_rs_v3::Engine::builder();
    builder
        .check_workflow(&workflow)
        .into_iter()
        .filter(|issue| issue.code == dataflow_rs_v3::IssueCode::EscapedTemplateKey)
        .filter_map(|issue| {
            let path = issue.path?;
            let suffix = path.strip_prefix(PREFIX)?;
            is_accidental_escape(&path).then(|| (suffix.to_string(), issue.message))
        })
        .collect()
}

/// Whether the key at the end of an `ESCAPED_TEMPLATE_KEY` path looks like one
/// the author did not mean to escape.
///
/// The engine reports *every* `$`-prefixed key, which is right for an audit and
/// wrong for a gate: `$$set` is the documented fix, so warning about it leaves
/// an author who has already fixed the problem with a warning they cannot
/// clear and a `--deny-warnings` build that can never go green. One leading
/// `$` is the accident — a MongoDB operator, a JSON Schema keyword, written by
/// someone who did not know the prefix was load-bearing. Two or more is a
/// decision, and this codebase would rather stay silent than be wrong.
///
/// The key is the path's last segment, in the `{path}.{key}` shape the engine
/// builds. A key containing a literal `.` makes that ambiguous — for every
/// path-based consumer, not just this one — and the failure is to stay silent
/// about a real one, never to invent a warning.
pub(super) fn is_accidental_escape(path: &str) -> bool {
    let key = path.rsplit('.').next().unwrap_or(path);
    key.starts_with('$') && !key.starts_with("$$")
}

/// Warn about JSONLogic nodes in connector-payload fields that nothing will
/// evaluate.
///
/// A `resolvable` field folds `{"var": ..}` nodes and **nothing else** — the
/// house rule for every connector handler. Any other operator node is a literal
/// from the handler's point of view: `mongo_write` stores `{"if": […]}` in the
/// document as a BSON object, a `filter` carrying one matches no rows, and a
/// stubbed test of either stays green. There is no error at write time and,
/// until the call log, nothing a test could assert on.
///
/// **A warning, never an error, and deliberately so.** The operator vocabulary
/// includes `length`, `type`, `in`, `keys`, `sort`, `map` and `join`, which are
/// ordinary field names in ordinary documents; and Orion is a rules engine, so
/// a document that legitimately *contains* a stored JSONLogic rule is a real
/// use case rather than a hypothetical. The array-argument test below removes
/// most of the noise (`{"length": 120}` is data, `{"cat": […]}` is not), but
/// not all of it — and a hard error would additionally refuse updates to
/// workflows that have been serving for months. `warn_on_unwritten_reads` is
/// the same shape for the same kind of reason.
///
/// Returns `(field path, message)` pairs — the tuple shape
/// [`crate::engine::functions::schema::StaticValidator`] already uses, because
/// `ValidationIssue` belongs to the admin routes and the CLI cannot see it.
pub fn unresolvable_logic_warnings(
    tasks: &Value,
    loop_config: Option<&Value>,
    functions: &FunctionRegistry,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for_each_input_field(tasks, loop_config, |function, field, path, value| {
        if functions.is_resolvable_field(function, field) {
            collect_unresolvable(value, path, function, &mut out);
        }
    });
    out
}

/// Call `visit(function name, field name, authored path, value)` for every
/// field of every task input in `tasks`.
///
/// The walk itself — flatten the steps so a task inside a guard clause is
/// reached like any other, then destructure `function.name` / `function.input`
/// — is the same for every check that reads task inputs, and getting it wrong
/// is silent: a flat `tasks.as_array()` loop skips everything inside a task
/// group. One copy, so `walk_steps` is reached from one place.
pub(super) fn for_each_input_field(
    tasks: &Value,
    loop_config: Option<&Value>,
    mut visit: impl FnMut(&str, &str, &str, &Value),
) {
    for (path, task) in crate::engine::walk_steps(tasks, loop_config.filter(|l| !l.is_null())).tasks
    {
        let Some(function) = task.get("function") else {
            continue;
        };
        let (Some(name), Some(input)) = (
            function.get("name").and_then(Value::as_str),
            function.get("input").and_then(Value::as_object),
        ) else {
            continue;
        };
        for (field, value) in input {
            visit(
                name,
                field,
                &format!("{path}.function.input.{field}"),
                value,
            );
        }
    }
}

/// Every secret reference sitting in a field that does not resolve one.
///
/// `env://NAME` and `vault://…` are resolved by the handler that reads a
/// particular *path*, not by a pass over the document — so a handful of paths
/// turn one into a credential (`schema::secret_paths`) and everywhere else the
/// string is sent on as itself. A task carrying `{"path": "env://API_BASE"}`
/// requests a URL spelled `env://API_BASE` and fails with whatever the backend
/// makes of it, which is a failure that names neither the reference nor the
/// field.
///
/// The exemption is per path, not per field: `jwt_verify.keys` resolves a
/// reference at `keys[].key` and nowhere else, so a reference in a sibling
/// `kid` — which the handler matches verbatim against the token's — is still
/// reported.
///
/// **An error, not a warning** — the opposite call from
/// [`unresolvable_logic_warnings`], and for the opposite reason. That check is
/// advisory because operator names (`in`, `map`, `length`) are also ordinary
/// field names, so it cannot tell a mistake from a document. `env://` at the
/// head of a string has no second reading: nothing legitimately sends that text
/// to a database, an SMTP server or an HTTP path.
///
/// Returns `(field path, message)` pairs, the shape the admin routes and the
/// CLI both already consume.
pub fn secret_reference_errors(
    tasks: &Value,
    loop_config: Option<&Value>,
    functions: &FunctionRegistry,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for_each_input_field(tasks, loop_config, |function, field, path, value| {
        let exempt = functions.secret_paths(function, field);
        // Whether a `{"secret": ..}` node in this field is a mistake — see the
        // branch it gates in [`collect_secret_references`]. Three cases, and
        // only the middle one is reportable:
        //
        // * A field the engine **evaluates** (`template_at`) resolves the node,
        //   because `secret` is a reserved JSONLogic operator registered on
        //   every engine. Nothing is wrong, so this stays false for those.
        // * A field the handler **folds** (`resolvable`) looks inside but folds
        //   `{"var": ..}` only, so the node survives as an object and is sent
        //   to the backend as one. That is the mistake this reports.
        // * A field read **literally** is opaque data, where a member named
        //   `secret` is a member named `secret` — and the engine would not have
        //   resolved it either, so there is nothing to warn about.
        //
        // A plugin function is the exception to the third case: every one of
        // its fields is looked inside. Its world has no way to use key
        // material and every byte it receives it may return, so a
        // `{"secret": ..}` anywhere in its input is refused — and in a
        // `template_at` field, where the engine *would* evaluate the node,
        // this refusal is what keeps the secret out of the guest's input.
        let plugin = functions
            .get(function)
            .is_some_and(|e| e.source == crate::engine::functions::schema::Source::Plugin);
        let inspects_nodes =
            plugin || functions.is_resolvable_field(function, field) || !exempt.is_empty();
        collect_secret_references(
            value,
            path,
            "",
            exempt,
            function,
            inspects_nodes,
            plugin,
            &mut out,
        );
    });
    out
}

/// Walk one authored value, reporting every secret reference inside it.
///
/// `rel` is the position within the field being walked, in the notation
/// `FieldSchema::secret_at` uses: `""` at the field root, `"[]"` for any array
/// element, `".name"` for an object member. A node whose `rel` is listed in
/// `exempt` is where the handler resolves key material, so it and everything
/// under it is left alone.
#[allow(clippy::too_many_arguments)]
pub(super) fn collect_secret_references(
    value: &Value,
    path: &str,
    rel: &str,
    exempt: &[&str],
    function: &str,
    inspects_nodes: bool,
    plugin: bool,
    out: &mut Vec<(String, String)>,
) {
    if exempt.contains(&rel) {
        return;
    }
    // A plugin function: the node is refused for a different reason than the
    // one below — not that it would be sent on unresolved, but that in a
    // template field it *would* be resolved, into the input of a guest that
    // must never hold key material.
    if plugin && let Some(name) = crate::engine::functions::secret_ref::secret_name(value) {
        out.push((
            path.to_string(),
            format!(
                "'{function}' is a plugin function, and a plugin never sees key material: \
                 {{\"secret\": \"{name}\"}} is refused anywhere in its input. Read the secret \
                 in a built-in that takes one, or pass the plugin a value derived from it."
            ),
        ));
        return;
    }
    // A `{"secret": "name"}` node in a field that does not read key material.
    //
    // The consequence is worse than a stray operator. In a field the handler
    // reads literally, the node is stored and sent on **as the object it is**:
    // a database gets `{"secret":"api_key"}` as a bind parameter, an SMTP
    // server gets it as a subject line. The author asked for a credential and
    // the backend received a JSON object naming one.
    //
    // Recognised with the engine's own predicate, so this names exactly the
    // shape that would have resolved somewhere it is read — including the
    // one-element array spelling.
    //
    // **Only in a field the handler looks inside** (`inspects_nodes`), and
    // that qualifier is what keeps this an error rather than an advisory.
    // Unlike `env://` at the head of a string, which has no second reading, a
    // single-key object is the *ordinary* shape of keyed data: `data_query`'s
    // `sort` is `[{"<column>": "asc"}]`, so a column named `secret` authors as
    // `{"secret": "asc"}` and means nothing of the kind. A field is only
    // looked inside when the function declares it `resolvable` (it folds
    // expression nodes) or gives it a `secret_at` (it reads key material out
    // of one) — and in a field with neither, the engine would not have
    // resolved the node either, so there is nothing to warn about. Same
    // reasoning as `unresolvable_logic_warnings` staying advisory because
    // operator names are also ordinary field names; here the answer is to
    // narrow where the rule speaks rather than to soften what it says.
    if inspects_nodes && let Some(name) = crate::engine::functions::secret_ref::secret_name(value) {
        out.push((
            path.to_string(),
            format!(
                "'{function}' does not read key material in this field, so \
                 {{\"secret\": \"{name}\"}} is stored and sent on as that object rather \
                 than resolved. Secrets are read only where a function takes a key; \
                 for a deployment value elsewhere, declare it under [vars] and read it \
                 as {{\"var\": \"metadata.vars.<name>\"}}."
            ),
        ));
        return;
    }
    match value {
        Value::String(s) => {
            // The masking policy's predicate, not a `starts_with("env://")`:
            // it is the one place that decides which schemes this build
            // understands, so a `vault://` reference is caught here too rather
            // than reaching the backend as its own text.
            if crate::connector::secrets::is_resolvable_reference(s) {
                out.push((
                    path.to_string(),
                    format!(
                        "'{function}' does not resolve secret references in this field, so \
                         '{s}' is sent on as that literal text. Move the value to a \
                         connector, or declare it in the config file — a deployment value \
                         under [vars], read as {{\"var\": \"metadata.vars.<name>\"}}, and key \
                         material under [secrets], read as {{\"secret\": \"<name>\"}} in one \
                         of the fields that take it."
                    ),
                ));
            }
        }
        Value::Object(map) => {
            for (key, child) in map {
                collect_secret_references(
                    child,
                    &format!("{path}.{key}"),
                    &format!("{rel}.{key}"),
                    exempt,
                    function,
                    inspects_nodes,
                    plugin,
                    out,
                );
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                collect_secret_references(
                    child,
                    &format!("{path}[{i}]"),
                    &format!("{rel}[]"),
                    exempt,
                    function,
                    inspects_nodes,
                    plugin,
                    out,
                );
            }
        }
        _ => {}
    }
}

/// Walk one authored value, reporting every operator node inside it.
pub(super) fn collect_unresolvable(
    value: &Value,
    path: &str,
    function: &str,
    out: &mut Vec<(String, String)>,
) {
    match value {
        Value::Object(map) => {
            if map.len() == 1 {
                let (key, arg) = map.iter().next().expect("len checked");
                // The one node that *is* folded. Its argument is a path, not an
                // expression, so there is nothing below it to walk.
                if key == "var" {
                    return;
                }
                // `val` is the trap this check most wants to catch: a
                // documented operator, spelled one letter away from the one
                // that works, that folds to nothing here.
                if key == "val" {
                    out.push((
                        path.to_string(),
                        format!(
                            "'{function}' folds {{\"var\": ..}} nodes only, so {{\"val\": ..}} is \
                             stored verbatim — write {{\"var\": ..}} here"
                        ),
                    ));
                    return;
                }
                // Every multi-argument operator takes an array. Requiring one
                // is what keeps `{"length": 120}` — a plain data field — out of
                // the report.
                if arg.is_array() && crate::engine::operators::is_operator(key) {
                    out.push((
                        path.to_string(),
                        format!(
                            "'{function}' folds {{\"var\": ..}} nodes only, so the '{key}' \
                             expression here is never evaluated — it is written through as a \
                             literal object. Compute it in a 'map' task first and reference the \
                             result with {{\"var\": ..}}."
                        ),
                    ));
                    return;
                }
            }
            for (key, child) in map {
                collect_unresolvable(child, &format!("{path}.{key}"), function, out);
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                collect_unresolvable(child, &format!("{path}[{i}]"), function, out);
            }
        }
        _ => {}
    }
}
