use crate::engine::FunctionRegistry;
use crate::errors::{FieldError, OrionError};
use crate::storage::repositories::workflows::{CreateWorkflowRequest, UpdateWorkflowRequest};

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
    let task_errors = validate_workflow_tasks_schema(&req.tasks, functions);
    if !task_errors.is_empty() {
        return Err(validation_with_details(
            "Workflow tasks contain invalid function inputs",
            task_errors,
        ));
    }
    reject_stray_secret_references(&req.tasks, functions)?;
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

/// Refuse a workflow carrying a secret reference in a field that resolves
/// none — [`secret_reference_errors`] as the create/update paths' refusal, so
/// both spell the code and the summary once.
fn reject_stray_secret_references(
    tasks: &Value,
    functions: &FunctionRegistry,
) -> Result<(), OrionError> {
    let refs: Vec<FieldError> = secret_reference_errors(tasks, functions)
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

pub fn validate_update_workflow(
    req: &UpdateWorkflowRequest,
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
    if let Some(ref tasks) = req.tasks {
        let task_errors = validate_workflow_tasks_schema(tasks, functions);
        if !task_errors.is_empty() {
            return Err(validation_with_details(
                "Workflow tasks contain invalid function inputs",
                task_errors,
            ));
        }
        reject_stray_secret_references(tasks, functions)?;
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
pub fn validate_workflow_tasks_schema(
    tasks: &serde_json::Value,
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
                // An advisory must never become a create-time refusal.
                // `validate_authored` emits none today — the three live on
                // `check_workflow` — but `IssueCode` is `#[non_exhaustive]`
                // and `engine_issue_to_field_error` maps anything it does not
                // recognise onto `INVALID`, so one arriving here would start
                // rejecting workflows that build and run. `Rejected` and
                // `Defect` stay errors: the second builds and then fails every
                // message, which is not something to accept at create.
                .filter(|issue| issue.severity() != dataflow_rs::Severity::Advisory)
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
pub fn engine_advisories(tasks: &Value, functions: &FunctionRegistry) -> Vec<EngineAdvisory> {
    // The synthetic wrapper `validate_workflow_tasks_schema` uses, for the same
    // reason: this function is given `tasks` alone.
    let synthetic = serde_json::json!({
        "id": "__shape_check__", "name": "__shape_check__",
        "condition": true, "tasks": tasks,
    });
    let Ok(workflow) = dataflow_rs::Workflow::from_json(&synthetic.to_string()) else {
        // Unparseable tasks are reported with a better message by the schema
        // check; there is nothing to say about the shape of a document that is
        // not a workflow.
        return Vec::new();
    };
    let issues = dataflow_rs::Engine::builder().check_workflow(&workflow);

    // Escaped keys first, then the walk that finds the ones the engine cannot,
    // then the shape findings — the order the three callers reported these in
    // when they were two functions called one after the other.
    let mut out: Vec<EngineAdvisory> = issues
        .iter()
        .filter(|issue| issue.code == dataflow_rs::IssueCode::EscapedTemplateKey)
        .filter(|issue| issue.path.as_deref().is_none_or(is_accidental_escape))
        .map(|issue| EngineAdvisory {
            check: EngineAdvisory::ESCAPED_TEMPLATE_KEY,
            path: issue.path.clone().unwrap_or_else(|| "tasks".to_string()),
            message: issue.message.clone(),
        })
        .collect();

    for_each_input_field(tasks, |function, field, path, value| {
        // Only for a name the engine treats as custom: a built-in's parameters
        // were already walked, and reporting them twice would be worse than
        // not reporting them at all.
        if dataflow_rs::is_builtin_function(function) {
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
        // `screen_workflow` asks and cannot answer it differently. The escape
        // is already reported above, with a path of its own.
        if issue.severity() != dataflow_rs::Severity::Advisory
            || issue.code == dataflow_rs::IssueCode::EscapedTemplateKey
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
            dataflow_rs::IssueCode::UnguardedValidation => {
                (EngineAdvisory::UNGUARDED_VALIDATION, "task")
            }
            dataflow_rs::IssueCode::GroupContinueOnError => {
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
    /// An advisory this version of Orion has no specific id for — a code a
    /// later dataflow-rs added. Reported rather than dropped: the serving
    /// screen already knows not to quarantine it, and an author should still
    /// hear what the engine said.
    pub const UNCLASSIFIED: &'static str = "engine.advisory";
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
fn escaped_keys_in(value: &Value) -> Vec<(String, String)> {
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
    let Ok(workflow) = dataflow_rs::Workflow::from_json(&synthetic.to_string()) else {
        return Vec::new();
    };
    let builder = dataflow_rs::Engine::builder();
    builder
        .check_workflow(&workflow)
        .into_iter()
        .filter(|issue| issue.code == dataflow_rs::IssueCode::EscapedTemplateKey)
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
fn is_accidental_escape(path: &str) -> bool {
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
    functions: &FunctionRegistry,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for_each_input_field(tasks, |function, field, path, value| {
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
fn for_each_input_field(tasks: &Value, mut visit: impl FnMut(&str, &str, &str, &Value)) {
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
    functions: &FunctionRegistry,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for_each_input_field(tasks, |function, field, path, value| {
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
fn collect_secret_references(
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

    // The tests below predate the registry parameter; these shadow the glob
    // import with the built-in registry so each case reads as it did.
    fn registry() -> &'static FunctionRegistry {
        FunctionRegistry::builtin()
    }
    fn validate_create_workflow(req: &CreateWorkflowRequest, cap: i64) -> Result<(), OrionError> {
        super::validate_create_workflow(req, cap, registry())
    }
    fn validate_update_workflow(req: &UpdateWorkflowRequest, cap: i64) -> Result<(), OrionError> {
        super::validate_update_workflow(req, cap, registry())
    }
    fn unresolvable_logic_warnings(tasks: &Value) -> Vec<(String, String)> {
        super::unresolvable_logic_warnings(tasks, registry())
    }
    fn secret_reference_errors(tasks: &Value) -> Vec<(String, String)> {
        super::secret_reference_errors(tasks, registry())
    }

    /// A plugin never sees key material, so a `{"secret": ..}` node is
    /// refused in every one of its fields — and in the template field it is
    /// the refusal that matters, because the engine would otherwise evaluate
    /// the node straight into the guest's input. A built-in's literal field
    /// stays silent for the same node: nothing there would resolve it.
    #[test]
    fn a_plugin_function_refuses_a_secret_node_in_every_field() {
        use crate::engine::functions::schema::{FieldKind, RetrySafety, Source, WriteShape};
        use crate::engine::{FieldSpec, FunctionEntry, PluginBinding};

        let field =
            |name: &str, template_at: &'static [&'static str], resolvable: bool| FieldSpec {
                name: name.to_string(),
                description: String::new(),
                kind: FieldKind::Any,
                required: false,
                resolvable,
                secret_at: &[],
                template_at,
                alias: None,
            };
        let registry = FunctionRegistry::builtin()
            .with_entries(vec![FunctionEntry {
                name: "acme.codec.parse".to_string(),
                description: String::new(),
                category: "transform".to_string(),
                source: Source::Plugin,
                aliases: Vec::new(),
                input_fields: Some(vec![
                    field("template", &[""], false),
                    field("folded", &[], true),
                    field("literal", &[], false),
                    field("output", &[], false),
                ]),
                writes: WriteShape::OutputPath { default_root: None },
                retry_safety: RetrySafety::Pure,
                deny_unknown: true,
                validate_static: None,
                connector: None,
                plugin: Some(PluginBinding {
                    id: "acme.codec".to_string(),
                    version: 1,
                    digest: "sha256:00".to_string(),
                    abi: "orion:plugin@1.0.0".to_string(),
                }),
            }])
            .expect("extends");

        let found = super::secret_reference_errors(
            &json!([{
                "id": "t", "name": "t",
                "function": {"name": "acme.codec.parse", "input": {
                    "template": {"cat": ["k=", {"secret": "api_key"}]},
                    "folded": {"secret": "api_key"},
                    "literal": {"nested": [{"secret": "api_key"}]},
                    "output": "data.out"
                }}
            }]),
            &registry,
        );
        let mut paths: Vec<&str> = found.iter().map(|(p, _)| p.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            [
                "tasks[0].function.input.folded",
                "tasks[0].function.input.literal.nested[0]",
                "tasks[0].function.input.template.cat[1]",
            ],
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .all(|(_, m)| m.contains("never sees key material")),
            "{found:?}"
        );
    }
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

    /// An operator inside an *array* element of a resolvable field is reported
    /// like one nested in an object.
    ///
    /// `db_read`'s `params` is the shape this matters for and the one an author
    /// actually writes: the handler folds `{"var": ..}` and nothing else, so an
    /// `or` computing a default reaches the driver as a literal object and
    /// binds as one — a `jsonb` document on a JSON column, a type error on any
    /// other, and never the value that was meant. The walk descends into arrays
    /// for exactly this; without that it would report the same mistake in a
    /// `mongo_write` `document` and miss it here.
    #[test]
    fn an_operator_in_an_array_element_of_a_resolvable_field_is_reported() {
        let warnings = unresolvable_logic_warnings(&serde_json::json!([{
            "id": "q", "name": "Query",
            "function": {"name": "db_read", "input": {
                "connector": "orders",
                "query": "SELECT id FROM orders LIMIT $1",
                "params": [{"or": [{"var": "data.req.limit"}, 50]}]
            }}
        }]));
        assert_eq!(warnings.len(), 1, "got {warnings:?}");
        assert_eq!(warnings[0].0, "tasks[0].function.input.params[0]");
        assert!(warnings[0].1.contains("'or'"), "{}", warnings[0].1);
        assert!(warnings[0].1.contains("'map' task"), "{}", warnings[0].1);
    }

    /// The literal a JSON column legitimately binds is not an expression, and
    /// must not be reported as one: post-1.7 a `json`/`jsonb` parameter takes
    /// the document itself, so an object in `params` is a real value. Only a
    /// single-key object whose key is a known operator *and* whose argument is
    /// an array is reported, which is what keeps an ordinary document out.
    #[test]
    fn a_json_document_bound_as_a_parameter_is_not_reported() {
        let clean = unresolvable_logic_warnings(&serde_json::json!([{
            "id": "q", "name": "Query",
            "function": {"name": "db_read", "input": {
                "connector": "orders",
                "query": "SELECT id FROM orders WHERE meta @> $1",
                "params": [{"tier": "gold", "length": 120}]
            }}
        }]));
        assert!(clean.is_empty(), "got {clean:?}");
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

    /// §3.3: a `{"secret": …}` node in a field that does not read key material
    /// is refused at authoring time, exactly as `env://` is.
    ///
    /// Left alone it is not a no-op: the handler stores the node and sends it
    /// on as an object, so a query gets `{"secret":"api_key"}` as a bind
    /// parameter. The author asked for a credential and the database received
    /// a JSON object naming one.
    #[test]
    fn a_secret_node_outside_a_key_field_is_reported() {
        let found = secret_reference_errors(&serde_json::json!([{
            "id": "t1",
            "function": {"name": "db_read", "input": {
                "connector": "orders",
                "query": "SELECT 1 WHERE token = $1",
                "params": [{"secret": "api_key"}],
            }},
        }]));
        assert_eq!(
            found.len(),
            1,
            "expected exactly one finding, got {found:?}"
        );
        assert!(found[0].0.contains("params"), "{:?}", found[0]);
        assert!(found[0].1.contains("api_key"), "{:?}", found[0]);
    }

    /// The array spelling resolves identically at runtime, so it must be
    /// refused identically — recognising only the string form would leave one
    /// of the two reaching a backend as an object.
    #[test]
    fn the_array_spelling_of_a_secret_node_is_reported_too() {
        let found = secret_reference_errors(&serde_json::json!([{
            "id": "t1",
            "function": {"name": "db_read", "input": {
                "connector": "pg",
                "query": "SELECT 1",
                "params": [{"secret": ["api_key"]}],
            }},
        }]));
        assert_eq!(
            found.len(),
            1,
            "expected exactly one finding, got {found:?}"
        );
    }

    /// And silent in a field the engine *evaluates*, where the node resolves.
    ///
    /// `secret` is a reserved JSONLogic operator registered on every engine, so
    /// in a `template_at` field — `send_email.subject`, an `http_call` header —
    /// `{"secret": …}` produces the value rather than surviving as an object.
    /// This rule exists for the fields that fold `{"var": …}` and nothing else,
    /// where it would not; firing here would refuse a task that works.
    #[test]
    fn a_secret_node_in_an_evaluated_field_is_not_a_finding() {
        let found = secret_reference_errors(&serde_json::json!([{
            "id": "t1",
            "function": {"name": "send_email", "input": {
                "connector": "mail",
                "subject": {"cat": ["token=", {"secret": "api_key"}]},
            }},
        }]));
        assert!(found.is_empty(), "{found:?}");
    }

    /// And silent where the handler does read key material — the whole point
    /// of the per-path exemption. `crypto.key` takes a secret node; a check
    /// that fired here would refuse the recommended way to write the task.
    #[test]
    fn a_secret_node_in_a_key_field_is_left_alone() {
        let clean = secret_reference_errors(&serde_json::json!([{
            "id": "t1",
            "function": {"name": "crypto", "input": {
                "operation": "hmac_sha256",
                "data": "payload",
                "key": {"secret": "signing_key"},
            }},
        }]));
        assert!(clean.is_empty(), "{clean:?}");
    }

    /// The exemption is per path, not per field: `jwt_verify.keys` reads key
    /// material at `keys[].key` and nowhere else, so a secret node in a
    /// sibling `kid` — matched verbatim against the token's — is still
    /// reported, and the `key` beside it still is not.
    #[test]
    fn the_secret_node_exemption_follows_the_path_not_the_field() {
        let found = secret_reference_errors(&serde_json::json!([{
            "id": "t1",
            "function": {"name": "jwt_verify", "input": {
                "token": "t",
                "keys": [{"kid": {"secret": "which_key"}, "key": {"secret": "signing_key"}}],
            }},
        }]));
        assert_eq!(
            found.len(),
            1,
            "expected only the `kid` finding, got {found:?}"
        );
        assert!(found[0].0.contains("kid"), "{:?}", found[0]);
    }

    /// A column named `secret` is not a secret node.
    ///
    /// `data_query`'s `sort` is a list of single-key objects keyed by column
    /// name, so sorting on a column called `secret` authors as
    /// `{"secret": "asc"}` — structurally identical to the reference, and
    /// nothing of the kind. `query` is neither `resolvable` nor
    /// `secret_at`-bearing, so the handler never looks inside it for nodes and
    /// neither does this check.
    ///
    /// The shape is what makes the `inspects_nodes` qualifier necessary rather
    /// than tidy: without it this is a hard 400 on a legitimate query, and the
    /// same false positive reaches any keyed-object field whose keys are the
    /// author's own names.
    #[test]
    fn a_column_named_secret_is_not_a_secret_node() {
        let clean = secret_reference_errors(&serde_json::json!([{
            "id": "t1",
            "function": {"name": "data_query", "input": {
                "connector": "orders",
                "schema": {"entities": {"items": {
                    "physical": "items",
                    "columns": {"id": {"queryable": true}, "secret": {"queryable": false}},
                }}},
                "query": {"source": "items", "sort": [{"secret": "asc"}]},
                "output": "data.result",
            }},
        }]));
        assert!(clean.is_empty(), "{clean:?}");
    }

    /// The same node in the *resolvable* field of the same function is still
    /// reported — so the rule above narrows where the check speaks, and does
    /// not soften what it says where it does.
    #[test]
    fn a_secret_node_in_data_querys_resolvable_params_is_still_reported() {
        let found = secret_reference_errors(&serde_json::json!([{
            "id": "t1",
            "function": {"name": "data_query", "input": {
                "connector": "orders",
                "query": {"source": "items"},
                "params": {"token": {"secret": "api_key"}},
                "output": "data.result",
            }},
        }]));
        assert_eq!(
            found.len(),
            1,
            "expected exactly one finding, got {found:?}"
        );
        assert!(found[0].0.contains("params"), "{:?}", found[0]);
    }

    /// An object that merely has a `secret` member among others is data, not a
    /// secret node — the engine would not resolve it, so neither does this.
    #[test]
    fn a_multi_key_object_holding_a_secret_member_is_data() {
        let clean = secret_reference_errors(&serde_json::json!([{
            "id": "t1",
            "function": {"name": "db_read", "input": {
                "connector": "orders",
                "query": "SELECT 1",
                "params": [{"secret": "a", "public": "b"}],
            }},
        }]));
        assert!(clean.is_empty(), "{clean:?}");
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

    /// The exemption is per path, not per field. `jwt_verify.keys` resolves a
    /// reference at each entry's `key` and nowhere else — `kid` is matched
    /// verbatim against the token's, so a reference there never matches any
    /// token and nothing says why.
    #[test]
    fn a_reference_in_a_sibling_of_a_key_is_still_reported() {
        let found = secret_reference_errors(&serde_json::json!([{
            "id": "jwt", "name": "Verify",
            "function": {"name": "jwt_verify", "input": {
                "token": {"var": "data.token"},
                "keys": [{
                    "algorithm": "HS256",
                    "key": {"secret": "partner_hmac"},
                    "kid": "env://PARTNER_KID",
                    "key_encoding": "utf8"
                }]
            }}
        }]));
        assert_eq!(found.len(), 1, "got {found:?}");
        assert_eq!(found[0].0, "tasks[0].function.input.keys[0].kid");
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

#[cfg(test)]
mod engine_advisory_tests {
    fn engine_advisories(tasks: &serde_json::Value) -> Vec<super::EngineAdvisory> {
        super::engine_advisories(tasks, crate::engine::FunctionRegistry::builtin())
    }
    use serde_json::json;

    /// #308's shape. A failing rule records 400, the executor's 4xx branch
    /// carries on, and `continue_on_error` governs 5xx and Err only — so the
    /// check reads correct and changes nothing. The engine says so from 3.10;
    /// this asserts the warning reaches an Orion surface rather than only a
    /// server log.
    #[test]
    fn an_unguarded_validation_is_reported() {
        let tasks = json!([
            { "id": "check", "name": "Check", "function": { "name": "validation", "input": {
                "rules": [{ "logic": { "==": [1, 2] }, "message": "no" }] } } },
            { "id": "respond", "name": "Respond", "function": { "name": "map", "input": {
                "mappings": [{ "path": "data.x", "logic": true }] } } }
        ]);
        let found = engine_advisories(&tasks);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].check, "engine.unguarded_validation");
        assert!(found[0].message.contains("halt_on"), "{}", found[0].message);
        assert_eq!(found[0].path, "task 'check'.halt_on");
    }

    /// The fix silences it, which is the half that keeps the advisory from
    /// being noise on every workflow that validates anything.
    #[test]
    fn halt_on_silences_it() {
        let tasks = json!([
            { "id": "check", "name": "Check", "halt_on": "failure",
              "function": { "name": "validation", "input": {
                "rules": [{ "logic": { "==": [1, 2] }, "message": "no" }] } } },
            { "id": "respond", "name": "Respond", "function": { "name": "map", "input": {
                "mappings": [{ "path": "data.x", "logic": true }] } } }
        ]);
        assert!(engine_advisories(&tasks).is_empty());
    }

    /// So does gating what follows, which is the older spelling and still
    /// correct — the advisory is about the *shape*, not about the keyword.
    #[test]
    fn a_guarded_successor_silences_it() {
        let tasks = json!([
            { "id": "check", "name": "Check", "function": { "name": "validation", "input": {
                "rules": [{ "logic": { "==": [1, 2] }, "message": "no" }] } } },
            { "id": "respond", "name": "Respond",
              "condition": { "==": [{ "var": "metadata.progress.status_code" }, 200] },
              "function": { "name": "map", "input": {
                "mappings": [{ "path": "data.x", "logic": true }] } } }
        ]);
        assert!(engine_advisories(&tasks).is_empty());
    }

    /// `continue_on_error` is real on a task and on a workflow, which is what
    /// makes a group the one place it looks like it should work. The engine
    /// parses it and drops it.
    #[test]
    fn continue_on_error_on_a_group_is_reported() {
        let tasks = json!([
            { "id": "g", "name": "G", "continue_on_error": true, "tasks": [
                { "id": "t", "name": "T", "function": { "name": "log", "input": {
                    "message": "x" } } } ] }
        ]);
        let found = engine_advisories(&tasks);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].check, "engine.group_continue_on_error");
        // A group, named as one: the engine records it in `task_id` for want
        // of another field, and the message beside this calls it a group.
        assert_eq!(found[0].path, "group 'g'.continue_on_error");
    }

    /// A workflow with neither shape reports nothing, so the two above are not
    /// simply reporting everything.
    #[test]
    fn an_ordinary_workflow_is_quiet() {
        let tasks = json!([
            { "id": "t", "name": "T", "function": { "name": "log", "input": {
                "message": "x" } } }
        ]);
        assert!(engine_advisories(&tasks).is_empty());
    }
}
