use async_trait::async_trait;
use dataflow_rs::Workflow;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::errors::OrionError;
use crate::storage::models::{self, Rule};

// -- DTOs --

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
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
}

#[derive(Debug, Deserialize)]
pub struct StatusChangeRequest {
    pub status: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct RuleFilter {
    pub status: Option<String>,
    pub channel: Option<String>,
    pub tag: Option<String>,
}

// -- Repository trait --

#[async_trait]
pub trait RuleRepository: Send + Sync {
    async fn create(&self, req: &CreateRuleRequest) -> Result<Rule, OrionError>;
    async fn get_by_id(&self, id: &str) -> Result<Rule, OrionError>;
    async fn list(&self, filter: &RuleFilter) -> Result<Vec<Rule>, OrionError>;
    async fn update(&self, id: &str, req: &UpdateRuleRequest) -> Result<Rule, OrionError>;
    async fn delete(&self, id: &str) -> Result<(), OrionError>;
    async fn list_active(&self) -> Result<Vec<Rule>, OrionError>;
    async fn update_status(&self, id: &str, status: &str) -> Result<Rule, OrionError>;
    async fn count_versions(&self, id: &str) -> Result<i64, OrionError>;
    async fn bulk_create(
        &self,
        rules: &[CreateRuleRequest],
    ) -> Result<Vec<Result<Rule, OrionError>>, OrionError>;
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

#[async_trait]
impl RuleRepository for SqliteRuleRepository {
    async fn create(&self, req: &CreateRuleRequest) -> Result<Rule, OrionError> {
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
    }

    async fn get_by_id(&self, id: &str) -> Result<Rule, OrionError> {
        sqlx::query_as::<_, Rule>("SELECT * FROM rules WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| OrionError::NotFound(format!("Rule '{}' not found", id)))
    }

    async fn list(&self, filter: &RuleFilter) -> Result<Vec<Rule>, OrionError> {
        let mut query = String::from("SELECT * FROM rules WHERE 1=1");
        let mut binds: Vec<String> = Vec::new();

        if let Some(ref status) = filter.status {
            query.push_str(" AND status = ?");
            binds.push(status.clone());
        }
        if let Some(ref channel) = filter.channel {
            query.push_str(" AND channel = ?");
            binds.push(channel.clone());
        }
        if let Some(ref tag) = filter.tag {
            query.push_str(" AND tags LIKE ?");
            binds.push(format!("%\"{}\"%", tag));
        }

        query.push_str(" ORDER BY priority DESC, name ASC");

        let mut q = sqlx::query_as::<_, Rule>(&query);
        for b in &binds {
            q = q.bind(b);
        }

        Ok(q.fetch_all(&self.pool).await?)
    }

    async fn update(&self, id: &str, req: &UpdateRuleRequest) -> Result<Rule, OrionError> {
        let existing = self.get_by_id(id).await?;

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

        let new_version = existing.version + 1;

        let mut tx = self.pool.begin().await?;

        sqlx::query(
            r#"UPDATE rules
               SET name = ?, description = ?, channel = ?, priority = ?,
                   version = ?, status = ?, condition_json = ?, tasks_json = ?,
                   tags = ?, continue_on_error = ?, updated_at = datetime('now')
               WHERE id = ?"#,
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
        .execute(&mut *tx)
        .await?;

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
    }

    async fn delete(&self, id: &str) -> Result<(), OrionError> {
        let result = sqlx::query("DELETE FROM rules WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(OrionError::NotFound(format!("Rule '{}' not found", id)));
        }

        Ok(())
    }

    async fn list_active(&self) -> Result<Vec<Rule>, OrionError> {
        Ok(sqlx::query_as::<_, Rule>(
            "SELECT * FROM rules WHERE status = 'active' ORDER BY priority DESC",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    async fn update_status(&self, id: &str, status: &str) -> Result<Rule, OrionError> {
        if !models::VALID_RULE_STATUSES.contains(&status) {
            return Err(OrionError::BadRequest(format!(
                "Invalid status '{}'. Must be one of: {}",
                status,
                models::VALID_RULE_STATUSES.join(", ")
            )));
        }

        let existing = self.get_by_id(id).await?;
        let new_version = existing.version + 1;

        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "UPDATE rules SET status = ?, version = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(status)
        .bind(new_version)
        .bind(id)
        .execute(&mut *tx)
        .await?;

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
        let mut tx = self.pool.begin().await?;

        for req in rules {
            let id = req
                .id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let condition_json = match serde_json::to_string(&req.condition) {
                Ok(v) => v,
                Err(e) => {
                    results.push(Err(OrionError::from(e)));
                    continue;
                }
            };
            let tasks_json = match serde_json::to_string(&req.tasks) {
                Ok(v) => v,
                Err(e) => {
                    results.push(Err(OrionError::from(e)));
                    continue;
                }
            };
            let tags_json = match serde_json::to_string(&req.tags) {
                Ok(v) => v,
                Err(e) => {
                    results.push(Err(OrionError::from(e)));
                    continue;
                }
            };

            let insert_result = sqlx::query(
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
            .await;

            match insert_result {
                Ok(_) => {
                    if let Err(e) = insert_version(
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
                    .await
                    {
                        results.push(Err(e));
                        continue;
                    }
                    results.push(Ok(Rule {
                        id,
                        name: req.name.clone(),
                        description: req.description.clone(),
                        channel: req.channel.clone(),
                        priority: req.priority,
                        version: 1,
                        status: models::RULE_STATUS_ACTIVE.to_string(),
                        condition_json,
                        tasks_json,
                        tags: tags_json,
                        continue_on_error: req.continue_on_error,
                        created_at: chrono::Utc::now().naive_utc(),
                        updated_at: chrono::Utc::now().naive_utc(),
                    }));
                }
                Err(e) => results.push(Err(OrionError::Storage(e))),
            }
        }

        tx.commit().await?;
        Ok(results)
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

    let workflow = Workflow::from_json(&serde_json::to_string(&workflow_json)?)?;
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
