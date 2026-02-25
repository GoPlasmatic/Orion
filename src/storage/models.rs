use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::OrionError;

// -- Rule status constants --
pub const RULE_STATUS_ACTIVE: &str = "active";
pub const RULE_STATUS_PAUSED: &str = "paused";
pub const RULE_STATUS_ARCHIVED: &str = "archived";
pub const VALID_RULE_STATUSES: [&str; 3] =
    [RULE_STATUS_ACTIVE, RULE_STATUS_PAUSED, RULE_STATUS_ARCHIVED];

// -- Job status constants --
pub const JOB_STATUS_PENDING: &str = "pending";
pub const JOB_STATUS_RUNNING: &str = "running";
pub const JOB_STATUS_COMPLETED: &str = "completed";
pub const JOB_STATUS_FAILED: &str = "failed";

// -- Sentinel values --
pub const DATA_API_CONNECTOR: &str = "__data_api__";

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct Rule {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub channel: String,
    pub priority: i64,
    pub version: i64,
    pub status: String,
    pub condition_json: String,
    pub tasks_json: String,
    pub tags: String,
    pub continue_on_error: bool,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// API-friendly representation of a Rule with parsed JSON fields.
#[derive(Debug, Clone, Serialize)]
pub struct RuleResponse {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub channel: String,
    pub priority: i64,
    pub version: i64,
    pub status: String,
    pub condition: Value,
    pub tasks: Value,
    pub tags: Value,
    pub continue_on_error: bool,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

impl TryFrom<&Rule> for RuleResponse {
    type Error = OrionError;

    fn try_from(rule: &Rule) -> Result<Self, Self::Error> {
        Ok(Self {
            id: rule.id.clone(),
            name: rule.name.clone(),
            description: rule.description.clone(),
            channel: rule.channel.clone(),
            priority: rule.priority,
            version: rule.version,
            status: rule.status.clone(),
            condition: serde_json::from_str(&rule.condition_json).map_err(|e| {
                OrionError::InternalSource {
                    context: format!("Corrupt JSON in rule {} condition_json", rule.id),
                    source: Box::new(e),
                }
            })?,
            tasks: serde_json::from_str(&rule.tasks_json).map_err(|e| {
                OrionError::InternalSource {
                    context: format!("Corrupt JSON in rule {} tasks_json", rule.id),
                    source: Box::new(e),
                }
            })?,
            tags: serde_json::from_str(&rule.tags).map_err(|e| OrionError::InternalSource {
                context: format!("Corrupt JSON in rule {} tags", rule.id),
                source: Box::new(e),
            })?,
            continue_on_error: rule.continue_on_error,
            created_at: rule.created_at,
            updated_at: rule.updated_at,
        })
    }
}

/// API-friendly representation of a RuleVersion with parsed JSON fields.
#[derive(Debug, Clone, Serialize)]
pub struct RuleVersionResponse {
    pub id: i64,
    pub rule_id: String,
    pub version: i64,
    pub name: String,
    pub description: Option<String>,
    pub channel: String,
    pub priority: i64,
    pub status: String,
    pub condition: Value,
    pub tasks: Value,
    pub tags: Value,
    pub continue_on_error: bool,
    pub created_at: NaiveDateTime,
}

impl TryFrom<&RuleVersion> for RuleVersionResponse {
    type Error = OrionError;

    fn try_from(v: &RuleVersion) -> Result<Self, Self::Error> {
        Ok(Self {
            id: v.id,
            rule_id: v.rule_id.clone(),
            version: v.version,
            name: v.name.clone(),
            description: v.description.clone(),
            channel: v.channel.clone(),
            priority: v.priority,
            status: v.status.clone(),
            condition: serde_json::from_str(&v.condition_json).map_err(|e| {
                OrionError::InternalSource {
                    context: format!("Corrupt JSON in rule version {} condition_json", v.id),
                    source: Box::new(e),
                }
            })?,
            tasks: serde_json::from_str(&v.tasks_json).map_err(|e| OrionError::InternalSource {
                context: format!("Corrupt JSON in rule version {} tasks_json", v.id),
                source: Box::new(e),
            })?,
            tags: serde_json::from_str(&v.tags).map_err(|e| OrionError::InternalSource {
                context: format!("Corrupt JSON in rule version {} tags", v.id),
                source: Box::new(e),
            })?,
            continue_on_error: v.continue_on_error,
            created_at: v.created_at,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct RuleVersion {
    pub id: i64,
    pub rule_id: String,
    pub version: i64,
    pub name: String,
    pub description: Option<String>,
    pub channel: String,
    pub priority: i64,
    pub status: String,
    pub condition_json: String,
    pub tasks_json: String,
    pub tags: String,
    pub continue_on_error: bool,
    pub created_at: NaiveDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct Connector {
    pub id: String,
    pub name: String,
    pub connector_type: String,
    pub config_json: String,
    pub enabled: bool,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct Job {
    pub id: String,
    pub connector_id: String,
    pub status: String,
    pub started_at: Option<NaiveDateTime>,
    pub completed_at: Option<NaiveDateTime>,
    pub error_message: Option<String>,
    pub records_processed: i64,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub result_json: Option<String>,
    pub channel: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn sample_datetime() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2025, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
    }

    fn sample_rule() -> Rule {
        Rule {
            id: "rule-1".to_string(),
            name: "Test Rule".to_string(),
            description: Some("A test rule".to_string()),
            channel: "orders".to_string(),
            priority: 10,
            version: 1,
            status: RULE_STATUS_ACTIVE.to_string(),
            condition_json: r#"{"==": [1, 1]}"#.to_string(),
            tasks_json: r#"[{"id": "t1", "function": "http_call"}]"#.to_string(),
            tags: r#"["test"]"#.to_string(),
            continue_on_error: false,
            created_at: sample_datetime(),
            updated_at: sample_datetime(),
        }
    }

    #[test]
    fn test_rule_response_try_from_valid() {
        let rule = sample_rule();
        let response = RuleResponse::try_from(&rule).unwrap();
        assert_eq!(response.id, "rule-1");
        assert_eq!(response.name, "Test Rule");
        assert_eq!(response.channel, "orders");
        assert_eq!(response.priority, 10);
        assert_eq!(response.version, 1);
        assert_eq!(response.status, RULE_STATUS_ACTIVE);
        assert_eq!(response.condition, serde_json::json!({"==": [1, 1]}));
        assert_eq!(
            response.tasks,
            serde_json::json!([{"id": "t1", "function": "http_call"}])
        );
        assert_eq!(response.tags, serde_json::json!(["test"]));
        assert!(!response.continue_on_error);
    }

    #[test]
    fn test_rule_response_try_from_invalid_condition_json() {
        let mut rule = sample_rule();
        rule.condition_json = "not valid json {{{".to_string();
        let result = RuleResponse::try_from(&rule);
        assert!(result.is_err());
    }

    #[test]
    fn test_rule_response_try_from_invalid_tasks_json() {
        let mut rule = sample_rule();
        rule.tasks_json = "invalid".to_string();
        let result = RuleResponse::try_from(&rule);
        assert!(result.is_err());
    }

    #[test]
    fn test_rule_response_try_from_invalid_tags_json() {
        let mut rule = sample_rule();
        rule.tags = "not json".to_string();
        let result = RuleResponse::try_from(&rule);
        assert!(result.is_err());
    }

    #[test]
    fn test_rule_response_try_from_no_description() {
        let mut rule = sample_rule();
        rule.description = None;
        let response = RuleResponse::try_from(&rule).unwrap();
        assert!(response.description.is_none());
    }

    #[test]
    fn test_rule_version_response_try_from_valid() {
        let version = RuleVersion {
            id: 1,
            rule_id: "rule-1".to_string(),
            version: 2,
            name: "Updated Rule".to_string(),
            description: Some("v2".to_string()),
            channel: "events".to_string(),
            priority: 5,
            status: RULE_STATUS_PAUSED.to_string(),
            condition_json: r#"{"==": [1, 1]}"#.to_string(),
            tasks_json: r#"[]"#.to_string(),
            tags: r#"["v2"]"#.to_string(),
            continue_on_error: true,
            created_at: sample_datetime(),
        };
        let response = RuleVersionResponse::try_from(&version).unwrap();
        assert_eq!(response.rule_id, "rule-1");
        assert_eq!(response.version, 2);
        assert_eq!(response.name, "Updated Rule");
        assert!(response.continue_on_error);
    }

    #[test]
    fn test_rule_version_response_try_from_invalid_json() {
        let version = RuleVersion {
            id: 1,
            rule_id: "rule-1".to_string(),
            version: 1,
            name: "Test".to_string(),
            description: None,
            channel: "ch".to_string(),
            priority: 1,
            status: RULE_STATUS_ACTIVE.to_string(),
            condition_json: "bad json".to_string(),
            tasks_json: r#"[]"#.to_string(),
            tags: r#"[]"#.to_string(),
            continue_on_error: false,
            created_at: sample_datetime(),
        };
        assert!(RuleVersionResponse::try_from(&version).is_err());
    }

    #[test]
    fn test_rule_version_response_try_from_invalid_tasks() {
        let version = RuleVersion {
            id: 1,
            rule_id: "rule-1".to_string(),
            version: 1,
            name: "Test".to_string(),
            description: None,
            channel: "ch".to_string(),
            priority: 1,
            status: RULE_STATUS_ACTIVE.to_string(),
            condition_json: r#"true"#.to_string(),
            tasks_json: "bad".to_string(),
            tags: r#"[]"#.to_string(),
            continue_on_error: false,
            created_at: sample_datetime(),
        };
        assert!(RuleVersionResponse::try_from(&version).is_err());
    }

    #[test]
    fn test_rule_version_response_try_from_invalid_tags() {
        let version = RuleVersion {
            id: 1,
            rule_id: "rule-1".to_string(),
            version: 1,
            name: "Test".to_string(),
            description: None,
            channel: "ch".to_string(),
            priority: 1,
            status: RULE_STATUS_ACTIVE.to_string(),
            condition_json: r#"true"#.to_string(),
            tasks_json: r#"[]"#.to_string(),
            tags: "bad".to_string(),
            continue_on_error: false,
            created_at: sample_datetime(),
        };
        assert!(RuleVersionResponse::try_from(&version).is_err());
    }

    #[test]
    fn test_valid_rule_statuses() {
        assert!(VALID_RULE_STATUSES.contains(&RULE_STATUS_ACTIVE));
        assert!(VALID_RULE_STATUSES.contains(&RULE_STATUS_PAUSED));
        assert!(VALID_RULE_STATUSES.contains(&RULE_STATUS_ARCHIVED));
    }

    #[test]
    fn test_job_status_constants() {
        assert_eq!(JOB_STATUS_PENDING, "pending");
        assert_eq!(JOB_STATUS_RUNNING, "running");
        assert_eq!(JOB_STATUS_COMPLETED, "completed");
        assert_eq!(JOB_STATUS_FAILED, "failed");
    }
}
