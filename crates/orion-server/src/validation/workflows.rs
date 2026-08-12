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
    let Some(arr) = tasks.as_array() else {
        return Vec::new();
    };
    let mut errors = Vec::new();
    let mut seen_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (i, task) in arr.iter().enumerate() {
        // Identity first, and independently of whether the function resolves:
        // a task can be broken in both ways at once, and an author fixing one
        // error at a time is the thing structured field errors exist to avoid.
        match task.get("id").and_then(|v| v.as_str()).map(str::trim) {
            None | Some("") => errors.push(FieldError::new(
                format!("tasks[{i}].id"),
                "REQUIRED",
                "Task 'id' is required and must be a non-empty string — it names \
                 the task in audit trails, execution traces, per-task metrics and \
                 `metadata.progress`, which workflow conditions can read. Without \
                 one this workflow would be accepted and then fail to load, \
                 taking its channel out of service",
            )),
            Some(id) => {
                if !seen_ids.insert(id) {
                    errors.push(FieldError::new(
                        format!("tasks[{i}].id"),
                        "DUPLICATE_TASK_ID",
                        format!(
                            "Duplicate task id '{id}' — ids must be unique within a \
                             workflow. The engine refuses to build one that repeats \
                             them, so this fails the entire engine reload rather \
                             than just this workflow"
                        ),
                    ));
                }
            }
        }
        // Presence only, matching the parse: `Task::name` is a required
        // `String`, but an empty one deserializes and loads fine, so refusing
        // it here would reject a workflow the engine would happily run.
        if task.get("name").and_then(|v| v.as_str()).is_none() {
            errors.push(FieldError::new(
                format!("tasks[{i}].name"),
                "REQUIRED",
                "Task 'name' is required and must be a string — it is what makes \
                 an audit trail or a trace readable to a human. It may be empty, \
                 but it must be present: without the key this workflow would be \
                 accepted and then fail to load, taking its channel out of service",
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
                format!("tasks[{i}].function.name"),
                "REQUIRED",
                "Task 'function' with a non-empty 'name' is required — the engine's \
                 task shape has no default for it, so without one this workflow \
                 would be accepted and then fail to build",
            ));
            continue;
        }
        if !crate::engine::is_known_function(fn_name) {
            errors.push(FieldError::new(
                format!("tasks[{i}].function.name"),
                "UNKNOWN_FUNCTION",
                format!(
                    "Unknown function '{fn_name}' — this workflow would be accepted \
                     and then fail at its first request"
                ),
            ));
            continue;
        }
        let input = function
            .and_then(|f| f.get("input"))
            .cloned()
            .unwrap_or(serde_json::Value::Object(Default::default()));
        let task_path = format!("tasks[{i}]");
        errors.extend(crate::engine::functions::schema::validate_input(
            fn_name, &input, &task_path,
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
    if errors.is_empty()
        && let Err(e) = <Vec<dataflow_rs::Task> as serde::Deserialize>::deserialize(tasks)
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

    #[test]
    fn test_validate_workflow_id() {
        assert!(validate_workflow_id("my-workflow-1").is_ok());
        assert!(validate_workflow_id("bad id!").is_err());
    }
}
