use crate::errors::OrionError;
use crate::storage::repositories::workflows::{CreateWorkflowRequest, UpdateWorkflowRequest};

use super::common::{validate_description, validate_id, validate_name};

pub fn validate_create_workflow(req: &CreateWorkflowRequest) -> Result<(), OrionError> {
    if let Some(ref id) = req.workflow_id {
        validate_id(id)?;
    }
    validate_name(&req.name, "Name")?;
    if let Some(ref desc) = req.description {
        validate_description(desc)?;
    }
    Ok(())
}

pub fn validate_update_workflow(req: &UpdateWorkflowRequest) -> Result<(), OrionError> {
    if let Some(ref name) = req.name {
        validate_name(name, "Name")?;
    }
    if let Some(ref desc) = req.description {
        validate_description(desc)?;
    }
    Ok(())
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
