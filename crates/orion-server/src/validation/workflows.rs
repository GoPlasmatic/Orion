use crate::errors::{FieldError, OrionError};
use crate::storage::repositories::workflows::{CreateWorkflowRequest, UpdateWorkflowRequest};

use super::common::{validate_description, validate_id, validate_name};

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
    let task_errors = validate_workflow_tasks_schema(&req.tasks);
    if !task_errors.is_empty() {
        return Err(validation_with_details(
            "Workflow tasks contain invalid function inputs",
            task_errors,
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
    if let Some(ref tasks) = req.tasks {
        let task_errors = validate_workflow_tasks_schema(tasks);
        if !task_errors.is_empty() {
            return Err(validation_with_details(
                "Workflow tasks contain invalid function inputs",
                task_errors,
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
        if let Some(terminal) = group.get("terminal")
            && !terminal.is_boolean()
        {
            errors.push(FieldError::new(
                format!("{path}.terminal"),
                "TYPE_MISMATCH",
                "'terminal' must be a boolean",
            ));
        }
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

        if let Some(terminal) = task.get("terminal")
            && !terminal.is_boolean()
        {
            errors.push(FieldError::new(
                format!("{path}.terminal"),
                "TYPE_MISMATCH",
                "'terminal' must be a boolean",
            ));
        }

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
    // load it" stay the same statement. Running the engine's own parse last
    // keeps that promise without enumerating — an unmirrored refusal lands
    // here as a 400 instead of a 201 followed by a failure to build. Only
    // when no field-pathed error was collected: those are the better
    // messages, and this one exists for the gaps they miss.
    // Through the engine's own step parser, not `Vec<Task>`: since 3.6 the
    // flattening lives in a `deserialize_with` on `Workflow::tasks`, so a bare
    // `Vec<Task>` would reject every grouped workflow the engine accepts.
    if errors.is_empty()
        && let Err(e) = serde_json::from_value::<dataflow_rs::Workflow>(serde_json::json!({
            "id": "__shape_check__", "name": "__shape_check__",
            "condition": true, "tasks": tasks,
        }))
    {
        errors.push(FieldError::new(
            "tasks",
            "INVALID",
            format!("tasks do not match the engine's task shape: {e}"),
        ));
    }
    errors
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
    match step.get("id").and_then(|v| v.as_str()).map(str::trim) {
        None | Some("") => errors.push(FieldError::new(
            format!("{path}.id"),
            "REQUIRED",
            "Step 'id' is required and must be a non-empty string — it names \
             the step in audit trails, execution traces, per-task metrics and \
             `metadata.progress`, which workflow conditions can read. Without \
             one this workflow would be accepted and then fail to load, \
             taking its channel out of service",
        )),
        Some(id) => {
            // `trim`ped for the emptiness test, but the untrimmed value is
            // what the engine keys on.
            let raw = step.get("id").and_then(|v| v.as_str()).unwrap_or(id);
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
            let base = format!("{path}.function.input.{field}");
            collect_unresolvable(value, &base, name, &mut out);
        }
    }
    out
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
                if arg.is_array()
                    && crate::engine::operators::OPERATOR_NAMES.contains(&key.as_str())
                {
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

    #[test]
    fn test_validate_create_workflow_full() {
        let req = CreateWorkflowRequest {
            workflow_id: Some("my-workflow-1".to_string()),
            name: "Test Workflow".to_string(),
            description: Some("A test workflow".to_string()),
            priority: 10,
            condition: json!(true),
            tasks: json!([]),
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
            tasks: json!([]),
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
            tasks: json!([]),
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
}
