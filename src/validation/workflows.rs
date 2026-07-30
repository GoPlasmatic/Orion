use crate::errors::{FieldError, OrionError};
use crate::storage::repositories::workflows::{CreateWorkflowRequest, UpdateWorkflowRequest};

use super::common::{validate_description, validate_id, validate_name};

pub fn validate_create_workflow(req: &CreateWorkflowRequest) -> Result<(), OrionError> {
    if let Some(ref id) = req.workflow_id {
        validate_id(id).map_err(|e| remap_to_field(e, "workflow.workflow_id"))?;
    }
    validate_name(&req.name, "Name").map_err(|e| remap_to_field(e, "workflow.name"))?;
    if let Some(ref desc) = req.description {
        validate_description(desc).map_err(|e| remap_to_field(e, "workflow.description"))?;
    }
    let task_errors = validate_workflow_tasks_schema(&req.tasks);
    if !task_errors.is_empty() {
        return Err(validation_with_details(
            "Workflow tasks contain invalid function inputs",
            task_errors,
        ));
    }
    Ok(())
}

pub fn validate_update_workflow(req: &UpdateWorkflowRequest) -> Result<(), OrionError> {
    if let Some(ref name) = req.name {
        validate_name(name, "Name").map_err(|e| remap_to_field(e, "workflow.name"))?;
    }
    if let Some(ref desc) = req.description {
        validate_description(desc).map_err(|e| remap_to_field(e, "workflow.description"))?;
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
    Ok(())
}

/// Walk the `tasks` array and collect schema-validation errors for each task's
/// `function.input` against the schema registered for `function.name`.
///
/// R5: an unknown `function.name` is a hard error — the function set is closed,
/// so such a workflow can only fail at its first request.
///
/// R20: `POST /admin/workflows/validate` reaches this through
/// `validate_create_workflow`, which is the point. This function was already
/// documented as *"public so the `/validate` endpoint can reuse it"* and had
/// zero external callers; the endpoint re-implemented the walk and reported an
/// unknown function as a **warning**, so it green-lit payloads create rejects.
pub fn validate_workflow_tasks_schema(tasks: &serde_json::Value) -> Vec<FieldError> {
    let Some(arr) = tasks.as_array() else {
        return Vec::new();
    };
    let mut errors = Vec::new();
    for (i, task) in arr.iter().enumerate() {
        let function = task.get("function");
        let fn_name = function
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("");
        if fn_name.is_empty() {
            continue;
        }
        if !crate::engine::is_known_function(fn_name) {
            errors.push(FieldError::new(
                format!("tasks[{i}].function.name"),
                "unknown_function",
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
    errors
}

fn validation_with_details(message: &str, details: Vec<FieldError>) -> OrionError {
    OrionError::Validation {
        code: "VALIDATION_ERROR",
        message: message.to_string(),
        details,
    }
}

fn remap_to_field(err: OrionError, path: &'static str) -> OrionError {
    match err {
        OrionError::BadRequest(msg) => OrionError::invalid_field(path, "INVALID", msg),
        other => other,
    }
}

pub fn validate_workflow_id(id: &str) -> Result<(), OrionError> {
    validate_id(id)
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
            continue_on_error: false,
        };
        assert!(validate_create_workflow(&req).is_ok());
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
            continue_on_error: false,
        };
        assert!(validate_create_workflow(&req).is_err());
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
            continue_on_error: false,
        };
        assert!(validate_create_workflow(&req).is_err());
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
            continue_on_error: None,
        };
        assert!(validate_update_workflow(&req).is_ok());
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
            continue_on_error: None,
        };
        assert!(validate_update_workflow(&req).is_err());
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
            continue_on_error: None,
        };
        assert!(validate_update_workflow(&req).is_err());
    }

    #[test]
    fn test_validate_workflow_id() {
        assert!(validate_workflow_id("my-workflow-1").is_ok());
        assert!(validate_workflow_id("bad id!").is_err());
    }
}
