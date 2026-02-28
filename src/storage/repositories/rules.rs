use async_trait::async_trait;
use dataflow_rs::Workflow;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::errors::OrionError;
use crate::storage::models::Rule;

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
    pub rule_id: Option<String>,
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
    pub tags: Option<Vec<String>>,
    pub continue_on_error: Option<bool>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct StatusChangeRequest {
    pub status: String,
    pub rollout_percentage: Option<i64>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct RolloutUpdateRequest {
    pub rollout_percentage: i64,
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
    /// Create a new rule as draft v1.
    async fn create(&self, req: &CreateRuleRequest) -> Result<Rule, OrionError>;
    /// Get the latest version of a rule.
    async fn get_by_id(&self, rule_id: &str) -> Result<Rule, OrionError>;
    /// Get a specific version of a rule.
    async fn get_version(&self, rule_id: &str, version: i64) -> Result<Rule, OrionError>;
    /// List rules using the current_rules view (latest version per rule_id).
    async fn list(&self, filter: &RuleFilter) -> Result<Vec<Rule>, OrionError>;
    /// List rules with pagination using the current_rules view.
    async fn list_paginated(
        &self,
        filter: &RuleFilter,
    ) -> Result<PaginatedResult<Rule>, OrionError>;
    /// Update the draft version of a rule. Errors if no draft exists.
    async fn update_draft(
        &self,
        rule_id: &str,
        req: &UpdateRuleRequest,
    ) -> Result<Rule, OrionError>;
    /// Delete all versions of a rule.
    async fn delete(&self, rule_id: &str) -> Result<(), OrionError>;
    /// List all active rules for engine loading.
    async fn list_active(&self) -> Result<Vec<Rule>, OrionError>;
    /// Activate the draft version of a rule with a rollout percentage.
    async fn activate(&self, rule_id: &str, rollout_pct: i64) -> Result<Rule, OrionError>;
    /// Archive the latest active version of a rule.
    async fn archive(&self, rule_id: &str) -> Result<Rule, OrionError>;
    /// Update rollout percentage of an active pair.
    async fn update_rollout(&self, rule_id: &str, pct: i64) -> Result<Rule, OrionError>;
    /// Create a new draft version by copying the latest active version.
    async fn create_new_version(&self, rule_id: &str) -> Result<Rule, OrionError>;
    /// Bulk create rules as draft v1.
    async fn bulk_create(
        &self,
        rules: &[CreateRuleRequest],
    ) -> Result<Vec<Result<Rule, OrionError>>, OrionError>;
    /// List all versions of a rule.
    async fn list_versions(
        &self,
        rule_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<PaginatedResult<Rule>, OrionError>;
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
            let rule_id = req
                .rule_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let condition_json = serde_json::to_string(&req.condition)?;
            let tasks_json = serde_json::to_string(&req.tasks)?;
            let tags_json = serde_json::to_string(&req.tags)?;

            sqlx::query(
                r#"INSERT INTO rules (rule_id, version, name, description, channel, priority, status, rollout_percentage, condition_json, tasks_json, tags, continue_on_error)
                   VALUES (?, 1, ?, ?, ?, ?, 'draft', 100, ?, ?, ?, ?)"#,
            )
            .bind(&rule_id)
            .bind(&req.name)
            .bind(&req.description)
            .bind(&req.channel)
            .bind(req.priority)
            .bind(&condition_json)
            .bind(&tasks_json)
            .bind(&tags_json)
            .bind(req.continue_on_error)
            .execute(&self.pool)
            .await?;

            self.get_version(&rule_id, 1).await
        })
        .await
    }

    async fn get_by_id(&self, rule_id: &str) -> Result<Rule, OrionError> {
        crate::metrics::timed_db_op("rules.get_by_id", async {
            sqlx::query_as::<_, Rule>(
                "SELECT * FROM rules WHERE rule_id = ? ORDER BY version DESC LIMIT 1",
            )
            .bind(rule_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| OrionError::NotFound(format!("Rule '{}' not found", rule_id)))
        })
        .await
    }

    async fn get_version(&self, rule_id: &str, version: i64) -> Result<Rule, OrionError> {
        sqlx::query_as::<_, Rule>("SELECT * FROM rules WHERE rule_id = ? AND version = ?")
            .bind(rule_id)
            .bind(version)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| {
                OrionError::NotFound(format!("Rule '{}' version {} not found", rule_id, version))
            })
    }

    async fn list(&self, filter: &RuleFilter) -> Result<Vec<Rule>, OrionError> {
        let (where_clause, binds) = build_where_clause(filter);
        let query =
            format!("SELECT * FROM current_rules {where_clause} ORDER BY priority DESC, name ASC");

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

            let count_query = format!("SELECT COUNT(*) FROM current_rules {where_clause}");
            let mut cq = sqlx::query_as::<_, (i64,)>(&count_query);
            for b in &binds {
                cq = cq.bind(b);
            }
            let (total,) = cq.fetch_one(&self.pool).await?;

            let data_query = format!(
                "SELECT * FROM current_rules {where_clause} ORDER BY priority DESC, name ASC LIMIT ? OFFSET ?"
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

    async fn update_draft(
        &self,
        rule_id: &str,
        req: &UpdateRuleRequest,
    ) -> Result<Rule, OrionError> {
        crate::metrics::timed_db_op("rules.update_draft", async {
            let existing = sqlx::query_as::<_, Rule>(
                "SELECT * FROM rules WHERE rule_id = ? AND status = 'draft'",
            )
            .bind(rule_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| {
                OrionError::BadRequest(format!("No draft version found for rule '{}'", rule_id))
            })?;

            let name = req.name.as_deref().unwrap_or(&existing.name);
            let description = req
                .description
                .as_deref()
                .or(existing.description.as_deref());
            let channel = req.channel.as_deref().unwrap_or(&existing.channel);
            let priority = req.priority.unwrap_or(existing.priority);
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

            sqlx::query(
                r#"UPDATE rules
                   SET name = ?, description = ?, channel = ?, priority = ?,
                       condition_json = ?, tasks_json = ?, tags = ?, continue_on_error = ?
                   WHERE rule_id = ? AND status = 'draft'"#,
            )
            .bind(name)
            .bind(description)
            .bind(channel)
            .bind(priority)
            .bind(&condition_json)
            .bind(&tasks_json)
            .bind(&tags_json)
            .bind(continue_on_error)
            .bind(rule_id)
            .execute(&self.pool)
            .await?;

            self.get_version(rule_id, existing.version).await
        })
        .await
    }

    async fn delete(&self, rule_id: &str) -> Result<(), OrionError> {
        crate::metrics::timed_db_op("rules.delete", async {
            let result = sqlx::query("DELETE FROM rules WHERE rule_id = ?")
                .bind(rule_id)
                .execute(&self.pool)
                .await?;

            if result.rows_affected() == 0 {
                return Err(OrionError::NotFound(format!(
                    "Rule '{}' not found",
                    rule_id
                )));
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

    async fn activate(&self, rule_id: &str, rollout_pct: i64) -> Result<Rule, OrionError> {
        if !(0..=100).contains(&rollout_pct) {
            return Err(OrionError::BadRequest(
                "rollout_percentage must be between 0 and 100".to_string(),
            ));
        }

        crate::metrics::timed_db_op("rules.activate", async {
            let mut tx = self.pool.begin().await?;

            // Find the draft
            let draft = sqlx::query_as::<_, Rule>(
                "SELECT * FROM rules WHERE rule_id = ? AND status = 'draft'",
            )
            .bind(rule_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| {
                OrionError::BadRequest(format!("No draft version found for rule '{}'", rule_id))
            })?;

            // Find current active version(s)
            let active_versions: Vec<Rule> = sqlx::query_as::<_, Rule>(
                "SELECT * FROM rules WHERE rule_id = ? AND status = 'active' ORDER BY version DESC",
            )
            .bind(rule_id)
            .fetch_all(&mut *tx)
            .await?;

            if rollout_pct == 100 {
                // Archive all current active versions
                for active in &active_versions {
                    sqlx::query(
                        "UPDATE rules SET status = 'archived' WHERE rule_id = ? AND version = ?",
                    )
                    .bind(rule_id)
                    .bind(active.version)
                    .execute(&mut *tx)
                    .await?;
                }

                // Activate the draft at 100%
                sqlx::query(
                    "UPDATE rules SET status = 'active', rollout_percentage = 100 WHERE rule_id = ? AND version = ?",
                )
                .bind(rule_id)
                .bind(draft.version)
                .execute(&mut *tx)
                .await?;
            } else {
                // Partial rollout: keep the latest active, archive any others
                if active_versions.len() > 1 {
                    for active in &active_versions[1..] {
                        sqlx::query(
                            "UPDATE rules SET status = 'archived' WHERE rule_id = ? AND version = ?",
                        )
                        .bind(rule_id)
                        .bind(active.version)
                        .execute(&mut *tx)
                        .await?;
                    }
                }

                if let Some(primary_active) = active_versions.first() {
                    // Adjust existing active to complement percentage
                    sqlx::query(
                        "UPDATE rules SET rollout_percentage = ? WHERE rule_id = ? AND version = ?",
                    )
                    .bind(100 - rollout_pct)
                    .bind(rule_id)
                    .bind(primary_active.version)
                    .execute(&mut *tx)
                    .await?;
                }

                // Activate the draft with the specified rollout
                sqlx::query(
                    "UPDATE rules SET status = 'active', rollout_percentage = ? WHERE rule_id = ? AND version = ?",
                )
                .bind(rollout_pct)
                .bind(rule_id)
                .bind(draft.version)
                .execute(&mut *tx)
                .await?;
            }

            tx.commit().await?;

            self.get_version(rule_id, draft.version).await
        })
        .await
    }

    async fn archive(&self, rule_id: &str) -> Result<Rule, OrionError> {
        crate::metrics::timed_db_op("rules.archive", async {
            // Find the latest active version
            let active = sqlx::query_as::<_, Rule>(
                "SELECT * FROM rules WHERE rule_id = ? AND status = 'active' ORDER BY version DESC LIMIT 1",
            )
            .bind(rule_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| {
                OrionError::BadRequest(format!("No active version found for rule '{}'", rule_id))
            })?;

            // Archive all active versions
            sqlx::query(
                "UPDATE rules SET status = 'archived' WHERE rule_id = ? AND status = 'active'",
            )
            .bind(rule_id)
            .execute(&self.pool)
            .await?;

            self.get_version(rule_id, active.version).await
        })
        .await
    }

    async fn update_rollout(&self, rule_id: &str, pct: i64) -> Result<Rule, OrionError> {
        if !(1..=100).contains(&pct) {
            return Err(OrionError::BadRequest(
                "rollout_percentage must be between 1 and 100".to_string(),
            ));
        }

        crate::metrics::timed_db_op("rules.update_rollout", async {
            let mut tx = self.pool.begin().await?;

            // Get active versions ordered by version DESC (newest first)
            let active_versions: Vec<Rule> = sqlx::query_as::<_, Rule>(
                "SELECT * FROM rules WHERE rule_id = ? AND status = 'active' ORDER BY version DESC",
            )
            .bind(rule_id)
            .fetch_all(&mut *tx)
            .await?;

            if active_versions.is_empty() {
                return Err(OrionError::BadRequest(format!(
                    "No active versions found for rule '{}'",
                    rule_id
                )));
            }

            if active_versions.len() == 1 {
                if pct == 100 {
                    // Already at 100% with one version, just confirm
                    return self.get_version(rule_id, active_versions[0].version).await;
                }
                return Err(OrionError::BadRequest(
                    "Cannot set partial rollout with only one active version".to_string(),
                ));
            }

            let newer = &active_versions[0];
            let older = &active_versions[1];

            if pct == 100 {
                // Archive the older version
                sqlx::query(
                    "UPDATE rules SET status = 'archived' WHERE rule_id = ? AND version = ?",
                )
                .bind(rule_id)
                .bind(older.version)
                .execute(&mut *tx)
                .await?;

                // Set newer to 100%
                sqlx::query(
                    "UPDATE rules SET rollout_percentage = 100 WHERE rule_id = ? AND version = ?",
                )
                .bind(rule_id)
                .bind(newer.version)
                .execute(&mut *tx)
                .await?;
            } else {
                // Update both percentages
                sqlx::query(
                    "UPDATE rules SET rollout_percentage = ? WHERE rule_id = ? AND version = ?",
                )
                .bind(pct)
                .bind(rule_id)
                .bind(newer.version)
                .execute(&mut *tx)
                .await?;

                sqlx::query(
                    "UPDATE rules SET rollout_percentage = ? WHERE rule_id = ? AND version = ?",
                )
                .bind(100 - pct)
                .bind(rule_id)
                .bind(older.version)
                .execute(&mut *tx)
                .await?;
            }

            tx.commit().await?;

            self.get_version(rule_id, newer.version).await
        })
        .await
    }

    async fn create_new_version(&self, rule_id: &str) -> Result<Rule, OrionError> {
        crate::metrics::timed_db_op("rules.create_new_version", async {
            // Check no draft already exists
            let existing_draft = sqlx::query_as::<_, Rule>(
                "SELECT * FROM rules WHERE rule_id = ? AND status = 'draft'",
            )
            .bind(rule_id)
            .fetch_optional(&self.pool)
            .await?;

            if existing_draft.is_some() {
                return Err(OrionError::Conflict(format!(
                    "Rule '{}' already has a draft version",
                    rule_id
                )));
            }

            // Find the latest version to copy from
            let latest = self.get_by_id(rule_id).await?;

            let new_version = latest.version + 1;

            sqlx::query(
                r#"INSERT INTO rules (rule_id, version, name, description, channel, priority, status, rollout_percentage, condition_json, tasks_json, tags, continue_on_error)
                   VALUES (?, ?, ?, ?, ?, ?, 'draft', 100, ?, ?, ?, ?)"#,
            )
            .bind(rule_id)
            .bind(new_version)
            .bind(&latest.name)
            .bind(&latest.description)
            .bind(&latest.channel)
            .bind(latest.priority)
            .bind(&latest.condition_json)
            .bind(&latest.tasks_json)
            .bind(&latest.tags)
            .bind(latest.continue_on_error)
            .execute(&self.pool)
            .await?;

            self.get_version(rule_id, new_version).await
        })
        .await
    }

    async fn bulk_create(
        &self,
        rules: &[CreateRuleRequest],
    ) -> Result<Vec<Result<Rule, OrionError>>, OrionError> {
        let mut results = Vec::with_capacity(rules.len());

        for req in rules {
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
    ) -> Result<PaginatedResult<Rule>, OrionError> {
        let limit = limit.clamp(1, 1000);
        let offset = offset.max(0);

        let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM rules WHERE rule_id = ?")
            .bind(rule_id)
            .fetch_one(&self.pool)
            .await?;

        let data = sqlx::query_as::<_, Rule>(
            "SELECT * FROM rules WHERE rule_id = ? ORDER BY version DESC LIMIT ? OFFSET ?",
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
        "id": rule.rule_id,
        "name": rule.name,
        "description": rule.description,
        "channel": rule.channel,
        "priority": rule.priority,
        "version": rule.version,
        "status": "active",
        "condition": condition,
        "tasks": tasks,
        "tags": tags,
        "continue_on_error": rule.continue_on_error,
    });

    let workflow: Workflow = serde_json::from_value(workflow_json)?;
    Ok(workflow)
}

/// Convert a Rule to a Workflow with rollout-aware condition wrapping and unique ID.
pub fn rule_to_workflow_with_rollout(
    rule: &Rule,
    bucket_min: i64,
    bucket_max: i64,
) -> Result<Workflow, OrionError> {
    let tasks: serde_json::Value = serde_json::from_str(&rule.tasks_json)?;
    let condition: serde_json::Value = serde_json::from_str(&rule.condition_json)?;
    let tags: Vec<String> = serde_json::from_str(&rule.tags)?;

    // Wrap condition with bucket range check
    let wrapped_condition = serde_json::json!({
        "and": [
            condition,
            {">=": [{"var": "_rollout_bucket"}, bucket_min]},
            {"<": [{"var": "_rollout_bucket"}, bucket_max]}
        ]
    });

    let workflow_json = serde_json::json!({
        "id": format!("{}:v{}", rule.rule_id, rule.version),
        "name": rule.name,
        "description": rule.description,
        "channel": rule.channel,
        "priority": rule.priority,
        "version": rule.version,
        "status": rule.status,
        "condition": wrapped_condition,
        "tasks": tasks,
        "tags": tags,
        "continue_on_error": rule.continue_on_error,
    });

    let workflow: Workflow = serde_json::from_value(workflow_json)?;
    Ok(workflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rule_to_workflow_basic() {
        let rule = Rule {
            rule_id: "test-rule".to_string(),
            name: "Test Rule".to_string(),
            description: Some("A test rule".to_string()),
            channel: "default".to_string(),
            priority: 10,
            version: 1,
            status: "active".to_string(),
            rollout_percentage: 100,
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
