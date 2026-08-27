use crate::errors::{FieldError, OrionError};
use crate::storage::repositories::workflows::{CreateWorkflowRequest, UpdateWorkflowRequest};

use super::common::{uncompiled_source_errors, validate_description, validate_id, validate_name};

pub fn validate_create_workflow(
    req: &CreateWorkflowRequest,
    max_loop_iterations: i64,
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
    let task_errors = validate_workflow_tasks_schema(&req.tasks);
    if !task_errors.is_empty() {
        return Err(validation_with_details(
            "Workflow tasks contain invalid function inputs",
            task_errors,
        ));
    }
    let refs = stray_secret_reference_errors(&req.tasks);
    if !refs.is_empty() {
        return Err(validation_with_details(
            "Workflow sends a secret reference somewhere it is never resolved",
            refs,
        ));
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
    Ok(())
}

/// [`secret_reference_errors`] as `FieldError`s, for the create/update paths.
fn stray_secret_reference_errors(tasks: &Value) -> Vec<FieldError> {
    secret_reference_errors(tasks)
        .into_iter()
        .map(|(path, message)| FieldError::new(path, "UNRESOLVED_SECRET_REF", message))
        .collect()
}

pub fn validate_update_workflow(
    req: &UpdateWorkflowRequest,
    max_loop_iterations: i64,
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
    if let Some(ref tasks) = req.tasks {
        let task_errors = validate_workflow_tasks_schema(tasks);
        if !task_errors.is_empty() {
            return Err(validation_with_details(
                "Workflow tasks contain invalid function inputs",
                task_errors,
            ));
        }
        let refs = stray_secret_reference_errors(tasks);
        if !refs.is_empty() {
            return Err(validation_with_details(
                "Workflow sends a secret reference somewhere it is never resolved",
                refs,
            ));
        }
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
    Ok(())
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

    let mut errors = Vec::new();

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
/// The duplicate case is the worst of the three because it is not contained by
/// the per-channel quarantine: `LogicCompiler::compile_workflows` calls
/// `Workflow::validate()`, so one repeated id takes down every channel on every
/// node rather than its own.
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
pub fn validate_workflow_tasks_schema(tasks: &serde_json::Value) -> Vec<FieldError> {
    if tasks.as_array().is_none() {
        return Vec::new();
    }
    let mut errors = Vec::new();
    // Flattened: since dataflow-rs 3.6 an element carrying `tasks` is a group,
    // and its members are tasks the engine will run. Validating only the top
    // level would accept a workflow whose guarded half was never checked, and
    // report the group itself as a task missing its `name` and `function`.
    let steps = crate::engine::walk_steps(tasks);

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
        if !crate::engine::is_known_function(fn_name) {
            let suggestion = crate::engine::suggest_known_function(fn_name)
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
        errors.extend(crate::engine::functions::schema::validate_input(
            fn_name, &input, path,
        ));
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
    // The workflow-level fields are synthesized: this function is given
    // `tasks` alone, and `workflow.id` / `workflow.name` are validated by
    // their own callers with their own messages.
    if errors.is_empty() {
        let synthetic = serde_json::json!({
            "id": "__shape_check__", "name": "__shape_check__",
            "condition": true, "tasks": tasks,
        });
        errors.extend(
            dataflow_rs::Workflow::validate_authored(&synthetic)
                .into_iter()
                .map(engine_issue_to_field_error),
        );
    }
    errors
}

/// Render one [`dataflow_rs::WorkflowIssue`] as an Orion `FieldError`.
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
fn engine_issue_to_field_error(issue: dataflow_rs::WorkflowIssue) -> FieldError {
    use dataflow_rs::IssueCode;
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

// ============================================================
// Advisory: JSONLogic in a field that folds only `{"var": ..}`
// ============================================================

use serde_json::Value;

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
pub fn unresolvable_logic_warnings(tasks: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    // Flattened, so a write inside a guard clause is checked like any other.
    for (path, task) in crate::engine::walk_steps(tasks).tasks {
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
            if !crate::engine::functions::schema::is_resolvable_field(name, field) {
                continue;
            }
            // A secret-bearing field resolves `{"secret": …}` itself (see
            // `engine::functions::secret_ref`), so the node is not an
            // unresolvable operator there — it is the field's other documented
            // spelling. `jwt_verify`'s `issuer` and `audience` are both.
            if crate::engine::functions::schema::is_secret_field(name, field)
                && crate::engine::functions::secret_ref::secret_name(value).is_some()
            {
                continue;
            }
            let base = format!("{path}.function.input.{field}");
            collect_unresolvable(value, &base, name, &mut out);
        }
    }
    out
}

/// Every secret reference sitting in a field that does not resolve one.
///
/// `env://NAME` and `vault://…` are resolved by the handler that reads a
/// particular field, not by a pass over the document — so five fields turn one
/// into a credential (`schema::is_secret_field`) and every other field sends
/// the string on as itself. A task carrying `{"path": "env://API_BASE"}`
/// requests a URL spelled `env://API_BASE` and fails with whatever the backend
/// makes of it, which is a failure that names neither the reference nor the
/// field.
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
pub fn secret_reference_errors(tasks: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    // Flattened, so a reference inside a guard clause is checked like any
    // other — a task group is where one is least likely to be noticed by eye.
    for (path, task) in crate::engine::walk_steps(tasks).tasks {
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
            // The whole subtree is skipped, not just its top level:
            // `jwt_verify.keys` is an array of objects whose `key` member is
            // the reference, so the legitimate one lives two levels down.
            if crate::engine::functions::schema::is_secret_field(name, field) {
                continue;
            }
            collect_secret_references(
                value,
                &format!("{path}.function.input.{field}"),
                name,
                &mut out,
            );
        }
    }
    out
}

/// Walk one authored value, reporting every secret reference inside it.
fn collect_secret_references(
    value: &Value,
    path: &str,
    function: &str,
    out: &mut Vec<(String, String)>,
) {
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
                collect_secret_references(child, &format!("{path}.{key}"), function, out);
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                collect_secret_references(child, &format!("{path}[{i}]"), function, out);
            }
        }
        _ => {}
    }
}

/// Walk one authored value, reporting every operator node inside it.
fn collect_unresolvable(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validation::common::MAX_DESCRIPTION_LEN;
    use serde_json::json;

    /// A minimal valid pipeline, for the tests that are about *other* fields.
    ///
    /// These used to pass `json!([])`, which the engine refuses — a workflow
    /// with no tasks fails `Workflow::validate()` and so fails the whole
    /// engine build. Create rejects it now, so a fixture standing in for
    /// "valid tasks" has to actually be some.
    fn one_task() -> serde_json::Value {
        json!([{
            "id": "t1", "name": "t1",
            "function": {"name": "map", "input": {"mappings": []}}
        }])
    }

    #[test]
    fn test_validate_create_workflow_full() {
        let req = CreateWorkflowRequest {
            workflow_id: Some("my-workflow-1".to_string()),
            name: "Test Workflow".to_string(),
            description: Some("A test workflow".to_string()),
            priority: 10,
            condition: json!(true),
            tasks: one_task(),
            tags: vec!["tag1".to_string()],
            loop_config: None,
            continue_on_error: false,
        };
        assert!(validate_create_workflow(&req, 10_000).is_ok());
    }

    #[test]
    fn test_validate_create_workflow_invalid_id() {
        let req = CreateWorkflowRequest {
            workflow_id: Some("bad id with spaces".to_string()),
            name: "Test Workflow".to_string(),
            description: None,
            priority: 0,
            condition: json!(true),
            tasks: one_task(),
            tags: vec![],
            loop_config: None,
            continue_on_error: false,
        };
        assert!(validate_create_workflow(&req, 10_000).is_err());
    }

    #[test]
    fn test_validate_create_workflow_long_description() {
        let req = CreateWorkflowRequest {
            workflow_id: None,
            name: "Test Workflow".to_string(),
            description: Some("d".repeat(MAX_DESCRIPTION_LEN + 1)),
            priority: 0,
            condition: json!(true),
            tasks: one_task(),
            tags: vec![],
            loop_config: None,
            continue_on_error: false,
        };
        assert!(validate_create_workflow(&req, 10_000).is_err());
    }

    #[test]
    fn test_validate_update_workflow_all_fields() {
        let req = UpdateWorkflowRequest {
            name: Some("Updated Name".to_string()),
            description: Some("Updated desc".to_string()),
            priority: Some(5),
            condition: None,
            tasks: None,
            tags: None,
            loop_config: None,
            continue_on_error: None,
        };
        assert!(validate_update_workflow(&req, 10_000).is_ok());
    }

    #[test]
    fn test_validate_update_workflow_invalid_name() {
        let req = UpdateWorkflowRequest {
            name: Some("".to_string()),
            description: None,
            priority: None,
            condition: None,
            tasks: None,
            tags: None,
            loop_config: None,
            continue_on_error: None,
        };
        assert!(validate_update_workflow(&req, 10_000).is_err());
    }

    #[test]
    fn test_validate_update_workflow_invalid_description() {
        let req = UpdateWorkflowRequest {
            name: None,
            description: Some("x".repeat(MAX_DESCRIPTION_LEN + 1)),
            priority: None,
            condition: None,
            tasks: None,
            tags: None,
            loop_config: None,
            continue_on_error: None,
        };
        assert!(validate_update_workflow(&req, 10_000).is_err());
    }

    /// Build a one-task workflow's `tasks` array around a `mongo_write` input.
    fn mongo_write_tasks(input: serde_json::Value) -> serde_json::Value {
        serde_json::json!([{
            "id": "w", "name": "Write",
            "function": {"name": "mongo_write", "input": input}
        }])
    }

    /// The bug the check exists for: an expression in a resolvable field is
    /// stored as a literal BSON object, with no error at write time.
    #[test]
    fn an_expression_in_a_resolvable_field_is_reported() {
        let warnings = unresolvable_logic_warnings(&mongo_write_tasks(serde_json::json!({
            "connector": "db", "database": "app", "collection": "sessions",
            "op": "update_one",
            "filter": {"_id": {"var": "data.id"}},
            "update": {"$set": {"expiresAt": {"cat": ["2026-", {"var": "data.month"}]}}}
        })));
        assert_eq!(warnings.len(), 1, "one finding: {warnings:?}");
        assert_eq!(
            warnings[0].0, "tasks[0].function.input.update.$set.expiresAt",
            "the path must point at the node, not the field"
        );
        assert!(warnings[0].1.contains("'cat'"), "{}", warnings[0].1);
    }

    /// `val` is a documented operator one letter from the one that works, and
    /// folds to nothing here — so it gets its own message naming the fix.
    #[test]
    fn a_val_node_is_reported_with_the_var_spelling() {
        let warnings = unresolvable_logic_warnings(&mongo_write_tasks(serde_json::json!({
            "connector": "db", "database": "app", "collection": "s",
            "op": "insert_one",
            "document": {"id": {"val": "data.id"}}
        })));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].1.contains("{\"var\": ..}"),
            "the message must name the spelling that works: {}",
            warnings[0].1
        );
    }

    /// Nested inside an extended-JSON wrapper, which is where this bug is most
    /// often written: `{"$date": {"now": []}}` looks like it computes a time.
    #[test]
    fn an_expression_nested_under_extended_json_is_reported() {
        let warnings = unresolvable_logic_warnings(&mongo_write_tasks(serde_json::json!({
            "connector": "db", "database": "app", "collection": "s",
            "op": "insert_one",
            "document": {"createdAt": {"$date": {"now": []}}}
        })));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].0.ends_with("document.createdAt.$date"),
            "path: {}",
            warnings[0].0
        );
    }

    /// The false positives that make this a warning rather than an error must
    /// at least not be *reported*. Operator names overlap real field names, and
    /// BSON operators are not JSONLogic at all.
    #[test]
    fn data_that_merely_looks_like_an_operator_is_not_reported() {
        let clean = unresolvable_logic_warnings(&mongo_write_tasks(serde_json::json!({
            "connector": "db", "database": "app", "collection": "s",
            "op": "update_one",
            // `$set`/`$and`/`$lt` are BSON, `{"var": ..}` is folded, and
            // `{"length": 120}` is a plain field whose argument is not an array.
            "filter": {"$and": [{"_id": {"var": "data.id"}}, {"n": {"$lt": 5}}]},
            "update": {"$set": {"video": {"length": 120}, "type": "clip"}}
        })));
        assert!(clean.is_empty(), "no findings expected, got {clean:?}");
    }

    /// A non-resolvable field is not scanned: the handler never folds it, so
    /// whatever it holds is a literal by design.
    #[test]
    fn a_non_resolvable_field_is_not_scanned() {
        let clean = unresolvable_logic_warnings(&serde_json::json!([{
            "id": "q", "name": "Query",
            "function": {"name": "db_read", "input": {
                "connector": "orders",
                // `sql` is literal text, not a resolvable field.
                "sql": "SELECT 1 WHERE cat = 'if'"
            }}
        }]));
        assert!(clean.is_empty(), "got {clean:?}");
    }

    #[test]
    fn test_validate_workflow_id() {
        assert!(validate_workflow_id("my-workflow-1").is_ok());
        assert!(validate_workflow_id("bad id!").is_err());
    }

    /// The motivating case: a reference in a field nothing resolves. Left
    /// alone it is requested as a URL spelled `env://API_BASE`.
    #[test]
    fn a_reference_outside_a_secret_field_is_reported() {
        let found = secret_reference_errors(&serde_json::json!([{
            "id": "call", "name": "Call",
            "function": {"name": "http_call", "input": {
                "connector": "crm",
                "path": "env://API_BASE"
            }}
        }]));
        assert_eq!(found.len(), 1, "got {found:?}");
        assert_eq!(found[0].0, "tasks[0].function.input.path");
        assert!(found[0].1.contains("env://API_BASE"), "{}", found[0].1);
    }

    /// The five fields that do resolve one must stay clean, including the
    /// reference two levels down inside `jwt_verify.keys`.
    #[test]
    fn a_reference_in_a_secret_field_is_left_alone() {
        let clean = secret_reference_errors(&serde_json::json!([
            {
                "id": "mac", "name": "MAC",
                "function": {"name": "crypto", "input": {
                    "op": "hmac", "key": "env://PARTNER_KEY", "data": {"var": "data.body"}
                }}
            },
            {
                "id": "jwt", "name": "Verify",
                "function": {"name": "jwt_verify", "input": {
                    "token": {"var": "data.token"},
                    "keys": [{"algorithm": "HS256", "key": "env://JWT_SECRET"}],
                    "audience": "env://OAUTH_CLIENT_ID"
                }}
            }
        ]));
        assert!(clean.is_empty(), "got {clean:?}");
    }

    /// A task group is where a stray reference is least likely to be caught by
    /// eye, so the walk has to descend into one.
    #[test]
    fn a_reference_inside_a_task_group_is_reported() {
        let found = secret_reference_errors(&serde_json::json!([{
            "id": "guarded", "name": "Guarded",
            "condition": {"var": "data.ok"},
            "tasks": [{
                "id": "send", "name": "Send",
                "function": {"name": "send_email", "input": {
                    "connector": "smtp", "to": "ops@example.com",
                    "subject": "hi", "text": "vault://secret/data/x#y"
                }}
            }]
        }]));
        assert_eq!(found.len(), 1, "got {found:?}");
        assert!(
            found[0].0.ends_with("function.input.text"),
            "{}",
            found[0].0
        );
    }

    /// An ordinary connection string is not a secret reference — the check
    /// asks the resolver registry, not "does it contain `://`".
    #[test]
    fn an_ordinary_url_is_not_a_reference() {
        let clean = secret_reference_errors(&serde_json::json!([{
            "id": "call", "name": "Call",
            "function": {"name": "http_call", "input": {
                "connector": "crm", "path": "https://example.com/v1/orders"
            }}
        }]));
        assert!(clean.is_empty(), "got {clean:?}");
    }
}
