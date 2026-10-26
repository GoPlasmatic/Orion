use super::*;

// ============================================================
// Advisory: JSONLogic in a field that folds only `{"var": ..}`
// ============================================================

use dataflow_rs::program::{IssueCode, Severity, StepKind};
use serde_json::Value;

/// Every informational finding about a task array: what the engine reports and
/// does not refuse, plus the half of the escape story only Orion can see.
///
/// One pass. The engine's parse and its check answer every code at once, and
/// the three surfaces that ask — `lint`, the definition-set check,
/// `preflight` — each want the whole set.
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
///   second half: a host function's `input` is a config document to
///   dataflow-rs, not a template — which of its fields are templates is the
///   function's business — so the bare check here skips it, and
///   [`FunctionRegistry::template_paths`], where that business
///   is declared, is what closes the gap.
/// - `engine.unguarded_validation` — a `validation` task whose failure changes
///   nothing, because a failing rule records `400` and its `on_rejection` is
///   `continue`. This is the shape that shipped a decorative CSRF check
///   ([#308](https://github.com/GoPlasmatic/Orion/issues/308)): the check read
///   correct, did nothing, and nothing said so. `"on_rejection": "halt"` is
///   the fix.
///
/// Every one fires on a definition that builds and runs — that is the engine's
/// classification, not a choice made here — so what a caller does with the
/// severity is the caller's. Two of them are advisories everywhere;
/// `preflight` raises the escaped key to a break, because a stored workflow
/// carrying one changes the shape of what it writes the moment the instance is
/// upgraded. `engine::quarantines` asks the same question on the serving
/// side, where the job is the opposite one — not to quarantine a channel over
/// any of them.
///
/// A bare compiler: every finding here is structural, so no function registry
/// and no secret store changes the answer, and the other issues such a
/// compiler reports — an unregistered function, an undeclared secret — belong
/// to checks that own them and would be reported twice with worse wording.
pub fn engine_advisories(
    tasks: &Value,
    loop_config: Option<&Value>,
    functions: &FunctionRegistry,
) -> Vec<EngineAdvisory> {
    // The synthetic wrapper `validate_workflow_tasks_schema` uses, loop
    // included: the engine reads a loop's `setup` steps and its `over` as it
    // reads the body.
    let loop_config = loop_config.filter(|l| !l.is_null());
    let Some(issues) = engine_issues(&shape_document(tasks, loop_config)) else {
        // Unparseable tasks are reported with a better message by the schema
        // check; there is nothing to say about the shape of a document that is
        // not a workflow.
        return Vec::new();
    };

    // Escaped keys first, then the walk that finds the ones the engine cannot,
    // then the shape findings — the order the three callers reported these in
    // when they were two functions called one after the other.
    let mut out: Vec<EngineAdvisory> = issues
        .iter()
        .filter(|issue| issue.code == IssueCode::EscapedTemplateKey)
        .filter(|issue| is_accidental_escape(&issue.path.to_string()))
        .map(|issue| EngineAdvisory {
            check: EngineAdvisory::ESCAPED_TEMPLATE_KEY,
            path: issue.path.to_string(),
            message: issue.message.clone(),
        })
        .collect();

    for_each_input_field(tasks, loop_config, |function, field, path, value| {
        // Only for a name the engine treats as custom: a built-in's parameters
        // were already walked, and reporting them twice would be worse than
        // not reporting them at all.
        if dataflow_rs::def::BUILTIN_FUNCTIONS.contains(&function) {
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

    out.extend(issues.into_iter().filter_map(|issue| {
        // Everything else the engine calls advisory. Selected by severity
        // rather than by naming the codes, so this asks the same question
        // `engine::quarantines` asks and cannot answer it differently. The
        // escape is already reported above, with a path of its own.
        if issue.severity != Severity::Advisory || issue.code == IssueCode::EscapedTemplateKey {
            return None;
        }
        // The catch-all is the point of selecting by severity: an advisory
        // code with no id of its own is reported under a generic one rather
        // than dropped for want of a specific one.
        let check = match issue.code {
            IssueCode::UnguardedValidation => EngineAdvisory::UNGUARDED_VALIDATION,
            _ => EngineAdvisory::UNCLASSIFIED,
        };
        // The step and the offending key on one line — `task 'check'.on_rejection`
        // — which is what the other warning reporters give and what a
        // pipeline greps. A group is named as one.
        let noun = match issue.step_kind {
            Some(StepKind::Group) => "group",
            _ => "task",
        };
        let at = issue.path.to_string();
        let key = at.rsplit('.').next().unwrap_or(&at);
        let path = match issue.step_id.as_deref() {
            Some(id) if !id.is_empty() => format!("{noun} '{id}'.{key}"),
            _ if issue.path.is_root() => "tasks".to_string(),
            _ => at.clone(),
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
    /// An advisory this version of Orion has no specific id for — a code a
    /// later dataflow-rs added. Reported rather than dropped: the serving
    /// screen already knows not to quarantine it, and an author should still
    /// hear what the engine said.
    pub const UNCLASSIFIED: &'static str = "engine.advisory";
}

/// Every issue the engine's parse and its bare check find in `workflow`, in
/// that order; `None` when the parse refuses it, since there is nothing to
/// check in a document that is not a workflow.
fn engine_issues(workflow: &Value) -> Option<Vec<dataflow_rs::Issue>> {
    static COMPILER: std::sync::LazyLock<dataflow_rs::program::Compiler> =
        std::sync::LazyLock::new(|| {
            dataflow_rs::Registry::new(crate::engine::kit::test_exprs())
                .compiler(dataflow_rs::program::CompileOptions::default())
        });
    let parsed = dataflow_rs::def::parse(workflow, crate::engine::parse_options());
    let def = parsed.def?;
    let mut issues = parsed.issues;
    issues.extend(COMPILER.check(&def));
    Some(issues)
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
    let Some(issues) = engine_issues(&synthetic) else {
        return Vec::new();
    };
    issues
        .into_iter()
        .filter(|issue| issue.code == IssueCode::EscapedTemplateKey)
        .filter_map(|issue| {
            let path = issue.path.to_string();
            let suffix = path.strip_prefix(PREFIX)?.to_string();
            is_accidental_escape(&path).then_some((suffix, issue.message))
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
/// from the handler's point of view: `cache_write` stores `{"if": […]}` in the
/// cached value as an object, a `db_read` param carrying one binds as a
/// literal, and a stubbed test of either stays green. There is no error at write time and,
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
            &SecretField {
                exempt,
                function,
                inspects_nodes,
                plugin,
            },
            &mut out,
        );
    });
    out
}

/// The field a secret-reference walk is in: the positions where its function
/// resolves key material (`exempt`), the function, whether the function looks
/// inside the field at all, and whether it is a plugin function.
pub(super) struct SecretField<'a> {
    pub exempt: &'a [&'a str],
    pub function: &'a str,
    pub inspects_nodes: bool,
    pub plugin: bool,
}

/// Walk one authored value, reporting every secret reference inside it.
///
/// `rel` is the position within the field being walked, in the notation
/// `FieldSchema::secret_at` uses: `""` at the field root, `"[]"` for any array
/// element, `".name"` for an object member. A node whose `rel` is listed in
/// `exempt` is where the handler resolves key material, so it and everything
/// under it is left alone.
pub(super) fn collect_secret_references(
    value: &Value,
    path: &str,
    rel: &str,
    field: &SecretField<'_>,
    out: &mut Vec<(String, String)>,
) {
    let &SecretField {
        exempt,
        function,
        inspects_nodes,
        plugin,
    } = field;
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
                    field,
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
                    field,
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
