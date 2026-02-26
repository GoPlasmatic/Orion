use async_trait::async_trait;
use dataflow_rs::Workflow;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::errors::OrionError;
use crate::storage::models::{self, Rule};

#[derive(Debug, Serialize)]
pub struct PaginatedResult<T: Serialize> {
    pub data: Vec<T>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

// -- DTOs --

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateRuleRequest {
    pub id: Option<String>,
    pub name: String,
    pub description: Option<String>,
    #[serde(default = "default_channel")]
    pub channel: String,
    #[serde(default)]
    pub priority: i64,
    #[serde(default = "default_condition")]
    pub condition: serde_json::Value,
    pub tasks: serde_json::Value,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub continue_on_error: bool,
}

fn default_channel() -> String {
    "default".to_string()
}

fn default_condition() -> serde_json::Value {
    serde_json::Value::Bool(true)
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateRuleRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub channel: Option<String>,
    pub priority: Option<i64>,
    pub condition: Option<serde_json::Value>,
    pub tasks: Option<serde_json::Value>,
    pub status: Option<String>,
    pub tags: Option<Vec<String>>,
    pub continue_on_error: Option<bool>,
    /// Expected current version for optimistic locking. If provided and the
    /// rule has been modified since this version, the update returns 409 Conflict.
    pub version: Option<i64>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct StatusChangeRequest {
    pub status: String,
    /// Expected current version for optimistic locking. If provided and the
    /// rule has been modified since this version, the update returns 409 Conflict.
    pub version: Option<i64>,
}

#[derive(Debug, Default, Deserialize, Serialize, utoipa::IntoParams)]
pub struct RuleFilter {
    pub status: Option<String>,
    pub channel: Option<String>,
    pub tag: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

// -- Repository trait --

#[async_trait]
pub trait RuleRepository: Send + Sync {
    async fn create(&self, req: &CreateRuleRequest) -> Result<Rule, OrionError>;
    async fn get_by_id(&self, id: &str) -> Result<Rule, OrionError>;
    async fn list(&self, filter: &RuleFilter) -> Result<Vec<Rule>, OrionError>;
    async fn list_paginated(
        &self,
        filter: &RuleFilter,
    ) -> Result<PaginatedResult<Rule>, OrionError>;
    async fn update(&self, id: &str, req: &UpdateRuleRequest) -> Result<Rule, OrionError>;
    async fn delete(&self, id: &str) -> Result<(), OrionError>;
    async fn list_active(&self) -> Result<Vec<Rule>, OrionError>;
    async fn update_status(
        &self,
        id: &str,
        status: &str,
        expected_version: Option<i64>,
    ) -> Result<Rule, OrionError>;
    async fn count_versions(&self, id: &str) -> Result<i64, OrionError>;
    async fn bulk_create(
        &self,
        rules: &[CreateRuleRequest],
    ) -> Result<Vec<Result<Rule, OrionError>>, OrionError>;
    /// List version history for a rule.
    async fn list_versions(
        &self,
        rule_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<PaginatedResult<models::RuleVersion>, OrionError>;
    /// Database connectivity check.
    async fn ping(&self) -> Result<(), OrionError>;
}

// -- SQLite implementation --

pub struct SqliteRuleRepository {
    pool: SqlitePool,
}

impl SqliteRuleRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

/// Data needed to insert a rule version record.
struct VersionData<'a> {
    rule_id: &'a str,
    version: i64,
    name: &'a str,
    description: Option<&'a str>,
    channel: &'a str,
    priority: i64,
    status: &'a str,
    condition_json: &'a str,
    tasks_json: &'a str,
    tags_json: &'a str,
    continue_on_error: bool,
}

/// Insert a rule version record using the given executor (pool or transaction).
async fn insert_version<'e, E>(executor: E, v: &VersionData<'_>) -> Result<(), OrionError>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query(
        r#"INSERT INTO rule_versions (rule_id, version, name, description, channel, priority, status, condition_json, tasks_json, tags, continue_on_error)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(v.rule_id)
    .bind(v.version)
    .bind(v.name)
    .bind(v.description)
    .bind(v.channel)
    .bind(v.priority)
    .bind(v.status)
    .bind(v.condition_json)
    .bind(v.tasks_json)
    .bind(v.tags_json)
    .bind(v.continue_on_error)
    .execute(executor)
    .await?;
    Ok(())
}

fn build_where_clause(filter: &RuleFilter) -> (String, Vec<String>) {
    let mut clause = String::from("WHERE 1=1");
    let mut binds: Vec<String> = Vec::new();

    if let Some(ref status) = filter.status {
        clause.push_str(" AND status = ?");
        binds.push(status.clone());
    }
    if let Some(ref channel) = filter.channel {
        clause.push_str(" AND channel = ?");
        binds.push(channel.clone());
    }
    if let Some(ref tag) = filter.tag {
        clause.push_str(" AND tags LIKE ? ESCAPE '\\'");
        let escaped = tag
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        binds.push(format!("%\"{}\"%", escaped));
    }

    (clause, binds)
}

#[async_trait]
impl RuleRepository for SqliteRuleRepository {
    async fn create(&self, req: &CreateRuleRequest) -> Result<Rule, OrionError> {
        crate::metrics::timed_db_op("rules.create", async {
            let id = req
                .id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let condition_json = serde_json::to_string(&req.condition)?;
            let tasks_json = serde_json::to_string(&req.tasks)?;
            let tags_json = serde_json::to_string(&req.tags)?;

            let mut tx = self.pool.begin().await?;

            sqlx::query(
                r#"INSERT INTO rules (id, name, description, channel, priority, condition_json, tasks_json, tags, continue_on_error)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
            )
            .bind(&id)
            .bind(&req.name)
            .bind(&req.description)
            .bind(&req.channel)
            .bind(req.priority)
            .bind(&condition_json)
            .bind(&tasks_json)
            .bind(&tags_json)
            .bind(req.continue_on_error)
            .execute(&mut *tx)
            .await?;

            insert_version(
                &mut *tx,
                &VersionData {
                    rule_id: &id,
                    version: 1,
                    name: &req.name,
                    description: req.description.as_deref(),
                    channel: &req.channel,
                    priority: req.priority,
                    status: models::RULE_STATUS_ACTIVE,
                    condition_json: &condition_json,
                    tasks_json: &tasks_json,
                    tags_json: &tags_json,
                    continue_on_error: req.continue_on_error,
                },
            )
            .await?;

            tx.commit().await?;

            self.get_by_id(&id).await
        })
        .await
    }

    async fn get_by_id(&self, id: &str) -> Result<Rule, OrionError> {
        crate::metrics::timed_db_op("rules.get_by_id", async {
            sqlx::query_as::<_, Rule>("SELECT * FROM rules WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?
                .ok_or_else(|| OrionError::NotFound(format!("Rule '{}' not found", id)))
        })
        .await
    }

    async fn list(&self, filter: &RuleFilter) -> Result<Vec<Rule>, OrionError> {
        let (where_clause, binds) = build_where_clause(filter);
        let query = format!("SELECT * FROM rules {where_clause} ORDER BY priority DESC, name ASC");

        let mut q = sqlx::query_as::<_, Rule>(&query);
        for b in &binds {
            q = q.bind(b);
        }

        Ok(q.fetch_all(&self.pool).await?)
    }

    async fn list_paginated(
        &self,
        filter: &RuleFilter,
    ) -> Result<PaginatedResult<Rule>, OrionError> {
        crate::metrics::timed_db_op("rules.list_paginated", async {
            let (where_clause, binds) = build_where_clause(filter);
            let limit = filter.limit.unwrap_or(50).clamp(1, 1000);
            let offset = filter.offset.unwrap_or(0).max(0);

            let count_query = format!("SELECT COUNT(*) FROM rules {where_clause}");
            let mut cq = sqlx::query_as::<_, (i64,)>(&count_query);
            for b in &binds {
                cq = cq.bind(b);
            }
            let (total,) = cq.fetch_one(&self.pool).await?;

            let data_query = format!(
                "SELECT * FROM rules {where_clause} ORDER BY priority DESC, name ASC LIMIT ? OFFSET ?"
            );
            let mut dq = sqlx::query_as::<_, Rule>(&data_query);
            for b in &binds {
                dq = dq.bind(b);
            }
            dq = dq.bind(limit).bind(offset);
            let data = dq.fetch_all(&self.pool).await?;

            Ok(PaginatedResult {
                data,
                total,
                limit,
                offset,
            })
        })
        .await
    }

    async fn update(&self, id: &str, req: &UpdateRuleRequest) -> Result<Rule, OrionError> {
        crate::metrics::timed_db_op("rules.update", async {
            let existing = self.get_by_id(id).await?;

            // Use client-provided version if available, otherwise use the
            // version we just read (still guards against TOCTOU within the
            // same request).
            let expected_version = req.version.unwrap_or(existing.version);

            let name = req.name.as_deref().unwrap_or(&existing.name);
            let description = req
                .description
                .as_deref()
                .or(existing.description.as_deref());
            let channel = req.channel.as_deref().unwrap_or(&existing.channel);
            let priority = req.priority.unwrap_or(existing.priority);
            let status = req.status.as_deref().unwrap_or(&existing.status);
            let continue_on_error = req.continue_on_error.unwrap_or(existing.continue_on_error);

            let condition_json = match &req.condition {
                Some(c) => serde_json::to_string(c)?,
                None => existing.condition_json.clone(),
            };
            let tasks_json = match &req.tasks {
                Some(t) => serde_json::to_string(t)?,
                None => existing.tasks_json.clone(),
            };
            let tags_json = match &req.tags {
                Some(t) => serde_json::to_string(t)?,
                None => existing.tags.clone(),
            };

            let new_version = expected_version + 1;

            let mut tx = self.pool.begin().await?;

            let result = sqlx::query(
                r#"UPDATE rules
                   SET name = ?, description = ?, channel = ?, priority = ?,
                       version = ?, status = ?, condition_json = ?, tasks_json = ?,
                       tags = ?, continue_on_error = ?, updated_at = datetime('now')
                   WHERE id = ? AND version = ?"#,
            )
            .bind(name)
            .bind(description)
            .bind(channel)
            .bind(priority)
            .bind(new_version)
            .bind(status)
            .bind(&condition_json)
            .bind(&tasks_json)
            .bind(&tags_json)
            .bind(continue_on_error)
            .bind(id)
            .bind(expected_version)
            .execute(&mut *tx)
            .await?;

            if result.rows_affected() == 0 {
                return Err(OrionError::Conflict(format!(
                    "Rule '{}' has been modified by another request (expected version {})",
                    id, expected_version
                )));
            }

            insert_version(
                &mut *tx,
                &VersionData {
                    rule_id: id,
                    version: new_version,
                    name,
                    description,
                    channel,
                    priority,
                    status,
                    condition_json: &condition_json,
                    tasks_json: &tasks_json,
                    tags_json: &tags_json,
                    continue_on_error,
                },
            )
            .await?;

            tx.commit().await?;

            self.get_by_id(id).await
        })
        .await
    }

    async fn delete(&self, id: &str) -> Result<(), OrionError> {
        crate::metrics::timed_db_op("rules.delete", async {
            let result = sqlx::query("DELETE FROM rules WHERE id = ?")
                .bind(id)
                .execute(&self.pool)
                .await?;

            if result.rows_affected() == 0 {
                return Err(OrionError::NotFound(format!("Rule '{}' not found", id)));
            }

            Ok(())
        })
        .await
    }

    async fn list_active(&self) -> Result<Vec<Rule>, OrionError> {
        crate::metrics::timed_db_op("rules.list_active", async {
            Ok(sqlx::query_as::<_, Rule>(
                "SELECT * FROM rules WHERE status = 'active' ORDER BY priority DESC",
            )
            .fetch_all(&self.pool)
            .await?)
        })
        .await
    }

    async fn update_status(
        &self,
        id: &str,
        status: &str,
        expected_version: Option<i64>,
    ) -> Result<Rule, OrionError> {
        if !models::VALID_RULE_STATUSES.contains(&status) {
            return Err(OrionError::BadRequest(format!(
                "Invalid status '{}'. Must be one of: {}",
                status,
                models::VALID_RULE_STATUSES.join(", ")
            )));
        }

        crate::metrics::timed_db_op("rules.update_status", async {
            let existing = self.get_by_id(id).await?;
            let expected_version = expected_version.unwrap_or(existing.version);
            let new_version = expected_version + 1;

            let mut tx = self.pool.begin().await?;

            let result = sqlx::query(
                "UPDATE rules SET status = ?, version = ?, updated_at = datetime('now') WHERE id = ? AND version = ?",
            )
            .bind(status)
            .bind(new_version)
            .bind(id)
            .bind(expected_version)
            .execute(&mut *tx)
            .await?;

            if result.rows_affected() == 0 {
                return Err(OrionError::Conflict(format!(
                    "Rule '{}' has been modified by another request (expected version {})",
                    id, expected_version
                )));
            }

            insert_version(
                &mut *tx,
                &VersionData {
                    rule_id: id,
                    version: new_version,
                    name: &existing.name,
                    description: existing.description.as_deref(),
                    channel: &existing.channel,
                    priority: existing.priority,
                    status,
                    condition_json: &existing.condition_json,
                    tasks_json: &existing.tasks_json,
                    tags_json: &existing.tags,
                    continue_on_error: existing.continue_on_error,
                },
            )
            .await?;

            tx.commit().await?;

            self.get_by_id(id).await
        })
        .await
    }

    async fn count_versions(&self, id: &str) -> Result<i64, OrionError> {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM rule_versions WHERE rule_id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0)
    }

    async fn bulk_create(
        &self,
        rules: &[CreateRuleRequest],
    ) -> Result<Vec<Result<Rule, OrionError>>, OrionError> {
        let mut results = Vec::with_capacity(rules.len());

        for req in rules {
            // Each rule gets its own transaction so partial success is possible
            let result = self.create(req).await;
            results.push(result);
        }

        Ok(results)
    }

    async fn list_versions(
        &self,
        rule_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<PaginatedResult<models::RuleVersion>, OrionError> {
        let limit = limit.clamp(1, 1000);
        let offset = offset.max(0);

        let (total,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM rule_versions WHERE rule_id = ?")
                .bind(rule_id)
                .fetch_one(&self.pool)
                .await?;

        let data = sqlx::query_as::<_, models::RuleVersion>(
            "SELECT * FROM rule_versions WHERE rule_id = ? ORDER BY version DESC LIMIT ? OFFSET ?",
        )
        .bind(rule_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        Ok(PaginatedResult {
            data,
            total,
            limit,
            offset,
        })
    }

    async fn ping(&self) -> Result<(), OrionError> {
        sqlx::query_scalar::<_, i32>("SELECT 1")
            .fetch_one(&self.pool)
            .await?;
        Ok(())
    }
}

/// Convert a Rule DB model to a dataflow-rs Workflow via JSON deserialization.
pub fn rule_to_workflow(rule: &Rule) -> Result<Workflow, OrionError> {
    let tasks: serde_json::Value = serde_json::from_str(&rule.tasks_json)?;
    let condition: serde_json::Value = serde_json::from_str(&rule.condition_json)?;
    let tags: Vec<String> = serde_json::from_str(&rule.tags)?;

    let workflow_json = serde_json::json!({
        "id": rule.id,
        "name": rule.name,
        "description": rule.description,
        "channel": rule.channel,
        "priority": rule.priority,
        "version": rule.version,
        "status": rule.status,
        "condition": condition,
        "tasks": tasks,
        "tags": tags,
        "continue_on_error": rule.continue_on_error,
    });

    // Use from_value to avoid to_string + from_json round-trip
    let workflow: Workflow = serde_json::from_value(workflow_json)?;
    Ok(workflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rule_to_workflow_basic() {
        let rule = Rule {
            id: "test-rule".to_string(),
            name: "Test Rule".to_string(),
            description: Some("A test rule".to_string()),
            channel: "default".to_string(),
            priority: 10,
            version: 1,
            status: "active".to_string(),
            condition_json: "true".to_string(),
            tasks_json: r#"[{"id":"log_task","name":"Log","function":{"name":"log","input":{"message":"hello"}}}]"#.to_string(),
            tags: "[]".to_string(),
            continue_on_error: false,
            created_at: chrono::NaiveDateTime::default(),
            updated_at: chrono::NaiveDateTime::default(),
        };

        let workflow = rule_to_workflow(&rule).unwrap();
        assert_eq!(workflow.id, "test-rule");
        assert_eq!(workflow.name, "Test Rule");
        assert_eq!(workflow.channel, "default");
        assert_eq!(workflow.priority, 10);
    }
}
