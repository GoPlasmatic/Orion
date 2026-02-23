use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
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

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Connector {
    pub id: String,
    pub name: String,
    pub connector_type: String,
    pub config_json: String,
    pub enabled: bool,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
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
