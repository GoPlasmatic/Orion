use crate::storage::DbPool;
use async_trait::async_trait;
use dataflow_rs::Workflow as DataflowWorkflow;
use sea_query::{Asterisk, Condition, Expr, Func, Order, Query};
use sea_query_binder::SqlxBinder;
use serde::{Deserialize, Serialize};

use crate::errors::OrionError;
use crate::storage::models::Workflow;
use crate::storage::{
    query_builder,
    schema::{CurrentWorkflows, Workflows},
};

use super::helpers::{clamp_pagination, optional_string_value};

#[derive(Debug, Serialize)]
pub struct PaginatedResult<T: Serialize> {
    pub data: Vec<T>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

// -- DTOs --

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateWorkflowRequest {
    pub workflow_id: Option<String>,
    pub name: String,
    pub description: Option<String>,
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

fn default_condition() -> serde_json::Value {
    serde_json::Value::Bool(true)
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateWorkflowRequest {
    pub name: Option<String>,
    pub description: Option<String>,
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
pub struct WorkflowFilter {
    pub status: Option<String>,
    pub tag: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// Column to sort by: priority (default), name, status, created_at, updated_at.
    pub sort_by: Option<String>,
    /// Sort direction: asc or desc (default).
    pub sort_order: Option<String>,
}

// -- Repository trait --

#[async_trait]
pub trait WorkflowRepository: Send + Sync {
    /// Create a new workflow as draft v1.
    async fn create(&self, req: &CreateWorkflowRequest) -> Result<Workflow, OrionError>;
    /// Get the latest version of a workflow.
    async fn get_by_id(&self, workflow_id: &str) -> Result<Workflow, OrionError>;
    /// Get a specific version of a workflow.
    async fn get_version(&self, workflow_id: &str, version: i64) -> Result<Workflow, OrionError>;
    /// List workflows using the current_workflows view (latest version per workflow_id).
    async fn list(&self, filter: &WorkflowFilter) -> Result<Vec<Workflow>, OrionError>;
    /// List workflows with pagination using the current_workflows view.
    async fn list_paginated(
        &self,
        filter: &WorkflowFilter,
    ) -> Result<PaginatedResult<Workflow>, OrionError>;
    /// Update the draft version of a workflow. Errors if no draft exists.
    async fn update_draft(
        &self,
        workflow_id: &str,
        req: &UpdateWorkflowRequest,
    ) -> Result<Workflow, OrionError>;
    /// Delete all versions of a workflow.
    async fn delete(&self, workflow_id: &str) -> Result<(), OrionError>;
    /// List all active workflows for engine loading.
    async fn list_active(&self) -> Result<Vec<Workflow>, OrionError>;
    /// List active workflows for the given workflow IDs.
    async fn list_active_by_ids(&self, workflow_ids: &[&str]) -> Result<Vec<Workflow>, OrionError>;
    /// Activate the draft version of a workflow with a rollout percentage.
    async fn activate(&self, workflow_id: &str, rollout_pct: i64) -> Result<Workflow, OrionError>;
    /// Archive the latest active version of a workflow.
    async fn archive(&self, workflow_id: &str) -> Result<Workflow, OrionError>;
    /// Update rollout percentage of an active pair.
    async fn update_rollout(&self, workflow_id: &str, pct: i64) -> Result<Workflow, OrionError>;
    /// Create a new draft version by copying the latest active version.
    async fn create_new_version(&self, workflow_id: &str) -> Result<Workflow, OrionError>;
    /// Bulk create workflows as draft v1.
    async fn bulk_create(
        &self,
        workflows: &[CreateWorkflowRequest],
    ) -> Result<Vec<Result<Workflow, OrionError>>, OrionError>;
    /// List all versions of a workflow.
    async fn list_versions(
        &self,
        workflow_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<PaginatedResult<Workflow>, OrionError>;
    /// Database connectivity check.
    async fn ping(&self) -> Result<(), OrionError>;
}

// -- SQL implementation --

pub struct SqlWorkflowRepository {
    pool: DbPool,
}

impl SqlWorkflowRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

fn build_condition(filter: &WorkflowFilter) -> Condition {
    let mut cond = Condition::all();
    if let Some(ref status) = filter.status {
        cond = cond.add(Expr::col(Workflows::Status).eq(status.as_str()));
    }
    if let Some(ref tag) = filter.tag {
        let escaped = tag
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        cond = cond.add(Expr::col(Workflows::Tags).like(format!("%\"{}\"%", escaped)));
    }
    cond
}

#[async_trait]
impl WorkflowRepository for SqlWorkflowRepository {
    async fn create(&self, req: &CreateWorkflowRequest) -> Result<Workflow, OrionError> {
        crate::metrics::timed_db_op("workflows.create", async {
            let workflow_id = req
                .workflow_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let condition_json = serde_json::to_string(&req.condition)?;
            let tasks_json = serde_json::to_string(&req.tasks)?;
            let tags_json = serde_json::to_string(&req.tags)?;

            let description_val = optional_string_value(req.description.as_deref());

            let (sql, values) = Query::insert()
                .into_table(Workflows::Table)
                .columns([
                    Workflows::WorkflowId,
                    Workflows::Version,
                    Workflows::Name,
                    Workflows::Description,
                    Workflows::Priority,
                    Workflows::Status,
                    Workflows::RolloutPercentage,
                    Workflows::ConditionJson,
                    Workflows::TasksJson,
                    Workflows::Tags,
                    Workflows::ContinueOnError,
                ])
                .values_panic([
                    Expr::val(workflow_id.as_str()).into(),
                    Expr::val(1i64).into(),
                    Expr::val(req.name.as_str()).into(),
                    Expr::val(description_val).into(),
                    Expr::val(req.priority).into(),
                    Expr::val("draft").into(),
                    Expr::val(100i64).into(),
                    Expr::val(condition_json.as_str()).into(),
                    Expr::val(tasks_json.as_str()).into(),
                    Expr::val(tags_json.as_str()).into(),
                    Expr::val(req.continue_on_error).into(),
                ])
                .build_sqlx(query_builder());

            sqlx::query_with(&sql, values).execute(&self.pool).await?;

            self.get_version(&workflow_id, 1).await
        })
        .await
    }

    async fn get_by_id(&self, workflow_id: &str) -> Result<Workflow, OrionError> {
        crate::metrics::timed_db_op("workflows.get_by_id", async {
            let (sql, values) = Query::select()
                .column(Asterisk)
                .from(Workflows::Table)
                .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                .order_by(Workflows::Version, Order::Desc)
                .limit(1)
                .build_sqlx(query_builder());

            sqlx::query_as_with::<_, Workflow, _>(&sql, values)
                .fetch_optional(&self.pool)
                .await?
                .ok_or_else(|| {
                    OrionError::NotFound(format!("Workflow '{}' not found", workflow_id))
                })
        })
        .await
    }

    async fn get_version(&self, workflow_id: &str, version: i64) -> Result<Workflow, OrionError> {
        let (sql, values) = Query::select()
            .column(Asterisk)
            .from(Workflows::Table)
            .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
            .and_where(Expr::col(Workflows::Version).eq(version))
            .build_sqlx(query_builder());

        sqlx::query_as_with::<_, Workflow, _>(&sql, values)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| {
                OrionError::NotFound(format!(
                    "Workflow '{}' version {} not found",
                    workflow_id, version
                ))
            })
    }

    async fn list(&self, filter: &WorkflowFilter) -> Result<Vec<Workflow>, OrionError> {
        let cond = build_condition(filter);
        let (sql, values) = Query::select()
            .column(Asterisk)
            .from(CurrentWorkflows::Table)
            .cond_where(cond)
            .order_by(Workflows::Priority, Order::Desc)
            .order_by(Workflows::Name, Order::Asc)
            .build_sqlx(query_builder());

        Ok(sqlx::query_as_with::<_, Workflow, _>(&sql, values)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn list_paginated(
        &self,
        filter: &WorkflowFilter,
    ) -> Result<PaginatedResult<Workflow>, OrionError> {
        crate::metrics::timed_db_op("workflows.list_paginated", async {
            let cond = build_condition(filter);
            let (limit, offset) = clamp_pagination(filter.limit, filter.offset);

            // Count
            let (sql, values) = Query::select()
                .expr(Func::count(Expr::col(Asterisk)))
                .from(CurrentWorkflows::Table)
                .cond_where(cond.clone())
                .build_sqlx(query_builder());
            let (total,): (i64,) = sqlx::query_as_with::<_, (i64,), _>(&sql, values)
                .fetch_one(&self.pool)
                .await?;

            // Sort column mapping
            let sort_iden = match filter.sort_by.as_deref() {
                Some("name") => Workflows::Name,
                Some("status") => Workflows::Status,
                Some("created_at") => Workflows::CreatedAt,
                Some("updated_at") => Workflows::UpdatedAt,
                _ => Workflows::Priority,
            };
            let order = match filter.sort_order.as_deref() {
                Some("asc") => Order::Asc,
                _ => Order::Desc,
            };

            // Data
            let (sql, values) = Query::select()
                .column(Asterisk)
                .from(CurrentWorkflows::Table)
                .cond_where(cond)
                .order_by(sort_iden, order)
                .limit(limit as u64)
                .offset(offset as u64)
                .build_sqlx(query_builder());
            let data = sqlx::query_as_with::<_, Workflow, _>(&sql, values)
                .fetch_all(&self.pool)
                .await?;

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
        workflow_id: &str,
        req: &UpdateWorkflowRequest,
    ) -> Result<Workflow, OrionError> {
        crate::metrics::timed_db_op("workflows.update_draft", async {
            // Fetch existing draft
            let (sql, values) = Query::select()
                .column(Asterisk)
                .from(Workflows::Table)
                .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                .and_where(Expr::col(Workflows::Status).eq("draft"))
                .build_sqlx(query_builder());

            let existing = sqlx::query_as_with::<_, Workflow, _>(&sql, values)
                .fetch_optional(&self.pool)
                .await?
                .ok_or_else(|| {
                    OrionError::BadRequest(format!(
                        "No draft version found for workflow '{}'",
                        workflow_id
                    ))
                })?;

            let name = req.name.as_deref().unwrap_or(&existing.name);
            let description = req
                .description
                .as_deref()
                .or(existing.description.as_deref());
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

            let description_val = optional_string_value(description);

            let (sql, values) = Query::update()
                .table(Workflows::Table)
                .value(Workflows::Name, name)
                .value(Workflows::Description, description_val)
                .value(Workflows::Priority, priority)
                .value(Workflows::ConditionJson, condition_json.as_str())
                .value(Workflows::TasksJson, tasks_json.as_str())
                .value(Workflows::Tags, tags_json.as_str())
                .value(Workflows::ContinueOnError, continue_on_error)
                .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                .and_where(Expr::col(Workflows::Status).eq("draft"))
                .build_sqlx(query_builder());

            sqlx::query_with(&sql, values).execute(&self.pool).await?;

            self.get_version(workflow_id, existing.version).await
        })
        .await
    }

    async fn delete(&self, workflow_id: &str) -> Result<(), OrionError> {
        crate::metrics::timed_db_op("workflows.delete", async {
            let (sql, values) = Query::delete()
                .from_table(Workflows::Table)
                .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                .build_sqlx(query_builder());

            let result = sqlx::query_with(&sql, values).execute(&self.pool).await?;

            if result.rows_affected() == 0 {
                return Err(OrionError::NotFound(format!(
                    "Workflow '{}' not found",
                    workflow_id
                )));
            }

            Ok(())
        })
        .await
    }

    async fn list_active(&self) -> Result<Vec<Workflow>, OrionError> {
        crate::metrics::timed_db_op("workflows.list_active", async {
            let (sql, values) = Query::select()
                .column(Asterisk)
                .from(Workflows::Table)
                .and_where(Expr::col(Workflows::Status).eq("active"))
                .order_by(Workflows::Priority, Order::Desc)
                .build_sqlx(query_builder());

            Ok(sqlx::query_as_with::<_, Workflow, _>(&sql, values)
                .fetch_all(&self.pool)
                .await?)
        })
        .await
    }

    async fn list_active_by_ids(&self, workflow_ids: &[&str]) -> Result<Vec<Workflow>, OrionError> {
        crate::metrics::timed_db_op("workflows.list_active_by_ids", async {
            if workflow_ids.is_empty() {
                return Ok(Vec::new());
            }

            let (sql, values) = Query::select()
                .column(Asterisk)
                .from(Workflows::Table)
                .and_where(Expr::col(Workflows::Status).eq("active"))
                .and_where(Expr::col(Workflows::WorkflowId).is_in(workflow_ids.iter().copied()))
                .order_by(Workflows::Priority, Order::Desc)
                .build_sqlx(query_builder());

            Ok(sqlx::query_as_with::<_, Workflow, _>(&sql, values)
                .fetch_all(&self.pool)
                .await?)
        })
        .await
    }

    async fn activate(&self, workflow_id: &str, rollout_pct: i64) -> Result<Workflow, OrionError> {
        if !(0..=100).contains(&rollout_pct) {
            return Err(OrionError::BadRequest(
                "rollout_percentage must be between 0 and 100".to_string(),
            ));
        }

        crate::metrics::timed_db_op("workflows.activate", async {
            let mut tx = self.pool.begin().await?;

            // Fetch draft version
            let (sql, values) = Query::select()
                .column(Asterisk)
                .from(Workflows::Table)
                .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                .and_where(Expr::col(Workflows::Status).eq("draft"))
                .build_sqlx(query_builder());

            let draft = sqlx::query_as_with::<_, Workflow, _>(&sql, values)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(|| {
                    OrionError::BadRequest(format!(
                        "No draft version found for workflow '{}'",
                        workflow_id
                    ))
                })?;

            // Fetch active versions
            let (sql, values) = Query::select()
                .column(Asterisk)
                .from(Workflows::Table)
                .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                .and_where(Expr::col(Workflows::Status).eq("active"))
                .order_by(Workflows::Version, Order::Desc)
                .build_sqlx(query_builder());

            let active_versions: Vec<Workflow> =
                sqlx::query_as_with::<_, Workflow, _>(&sql, values)
                    .fetch_all(&mut *tx)
                    .await?;

            if rollout_pct == 100 {
                for active in &active_versions {
                    let (sql, values) = Query::update()
                        .table(Workflows::Table)
                        .value(Workflows::Status, "archived")
                        .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                        .and_where(Expr::col(Workflows::Version).eq(active.version))
                        .build_sqlx(query_builder());
                    sqlx::query_with(&sql, values).execute(&mut *tx).await?;
                }

                let (sql, values) = Query::update()
                    .table(Workflows::Table)
                    .value(Workflows::Status, "active")
                    .value(Workflows::RolloutPercentage, 100i64)
                    .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                    .and_where(Expr::col(Workflows::Version).eq(draft.version))
                    .build_sqlx(query_builder());
                sqlx::query_with(&sql, values).execute(&mut *tx).await?;
            } else {
                if active_versions.len() > 1 {
                    for active in &active_versions[1..] {
                        let (sql, values) = Query::update()
                            .table(Workflows::Table)
                            .value(Workflows::Status, "archived")
                            .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                            .and_where(Expr::col(Workflows::Version).eq(active.version))
                            .build_sqlx(query_builder());
                        sqlx::query_with(&sql, values).execute(&mut *tx).await?;
                    }
                }

                if let Some(primary_active) = active_versions.first() {
                    let (sql, values) = Query::update()
                        .table(Workflows::Table)
                        .value(Workflows::RolloutPercentage, 100 - rollout_pct)
                        .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                        .and_where(Expr::col(Workflows::Version).eq(primary_active.version))
                        .build_sqlx(query_builder());
                    sqlx::query_with(&sql, values).execute(&mut *tx).await?;
                }

                let (sql, values) = Query::update()
                    .table(Workflows::Table)
                    .value(Workflows::Status, "active")
                    .value(Workflows::RolloutPercentage, rollout_pct)
                    .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                    .and_where(Expr::col(Workflows::Version).eq(draft.version))
                    .build_sqlx(query_builder());
                sqlx::query_with(&sql, values).execute(&mut *tx).await?;
            }

            tx.commit().await?;

            self.get_version(workflow_id, draft.version).await
        })
        .await
    }

    async fn archive(&self, workflow_id: &str) -> Result<Workflow, OrionError> {
        crate::metrics::timed_db_op("workflows.archive", async {
            // Fetch latest active version
            let (sql, values) = Query::select()
                .column(Asterisk)
                .from(Workflows::Table)
                .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                .and_where(Expr::col(Workflows::Status).eq("active"))
                .order_by(Workflows::Version, Order::Desc)
                .limit(1)
                .build_sqlx(query_builder());

            let active = sqlx::query_as_with::<_, Workflow, _>(&sql, values)
                .fetch_optional(&self.pool)
                .await?
                .ok_or_else(|| {
                    OrionError::BadRequest(format!(
                        "No active version found for workflow '{}'",
                        workflow_id
                    ))
                })?;

            // Archive all active versions
            let (sql, values) = Query::update()
                .table(Workflows::Table)
                .value(Workflows::Status, "archived")
                .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                .and_where(Expr::col(Workflows::Status).eq("active"))
                .build_sqlx(query_builder());

            sqlx::query_with(&sql, values).execute(&self.pool).await?;

            self.get_version(workflow_id, active.version).await
        })
        .await
    }

    async fn update_rollout(&self, workflow_id: &str, pct: i64) -> Result<Workflow, OrionError> {
        if !(1..=100).contains(&pct) {
            return Err(OrionError::BadRequest(
                "rollout_percentage must be between 1 and 100".to_string(),
            ));
        }

        crate::metrics::timed_db_op("workflows.update_rollout", async {
            let mut tx = self.pool.begin().await?;

            // Get active versions ordered by version DESC (newest first)
            let (sql, values) = Query::select()
                .column(Asterisk)
                .from(Workflows::Table)
                .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                .and_where(Expr::col(Workflows::Status).eq("active"))
                .order_by(Workflows::Version, Order::Desc)
                .build_sqlx(query_builder());

            let active_versions: Vec<Workflow> =
                sqlx::query_as_with::<_, Workflow, _>(&sql, values)
                    .fetch_all(&mut *tx)
                    .await?;

            if active_versions.is_empty() {
                return Err(OrionError::BadRequest(format!(
                    "No active versions found for workflow '{}'",
                    workflow_id
                )));
            }

            if active_versions.len() == 1 {
                if pct == 100 {
                    // Already at 100% with one version, just confirm
                    return self
                        .get_version(workflow_id, active_versions[0].version)
                        .await;
                }
                return Err(OrionError::BadRequest(
                    "Cannot set partial rollout with only one active version".to_string(),
                ));
            }

            let newer = &active_versions[0];
            let older = &active_versions[1];

            if pct == 100 {
                // Archive the older version
                let (sql, values) = Query::update()
                    .table(Workflows::Table)
                    .value(Workflows::Status, "archived")
                    .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                    .and_where(Expr::col(Workflows::Version).eq(older.version))
                    .build_sqlx(query_builder());
                sqlx::query_with(&sql, values).execute(&mut *tx).await?;

                // Set newer to 100%
                let (sql, values) = Query::update()
                    .table(Workflows::Table)
                    .value(Workflows::RolloutPercentage, 100i64)
                    .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                    .and_where(Expr::col(Workflows::Version).eq(newer.version))
                    .build_sqlx(query_builder());
                sqlx::query_with(&sql, values).execute(&mut *tx).await?;
            } else {
                // Update both percentages
                let (sql, values) = Query::update()
                    .table(Workflows::Table)
                    .value(Workflows::RolloutPercentage, pct)
                    .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                    .and_where(Expr::col(Workflows::Version).eq(newer.version))
                    .build_sqlx(query_builder());
                sqlx::query_with(&sql, values).execute(&mut *tx).await?;

                let (sql, values) = Query::update()
                    .table(Workflows::Table)
                    .value(Workflows::RolloutPercentage, 100 - pct)
                    .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                    .and_where(Expr::col(Workflows::Version).eq(older.version))
                    .build_sqlx(query_builder());
                sqlx::query_with(&sql, values).execute(&mut *tx).await?;
            }

            tx.commit().await?;

            self.get_version(workflow_id, newer.version).await
        })
        .await
    }

    async fn create_new_version(&self, workflow_id: &str) -> Result<Workflow, OrionError> {
        crate::metrics::timed_db_op("workflows.create_new_version", async {
            // Check no draft already exists
            let (sql, values) = Query::select()
                .column(Asterisk)
                .from(Workflows::Table)
                .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                .and_where(Expr::col(Workflows::Status).eq("draft"))
                .build_sqlx(query_builder());

            let existing_draft = sqlx::query_as_with::<_, Workflow, _>(&sql, values)
                .fetch_optional(&self.pool)
                .await?;

            if existing_draft.is_some() {
                return Err(OrionError::Conflict(format!(
                    "Workflow '{}' already has a draft version",
                    workflow_id
                )));
            }

            // Find the latest version to copy from
            let latest = self.get_by_id(workflow_id).await?;

            let new_version = latest.version + 1;

            let description_val = optional_string_value(latest.description.as_deref());

            let (sql, values) = Query::insert()
                .into_table(Workflows::Table)
                .columns([
                    Workflows::WorkflowId,
                    Workflows::Version,
                    Workflows::Name,
                    Workflows::Description,
                    Workflows::Priority,
                    Workflows::Status,
                    Workflows::RolloutPercentage,
                    Workflows::ConditionJson,
                    Workflows::TasksJson,
                    Workflows::Tags,
                    Workflows::ContinueOnError,
                ])
                .values_panic([
                    Expr::val(workflow_id).into(),
                    Expr::val(new_version).into(),
                    Expr::val(latest.name.as_str()).into(),
                    Expr::val(description_val).into(),
                    Expr::val(latest.priority).into(),
                    Expr::val("draft").into(),
                    Expr::val(100i64).into(),
                    Expr::val(latest.condition_json.as_str()).into(),
                    Expr::val(latest.tasks_json.as_str()).into(),
                    Expr::val(latest.tags.as_str()).into(),
                    Expr::val(latest.continue_on_error).into(),
                ])
                .build_sqlx(query_builder());

            sqlx::query_with(&sql, values).execute(&self.pool).await?;

            self.get_version(workflow_id, new_version).await
        })
        .await
    }

    async fn bulk_create(
        &self,
        workflows: &[CreateWorkflowRequest],
    ) -> Result<Vec<Result<Workflow, OrionError>>, OrionError> {
        let mut results = Vec::with_capacity(workflows.len());

        for req in workflows {
            let result = self.create(req).await;
            results.push(result);
        }

        Ok(results)
    }

    async fn list_versions(
        &self,
        workflow_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<PaginatedResult<Workflow>, OrionError> {
        let limit = limit.clamp(1, 1000);
        let offset = offset.max(0);

        // Count
        let (sql, values) = Query::select()
            .expr(Func::count(Expr::col(Asterisk)))
            .from(Workflows::Table)
            .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
            .build_sqlx(query_builder());
        let (total,): (i64,) = sqlx::query_as_with::<_, (i64,), _>(&sql, values)
            .fetch_one(&self.pool)
            .await?;

        // Data
        let (sql, values) = Query::select()
            .column(Asterisk)
            .from(Workflows::Table)
            .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
            .order_by(Workflows::Version, Order::Desc)
            .limit(limit as u64)
            .offset(offset as u64)
            .build_sqlx(query_builder());
        let data = sqlx::query_as_with::<_, Workflow, _>(&sql, values)
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
        let (sql, values) = Query::select()
            .expr(Expr::val(1i32))
            .build_sqlx(query_builder());
        sqlx::query_scalar_with::<_, i32, _>(&sql, values)
            .fetch_one(&self.pool)
            .await?;
        Ok(())
    }
}

/// Convert a Workflow DB model to a dataflow-rs Workflow via JSON deserialization.
/// The `channel_name` parameter is supplied externally (from the Channel entity).
pub fn workflow_to_dataflow(
    workflow: &Workflow,
    channel_name: &str,
) -> Result<DataflowWorkflow, OrionError> {
    let tasks: serde_json::Value = serde_json::from_str(&workflow.tasks_json)?;
    let condition: serde_json::Value = serde_json::from_str(&workflow.condition_json)?;
    let tags: Vec<String> = serde_json::from_str(&workflow.tags)?;

    let workflow_json = serde_json::json!({
        "id": workflow.workflow_id,
        "name": workflow.name,
        "description": workflow.description,
        "channel": channel_name,
        "priority": workflow.priority,
        "version": workflow.version,
        "status": "active",
        "condition": condition,
        "tasks": tasks,
        "tags": tags,
        "continue_on_error": workflow.continue_on_error,
    });

    let df_workflow: DataflowWorkflow = serde_json::from_value(workflow_json)?;
    Ok(df_workflow)
}

/// Convert a Workflow to a dataflow-rs Workflow with rollout-aware condition wrapping and unique ID.
/// The `channel_name` parameter is supplied externally (from the Channel entity).
pub fn workflow_to_dataflow_with_rollout(
    workflow: &Workflow,
    channel_name: &str,
    bucket_min: i64,
    bucket_max: i64,
) -> Result<DataflowWorkflow, OrionError> {
    let tasks: serde_json::Value = serde_json::from_str(&workflow.tasks_json)?;
    let condition: serde_json::Value = serde_json::from_str(&workflow.condition_json)?;
    let tags: Vec<String> = serde_json::from_str(&workflow.tags)?;

    // Wrap condition with bucket range check
    let wrapped_condition = serde_json::json!({
        "and": [
            condition,
            {">=": [{"var": "_rollout_bucket"}, bucket_min]},
            {"<": [{"var": "_rollout_bucket"}, bucket_max]}
        ]
    });

    let workflow_json = serde_json::json!({
        "id": format!("{}:v{}", workflow.workflow_id, workflow.version),
        "name": workflow.name,
        "description": workflow.description,
        "channel": channel_name,
        "priority": workflow.priority,
        "version": workflow.version,
        "status": workflow.status,
        "condition": wrapped_condition,
        "tasks": tasks,
        "tags": tags,
        "continue_on_error": workflow.continue_on_error,
    });

    let df_workflow: DataflowWorkflow = serde_json::from_value(workflow_json)?;
    Ok(df_workflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workflow_to_dataflow_basic() {
        let workflow = Workflow {
            workflow_id: "test-workflow".to_string(),
            name: "Test Workflow".to_string(),
            description: Some("A test workflow".to_string()),
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

        let df_workflow = workflow_to_dataflow(&workflow, "default").unwrap();
        assert_eq!(df_workflow.id, "test-workflow");
        assert_eq!(df_workflow.name, "Test Workflow");
        assert_eq!(df_workflow.channel, "default");
        assert_eq!(df_workflow.priority, 10);
    }

    #[test]
    fn test_workflow_to_dataflow_custom_channel() {
        let workflow = Workflow {
            workflow_id: "wf-orders".to_string(),
            name: "Order Workflow".to_string(),
            description: None,
            priority: 5,
            version: 2,
            status: "active".to_string(),
            rollout_percentage: 100,
            condition_json: r#"{"==": [{"var": "type"}, "order"]}"#.to_string(),
            tasks_json: r#"[{"id":"t1","name":"Process","function":{"name":"log","input":{}}}]"#
                .to_string(),
            tags: r#"["orders"]"#.to_string(),
            continue_on_error: true,
            created_at: chrono::NaiveDateTime::default(),
            updated_at: chrono::NaiveDateTime::default(),
        };

        let df_workflow = workflow_to_dataflow(&workflow, "orders").unwrap();
        assert_eq!(df_workflow.channel, "orders");
        assert_eq!(df_workflow.id, "wf-orders");
    }

    #[test]
    fn test_workflow_to_dataflow_with_rollout_wraps_condition() {
        let workflow = Workflow {
            workflow_id: "rollout-wf".to_string(),
            name: "Rollout Test".to_string(),
            description: None,
            priority: 1,
            version: 3,
            status: "active".to_string(),
            rollout_percentage: 50,
            condition_json: "true".to_string(),
            tasks_json: r#"[{"id":"t1","name":"Noop","function":{"name":"log","input":{}}}]"#
                .to_string(),
            tags: "[]".to_string(),
            continue_on_error: false,
            created_at: chrono::NaiveDateTime::default(),
            updated_at: chrono::NaiveDateTime::default(),
        };

        let df_workflow = workflow_to_dataflow_with_rollout(&workflow, "default", 0, 50).unwrap();
        assert_eq!(df_workflow.id, "rollout-wf:v3");
        assert_eq!(df_workflow.channel, "default");

        // Verify the condition was wrapped
        let cond = &df_workflow.condition;
        assert!(
            cond.get("and").is_some(),
            "condition should be wrapped in 'and'"
        );
        let and_arr = cond.get("and").unwrap().as_array().unwrap();
        assert_eq!(and_arr.len(), 3);
    }

    #[test]
    fn test_build_condition_empty_filter() {
        let filter = WorkflowFilter::default();
        let cond = build_condition(&filter);
        let (sql, _) = Query::select()
            .column(Asterisk)
            .from(CurrentWorkflows::Table)
            .cond_where(cond)
            .build(sea_query::SqliteQueryBuilder);
        // With no filters, there should be no WHERE clause (Condition::all() with no adds is empty)
        assert!(!sql.contains("WHERE") || sql.contains("WHERE TRUE") || sql.contains("WHERE 1"));
    }

    #[test]
    fn test_build_condition_status_filter() {
        let filter = WorkflowFilter {
            status: Some("active".to_string()),
            ..Default::default()
        };
        let cond = build_condition(&filter);
        let (sql, _) = Query::select()
            .column(Asterisk)
            .from(CurrentWorkflows::Table)
            .cond_where(cond)
            .build(sea_query::SqliteQueryBuilder);
        assert!(sql.contains("\"status\""));
    }

    #[test]
    fn test_build_condition_tag_filter() {
        let filter = WorkflowFilter {
            tag: Some("billing".to_string()),
            ..Default::default()
        };
        let cond = build_condition(&filter);
        let (sql, _) = Query::select()
            .column(Asterisk)
            .from(CurrentWorkflows::Table)
            .cond_where(cond)
            .build(sea_query::SqliteQueryBuilder);
        assert!(sql.contains("LIKE"));
    }

    #[test]
    fn test_build_condition_tag_escaping() {
        let filter = WorkflowFilter {
            tag: Some("100%_done".to_string()),
            ..Default::default()
        };
        let cond = build_condition(&filter);
        let (sql, _) = Query::select()
            .column(Asterisk)
            .from(CurrentWorkflows::Table)
            .cond_where(cond)
            .build(sea_query::SqliteQueryBuilder);
        assert!(sql.contains("LIKE"));
    }

    #[test]
    fn test_build_condition_combined_filters() {
        let filter = WorkflowFilter {
            status: Some("draft".to_string()),
            tag: Some("test".to_string()),
            limit: Some(10),
            offset: Some(0),
            ..Default::default()
        };
        let cond = build_condition(&filter);
        let (sql, _) = Query::select()
            .column(Asterisk)
            .from(CurrentWorkflows::Table)
            .cond_where(cond)
            .build(sea_query::SqliteQueryBuilder);
        assert!(sql.contains("\"status\""));
        assert!(sql.contains("LIKE"));
    }

    #[test]
    fn test_create_workflow_request_defaults() {
        let json = r#"{"name": "Test", "tasks": []}"#;
        let req: CreateWorkflowRequest = serde_json::from_str(json).unwrap();
        assert!(req.workflow_id.is_none());
        assert_eq!(req.name, "Test");
        assert_eq!(req.priority, 0);
        assert_eq!(req.condition, serde_json::Value::Bool(true));
        assert!(req.tags.is_empty());
        assert!(!req.continue_on_error);
    }

    #[test]
    fn test_update_workflow_request_all_none() {
        let json = r#"{}"#;
        let req: UpdateWorkflowRequest = serde_json::from_str(json).unwrap();
        assert!(req.name.is_none());
        assert!(req.description.is_none());
        assert!(req.priority.is_none());
        assert!(req.condition.is_none());
        assert!(req.tasks.is_none());
        assert!(req.tags.is_none());
        assert!(req.continue_on_error.is_none());
    }

    #[test]
    fn test_status_change_request_parse() {
        let json = r#"{"status": "active", "rollout_percentage": 50}"#;
        let req: StatusChangeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.status, "active");
        assert_eq!(req.rollout_percentage, Some(50));
    }

    #[test]
    fn test_status_change_request_no_rollout() {
        let json = r#"{"status": "archived"}"#;
        let req: StatusChangeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.status, "archived");
        assert!(req.rollout_percentage.is_none());
    }

    #[test]
    fn test_rollout_update_request_parse() {
        let json = r#"{"rollout_percentage": 75}"#;
        let req: RolloutUpdateRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.rollout_percentage, 75);
    }

    #[test]
    fn test_workflow_filter_defaults() {
        let filter = WorkflowFilter::default();
        assert!(filter.status.is_none());
        assert!(filter.tag.is_none());
        assert!(filter.limit.is_none());
        assert!(filter.offset.is_none());
    }

    #[test]
    fn test_paginated_result_serialization() {
        let result = PaginatedResult {
            data: vec!["a".to_string(), "b".to_string()],
            total: 10,
            limit: 2,
            offset: 0,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["total"], 10);
        assert_eq!(json["limit"], 2);
        assert_eq!(json["offset"], 0);
        assert_eq!(json["data"].as_array().unwrap().len(), 2);
    }
}
