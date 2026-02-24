use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

impl From<&Rule> for RuleResponse {
    fn from(rule: &Rule) -> Self {
        Self {
            id: rule.id.clone(),
            name: rule.name.clone(),
            description: rule.description.clone(),
            channel: rule.channel.clone(),
            priority: rule.priority,
            version: rule.version,
            status: rule.status.clone(),
            condition: serde_json::from_str(&rule.condition_json).unwrap_or(Value::Null),
            tasks: serde_json::from_str(&rule.tasks_json).unwrap_or(Value::Null),
            tags: serde_json::from_str(&rule.tags).unwrap_or(Value::Null),
            continue_on_error: rule.continue_on_error,
            created_at: rule.created_at,
            updated_at: rule.updated_at,
        }
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

impl From<&RuleVersion> for RuleVersionResponse {
    fn from(v: &RuleVersion) -> Self {
        Self {
            id: v.id,
            rule_id: v.rule_id.clone(),
            version: v.version,
            name: v.name.clone(),
            description: v.description.clone(),
            channel: v.channel.clone(),
            priority: v.priority,
            status: v.status.clone(),
            condition: serde_json::from_str(&v.condition_json).unwrap_or(Value::Null),
            tasks: serde_json::from_str(&v.tasks_json).unwrap_or(Value::Null),
            tags: serde_json::from_str(&v.tags).unwrap_or(Value::Null),
            continue_on_error: v.continue_on_error,
            created_at: v.created_at,
        }
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
