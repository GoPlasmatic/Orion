use crate::engine::FunctionRegistry;
use crate::errors::{FieldError, OrionError};
use crate::storage::repositories::workflows::{CreateWorkflowRequest, UpdateWorkflowRequest};
use serde_json::Value;

use super::common::{uncompiled_source_errors, validate_description, validate_id, validate_name};

pub fn validate_create_workflow(
    req: &CreateWorkflowRequest,
    max_loop_iterations: i64,
    functions: &FunctionRegistry,
) -> Result<(), OrionError> {
    if let Some(ref id) = req.workflow_id {
        validate_id(id, "workflow.workflow_id")?;
    }
    validate_name(&req.name, "workflow.name")?;
    if let Some(ref desc) = req.description {
        validate_description(desc, "workflow.description")?;
    }
    // Before the schema walk, not after: an uncompiled `$from` reaches that
    // walk as literal JSON and is refused for the fields it would have
    // supplied, which is an error describing the symptom and hiding the cause.
    let source = source_form_errors(
        Some(&req.tasks),
        Some(&req.condition),
        req.loop_config.as_ref(),
    );
    if !source.is_empty() {
        return Err(uncompiled(source));
    }
    if let Some(loop_config) = &req.loop_config {
        let loop_errors = validate_workflow_loop_schema(loop_config, max_loop_iterations);
        if !loop_errors.is_empty() {
            return Err(validation_with_details(
                "Workflow loop is invalid",
                loop_errors,
            ));
        }
    }
    reject_task_errors(validate_workflow_tasks_schema(
        &req.tasks,
        req.loop_config.as_ref(),
        functions,
    ))?;
    reject_stray_secret_references(&req.tasks, req.loop_config.as_ref(), functions)
}

/// Refuse a workflow whose steps [`validate_workflow_tasks_schema`] found
/// fault with, summarised by where the faults are: the loop's own fields and
/// its `setup` steps, or the body.
fn reject_task_errors(errors: Vec<FieldError>) -> Result<(), OrionError> {
    if errors.is_empty() {
        return Ok(());
    }
    let summary = if errors.iter().all(|e| e.path.starts_with("loop")) {
        "Workflow loop is invalid"
    } else {
        "Workflow tasks contain invalid function inputs"
    };
    Err(validation_with_details(summary, errors))
}

/// Refuse a workflow carrying a secret reference in a field that resolves
/// none — [`secret_reference_errors`] as the create/update paths' refusal, so
/// both spell the code and the summary once.
fn reject_stray_secret_references(
    tasks: &Value,
    loop_config: Option<&Value>,
    functions: &FunctionRegistry,
) -> Result<(), OrionError> {
    let refs: Vec<FieldError> = secret_reference_errors(tasks, loop_config, functions)
        .into_iter()
        .map(|(path, message)| FieldError::new(path, "UNRESOLVED_SECRET_REF", message))
        .collect();
    if refs.is_empty() {
        return Ok(());
    }
    Err(validation_with_details(
        "Workflow sends a secret reference somewhere it is never resolved",
        refs,
    ))
}

/// `stored` is the draft the update applies to, when the caller has it. An
/// update carrying only one of `tasks` and `loop` keeps the stored other half,
/// and the two share one step-id namespace, so each is checked against the
/// other as it will be written: a new body step can collide with a stored
/// setup step and the reverse. Without `stored`, only the request's own half
/// is checked.
pub fn validate_update_workflow(
    req: &UpdateWorkflowRequest,
    stored: Option<&crate::storage::models::Workflow>,
    max_loop_iterations: i64,
    functions: &FunctionRegistry,
) -> Result<(), OrionError> {
    if let Some(ref name) = req.name {
        validate_name(name, "workflow.name")?;
    }
    if let Some(ref desc) = req.description {
        validate_description(desc, "workflow.description")?;
    }
    let source = source_form_errors(
        req.tasks.as_ref(),
        req.condition.as_ref(),
        req.loop_config.as_ref(),
    );
    if !source.is_empty() {
        return Err(uncompiled(source));
    }
    // `null` clears the loop and needs no checking; anything else is a config
    // that has to hold up.
    if let Some(loop_config) = &req.loop_config
        && !loop_config.is_null()
    {
        let loop_errors = validate_workflow_loop_schema(loop_config, max_loop_iterations);
        if !loop_errors.is_empty() {
            return Err(validation_with_details(
                "Workflow loop is invalid",
                loop_errors,
            ));
        }
    }
    let loop_changed = matches!(&req.loop_config, Some(l) if !l.is_null());
    if req.tasks.is_some() || loop_changed {
        // The steps as they will be written: the request's half where it
        // carries one, the stored draft's otherwise. With no body to check a
        // new loop against (no stored draft given), the loop's own fields
        // above are all this request can speak for.
        let stored_tasks = stored.and_then(|w| serde_json::from_str::<Value>(&w.tasks_json).ok());
        let stored_loop = stored
            .and_then(|w| w.loop_json.as_deref())
            .and_then(|json| serde_json::from_str::<Value>(json).ok());
        let loop_config = match &req.loop_config {
            Some(l) if l.is_null() => None,
            Some(l) => Some(l),
            None => stored_loop.as_ref(),
        };
        if let Some(tasks) = req.tasks.as_ref().or(stored_tasks.as_ref()) {
            reject_task_errors(validate_workflow_tasks_schema(
                tasks,
                loop_config,
                functions,
            ))?;
            reject_stray_secret_references(tasks, loop_config, functions)?;
        }
    }
    Ok(())
}

/// The keys dataflow-rs reads on a `loop` (3.14).
const LOOP_FIELDS: &[&str] = &[
    "counter",
    "init",
    "increment",
    "max",
    "over",
    "as",
    "scratch",
    "setup",
];

/// The keys dataflow-rs reads on a task's `for_each` (3.14).
const FOR_EACH_FIELDS: &[&str] = &["over", "as", "max_concurrency", "collect", "into"];

/// A key the engine does not read, each as an `UNKNOWN_FIELD` at `at.<key>`.
///
/// dataflow-rs parses neither `loop` nor `for_each` with
/// `deny_unknown_fields`, so a typo there is dropped without a word:
/// `max_concurency: 8` runs one call at a time and `steup` runs no setup
/// (#351). The engine's answer is to ignore it; an author's is almost always
/// a misspelling, which is what the suggestion is for.
fn unknown_keys(
    obj: &serde_json::Map<String, Value>,
    at: &str,
    noun: &str,
    known: &[&str],
) -> Vec<FieldError> {
    obj.keys()
        .filter(|key| !known.contains(&key.as_str()))
        .map(|key| {
            let hint = known
                .iter()
                .map(|k| (crate::text::edit_distance(key, k), *k))
                .filter(|(d, _)| *d <= 2)
                .min()
                .map(|(_, k)| format!(" Did you mean '{k}'?"))
                .unwrap_or_default();
            FieldError::new(
                format!("{at}.{key}"),
                "UNKNOWN_FIELD",
                format!(
                    "'{key}' is not a {noun} field, and the engine would ignore it.{hint} \
                     Known fields: {}.",
                    known.join(", ")
                ),
            )
        })
        .collect()
}

/// Validate a workflow's `loop` object.
///
/// Every structural rule here mirrors `LoopConfig::validate` in dataflow-rs,
/// which runs at `Engine::build()` time. That timing is the whole reason this
/// exists: a loop the engine refuses does not fail the request that wrote it,
/// it fails the *reload*, and a reload that cannot build takes every channel
/// on every node with it. Catching the same conditions here turns a
/// cluster-wide outage into a `400` on one API call — the same bargain
/// [`validate_workflow_tasks_schema`] strikes for duplicate task ids.
///
/// The ceiling is Orion's own, not the engine's. dataflow-rs requires `max`
/// but does not bound it; a sweep can call a connector, so an unbounded `max`
/// is a request that holds pool connections until the channel timeout fires.
/// `0` disables the check.
pub fn validate_workflow_loop_schema(
    loop_config: &serde_json::Value,
    max_loop_iterations: i64,
) -> Vec<FieldError> {
    let Some(obj) = loop_config.as_object() else {
        return vec![FieldError::new(
            "loop",
            "INVALID",
            "Workflow 'loop' must be an object with at least a 'max' — or absent, \
             which runs the task list exactly once",
        )];
    };

    let mut errors = unknown_keys(obj, "loop", "loop", LOOP_FIELDS);

    // Read the three numbers up front: `max`'s rules are stated against
    // `init`, so a bad `init` has to be known before `max` is judged.
    let init = match obj.get("init") {
        None => Some(0),
        Some(v) => match v.as_i64() {
            Some(n) => Some(n),
            None => {
                errors.push(FieldError::new(
                    "loop.init",
                    "INVALID",
                    "Loop 'init' must be an integer — it is the counter's first value \
                     (default 0)",
                ));
                None
            }
        },
    };

    match obj.get("increment") {
        None => {}
        Some(v) => match v.as_i64() {
            Some(n) if n < 1 => errors.push(FieldError::new(
                "loop.increment",
                "INVALID",
                format!(
                    "Loop 'increment' must be at least 1, got {n} — a counter that does \
                     not advance would never reach 'max'"
                ),
            )),
            Some(_) => {}
            None => errors.push(FieldError::new(
                "loop.increment",
                "INVALID",
                "Loop 'increment' must be an integer of at least 1 (default 1)",
            )),
        },
    }

    match obj.get("max") {
        None => errors.push(FieldError::new(
            "loop.max",
            "REQUIRED",
            "Loop 'max' is required — it is the upper bound that makes termination \
             structural rather than a property of the condition being written correctly",
        )),
        Some(v) => match v.as_i64() {
            Some(max) => {
                if let Some(init) = init
                    && max <= init
                {
                    errors.push(FieldError::new(
                        "loop.max",
                        "INVALID",
                        format!(
                            "Loop 'max' ({max}) must be greater than 'init' ({init}) — the \
                             bound is half-open, so this could never run a sweep"
                        ),
                    ));
                }
                if max_loop_iterations > 0 && max > max_loop_iterations {
                    errors.push(FieldError::new(
                        "loop.max",
                        "INVALID",
                        format!(
                            "Loop 'max' ({max}) exceeds the configured ceiling of \
                             {max_loop_iterations} — raise engine.max_loop_iterations if this \
                             workload genuinely needs more sweeps"
                        ),
                    ));
                }
            }
            None => errors.push(FieldError::new(
                "loop.max",
                "INVALID",
                "Loop 'max' must be an integer",
            )),
        },
    }

    if let Some(counter) = obj.get("counter")
        && !counter.is_null()
    {
        match counter.as_str() {
            Some(path) if path.is_empty() || path.split('.').any(str::is_empty) => {
                errors.push(FieldError::new(
                    "loop.counter",
                    "INVALID",
                    format!(
                        "Loop 'counter' must be a non-empty temp_data field path, got \
                         {path:?} — \"i\" writes temp_data.i, and dots nest"
                    ),
                ));
            }
            Some(_) => {}
            None => errors.push(FieldError::new(
                "loop.counter",
                "INVALID",
                "Loop 'counter' must be a string naming a temp_data field, or absent to \
                 bound the loop without exposing the count",
            )),
        }
    }

    errors
}

/// Walk the `tasks` array and collect validation errors: each task's identity,
/// then its `function.input` against the schema registered for `function.name`.
///
/// R5: an unknown `function.name` is a hard error — the function set is closed,
/// so such a workflow can only fail at its first request.
///
/// R20: `POST /admin/workflows/validate` reaches this through
/// `validate_create_workflow`, which is the point. This function was already
/// documented as *"public so the `/validate` endpoint can reuse it"* and had
/// zero external callers; the endpoint re-implemented the walk and reported an
/// unknown function as a **warning**, so it green-lit payloads create rejects.
///
/// The identity checks exist for the same reason as the function-name one, and
/// they were the last three ways to author a workflow that create accepts and
/// the engine then refuses. All three were verified end to end:
///
/// | Authored | Create | What actually happened |
/// |---|---|---|
/// | task without `id` | `201` | `503` on every request; channel quarantined at load |
/// | task without `name` | `201` | same — both are required `String`s upstream |
/// | two tasks sharing an `id` | `201` | `500` on activate; **the whole engine reload fails**, and at boot `Engine::new` aborts the process |
/// | `"tasks": []` | `201` | same as the duplicate case — `Workflow::validate()` refuses a workflow with no tasks, and it is the engine *build* that runs it |
///
/// The duplicate case was the worst of the three because the per-channel
/// quarantine did not contain it: `LogicCompiler::compile_workflows` calls
/// `Workflow::validate()`, so one repeated id took down every channel on every
/// node rather than its own. The load screen now runs `validate()` as well, so
/// a stored row carrying one is quarantined; refusing it here is still the
/// better answer.
///
/// `loop_config` is checked with the body, as `build` checks it: its `setup`
/// steps share the body's step-id namespace, and its own fields carry rules
/// the engine enforces.
///
/// **These track dataflow-rs's parsing rules rather than tightening them**, so
/// that "Orion accepts it" and "the engine can load it" stay the same
/// statement. Both fields must be *present* because both are required `String`s
/// with no serde default. Only `id` additionally has to be non-empty, and that
/// is the one deliberate step beyond the parse: `""` deserializes happily, but
/// it collides with any second blank id on `Workflow::validate()`'s uniqueness
/// check, and a task that does parse with one writes an empty `task_id` into
/// every audit entry, trace step and metric label. `name` is left alone — an
/// empty one is unhelpful in a log, but it loads, and refusing it would be
/// Orion inventing a rule the engine does not have.
pub fn validate_workflow_tasks_schema(
    tasks: &serde_json::Value,
    loop_config: Option<&serde_json::Value>,
    functions: &FunctionRegistry,
) -> Vec<FieldError> {
    if tasks.as_array().is_none() {
        return Vec::new();
    }
    let mut errors = Vec::new();
    // Flattened: since dataflow-rs 3.6 an element carrying `tasks` is a group,
    // and its members are tasks the engine will run. Validating only the top
    // level would accept a workflow whose guarded half was never checked, and
    // report the group itself as a task missing its `name` and `function`.
    let steps = crate::engine::walk_steps(tasks, loop_config.filter(|l| !l.is_null()));

    // Ids are one namespace across tasks and groups — both name a step, and
    // both surface in traces — so uniqueness is checked over the union.
    let mut seen_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for path in &steps.too_deep {
        errors.push(FieldError::new(
            format!("{path}.tasks"),
            "INVALID",
            format!(
                "Task groups nest more than {} deep — the engine refuses to build \
                 this workflow, which fails the whole reload rather than just this \
                 workflow",
                crate::engine::MAX_STEP_DEPTH
            ),
        ));
    }

    for (path, group) in &steps.groups {
        check_step_id(group, path, &mut seen_ids, &mut errors);
        match group.get("tasks").and_then(|t| t.as_array()) {
            Some(inner) if !inner.is_empty() => {}
            Some(_) => errors.push(FieldError::new(
                format!("{path}.tasks"),
                "INVALID",
                "A task group must contain at least one task — an empty group is a \
                 condition guarding nothing",
            )),
            None => errors.push(FieldError::new(
                format!("{path}.tasks"),
                "TYPE_MISMATCH",
                "A task group's 'tasks' must be an array of steps",
            )),
        }
        check_terminal(group, path, &mut errors);
    }

    for (path, task) in &steps.tasks {
        // Identity first, and independently of whether the function resolves:
        // a task can be broken in both ways at once, and an author fixing one
        // error at a time is the thing structured field errors exist to avoid.
        check_step_id(task, path, &mut seen_ids, &mut errors);
        if let Some(for_each) = task.get("for_each").and_then(Value::as_object) {
            errors.extend(unknown_keys(
                for_each,
                &format!("{path}.for_each"),
                "for_each",
                FOR_EACH_FIELDS,
            ));
        }

        // Presence only, matching the parse: `Task::name` is a required
        // `String`, but an empty one deserializes and loads fine, so refusing
        // it here would reject a workflow the engine would happily run.
        if task.get("name").and_then(|v| v.as_str()).is_none() {
            errors.push(FieldError::new(
                format!("{path}.name"),
                "REQUIRED",
                "Task 'name' is required and must be a string — it is what makes \
                 an audit trail or a trace readable to a human. It may be empty, \
                 but it must be present: without the key this workflow would be \
                 accepted and then fail to load, taking its channel out of service",
            ));
        }

        check_terminal(task, path, &mut errors);

        let function = task.get("function");
        let fn_name = function
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("");
        if fn_name.is_empty() {
            // `Task::function` has no serde default, so a task without one
            // (or with a nameless function) is not a lenient shape — it is a
            // workflow the engine cannot deserialize. Skipping it here made
            // that a 201 followed by a 500 from the dry-run endpoint.
            errors.push(FieldError::new(
                format!("{path}.function.name"),
                "REQUIRED",
                "Task 'function' with a non-empty 'name' is required — the engine's \
                 task shape has no default for it, so without one this workflow \
                 would be accepted and then fail to build",
            ));
            continue;
        }
        if !functions.contains(fn_name) {
            let suggestion = functions
                .suggest(fn_name)
                .map(|closest| format!(" — did you mean '{closest}'?"))
                .unwrap_or_default();
            errors.push(FieldError::new(
                format!("{path}.function.name"),
                "UNKNOWN_FUNCTION",
                format!(
                    "Unknown function '{fn_name}'{suggestion} — this workflow would be \
                     accepted and then fail at its first request"
                ),
            ));
            continue;
        }
        let input = function
            .and_then(|f| f.get("input"))
            .cloned()
            .unwrap_or(serde_json::Value::Object(Default::default()));
        errors.extend(functions.validate_input(fn_name, &input, path));
    }

    // Catch-all for the class the checks above mirror by hand: a dataflow-rs
    // upgrade can grow a task-shape requirement the mirror has not learned
    // yet, and the doc above promises "Orion accepts it" and "the engine can
    // load it" stay the same statement. Asking the engine keeps that promise
    // without enumerating — an unmirrored refusal lands here as a 400 instead
    // of a 201 followed by a failure to build. Only when no field-pathed error
    // was collected: those are the better messages, and this one exists for
    // the gaps they miss.
    //
    // `Workflow::validate_authored` (dataflow-rs 3.7) replaced a bare
    // round-trip `from_value::<Workflow>` here. Three things improved. It
    // reports *every* remaining problem rather than the parser's first. Each
    // one carries the coordinate the author typed, so the error points at
    // `tasks[1].tasks[0].id` instead of at a bare `tasks`. And it runs
    // `Workflow::validate()` as well as the parse, which is what the engine
    // build actually runs — so `tasks: []`, which parses cleanly and then
    // fails `validate()`, is now refused at create instead of taken down the
    // whole reload on activation.
    //
    // The workflow-level fields are synthesized: `workflow.id` /
    // `workflow.name` are validated by their own callers with their own
    // messages. `loop` goes in whole, because `build` validates it whole:
    // its `setup` steps share the body's step-id namespace, and `as`,
    // `scratch` and `over` carry rules of their own. A synthetic document
    // without it accepted a workflow whose loop then failed every reload.
    if errors.is_empty() {
        let mut synthetic = serde_json::json!({
            "id": "__shape_check__", "name": "__shape_check__",
            "condition": true, "tasks": tasks,
        });
        if let Some(loop_config) = loop_config.filter(|l| !l.is_null()) {
            synthetic["loop"] = loop_config.clone();
        }
        errors.extend(
            dataflow_rs_v3::Workflow::validate_authored(&synthetic)
                .into_iter()
                // An advisory must never become a create-time refusal.
                // `validate_authored` emits none today — the three live on
                // `check_workflow` — but `IssueCode` is `#[non_exhaustive]`
                // and `engine_issue_to_field_error` maps anything it does not
                // recognise onto `INVALID`, so one arriving here would start
                // rejecting workflows that build and run. `Rejected` and
                // `Defect` stay errors: the second builds and then fails every
                // message, which is not something to accept at create.
                .filter(|issue| issue.severity() != dataflow_rs_v3::Severity::Advisory)
                .map(engine_issue_to_field_error),
        );
    }
    errors
}

/// Render one [`dataflow_rs_v3::WorkflowIssue`] as an Orion `FieldError`.
///
/// The code registry is closed (`orion_api::error::field_codes`), so this maps
/// onto the codes Orion already emits rather than minting one per `IssueCode`.
/// `IssueCode` is `#[non_exhaustive]`: a variant added upstream lands on
/// `INVALID` and still reaches the author with the engine's own message, which
/// is the point of having a catch-all at all.
///
/// The path is the engine's authored coordinate, already rooted at `tasks`, so
/// it drops straight into a field error. A workflow-level issue carries no
/// path and is reported against `tasks` — the only field this function was
/// given.
fn engine_issue_to_field_error(issue: dataflow_rs_v3::WorkflowIssue) -> FieldError {
    use dataflow_rs_v3::IssueCode;
    let code = match issue.code {
        IssueCode::NoTasks | IssueCode::MissingStepId | IssueCode::MissingFunction => "REQUIRED",
        IssueCode::DuplicateStepId => "DUPLICATE_TASK_ID",
        IssueCode::InvalidTerminal => "TYPE_MISMATCH",
        IssueCode::UnknownFunction | IssueCode::MissingHandler => "UNKNOWN_FUNCTION",
        _ => "INVALID",
    };
    FieldError::new(
        issue.path.unwrap_or_else(|| "tasks".to_string()),
        code,
        issue.message,
    )
}

/// Every JSON-bearing field of a workflow request, checked for authoring
/// source form.
///
/// All three are walked because the compiler splices `$from` at any depth in
/// any of them — a shared timeout in `loop`, a shared predicate in
/// `condition` — and a check that covered only `tasks` would let the other two
/// through to be stored uncompiled.
fn source_form_errors(
    tasks: Option<&serde_json::Value>,
    condition: Option<&serde_json::Value>,
    loop_config: Option<&serde_json::Value>,
) -> Vec<FieldError> {
    let mut errors = Vec::new();
    if let Some(tasks) = tasks {
        errors.extend(uncompiled_source_errors(tasks, "tasks"));
    }
    if let Some(condition) = condition {
        errors.extend(uncompiled_source_errors(condition, "condition"));
    }
    if let Some(loop_config) = loop_config {
        errors.extend(uncompiled_source_errors(loop_config, "loop"));
    }
    errors
}

fn uncompiled(details: Vec<FieldError>) -> OrionError {
    validation_with_details(
        "Workflow has not been compiled: it still contains shared-definition references",
        details,
    )
}

fn validation_with_details(message: &str, details: Vec<FieldError>) -> OrionError {
    OrionError::Validation {
        code: "VALIDATION_ERROR",
        message: message.to_string(),
        details,
    }
}

pub fn validate_workflow_id(id: &str) -> Result<(), OrionError> {
    validate_id(id, "workflow.workflow_id")
}

/// A step's `id`: required, non-empty, and unique across tasks *and* groups.
///
/// One namespace because dataflow-rs uses one — both name a step and both
/// surface in traces, so a group id colliding with a task id is refused at
/// build, which for Orion is a failed reload rather than one bad workflow.
fn check_step_id<'a>(
    step: &'a serde_json::Value,
    path: &str,
    seen: &mut std::collections::HashSet<&'a str>,
    errors: &mut Vec<FieldError>,
) {
    // `trim`med for the emptiness test only — the untrimmed value is what the
    // engine keys on, so that is what uniqueness is checked over.
    let raw = step
        .get("id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if raw.trim().is_empty() {
        errors.push(FieldError::new(
            format!("{path}.id"),
            "REQUIRED",
            "Step 'id' is required and must be a non-empty string — it names \
             the step in audit trails, execution traces, per-task metrics and \
             `metadata.progress`, which workflow conditions can read. Without \
             one this workflow would be accepted and then fail to load, \
             taking its channel out of service",
        ));
        return;
    }
    if !seen.insert(raw) {
        errors.push(FieldError::new(
            format!("{path}.id"),
            "DUPLICATE_TASK_ID",
            format!(
                "Duplicate step id '{raw}' — ids must be unique within a \
                 workflow, across tasks and task groups alike. The engine \
                 refuses to build one that repeats them, so this fails the \
                 entire engine reload rather than just this workflow"
            ),
        ));
    }
}

/// A step's `terminal` flag, shared by tasks and groups: optional, but a
/// boolean when present. Any step may set it to end the workflow.
fn check_terminal(step: &serde_json::Value, path: &str, errors: &mut Vec<FieldError>) {
    if let Some(terminal) = step.get("terminal")
        && !terminal.is_boolean()
    {
        errors.push(FieldError::new(
            format!("{path}.terminal"),
            "TYPE_MISMATCH",
            "'terminal' must be a boolean",
        ));
    }
}

mod advisories;
pub use advisories::*;

#[cfg(test)]
mod tests;
