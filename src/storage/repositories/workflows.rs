use crate::storage::DbPool;
use async_trait::async_trait;
use dataflow_rs::Workflow as DataflowWorkflow;
use sea_query::{Asterisk, Condition, Expr, Order, Query};
use serde::{Deserialize, Serialize};

use crate::errors::OrionError;
use crate::storage::models::{EntityStatus, Workflow};
use crate::storage::{
    build_sqlx,
    schema::{CurrentWorkflows, Workflows},
};

use super::helpers::{
    clamp_pagination, count_where, ensure_absent, fetch_required, fetch_required_tx,
    optional_string_value, parse_sort_order,
};

#[derive(Debug, Serialize)]
pub struct PaginatedResult<T> {
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
    pub status: EntityStatus,
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

/// Fields needed to materialise one row of the `workflows` table — used by
/// `build_workflow_insert` to avoid an 11-argument positional signature
/// across `create`, `create_new_version`, and `bulk_create`.
struct WorkflowInsertRow<'a> {
    workflow_id: &'a str,
    version: i64,
    name: &'a str,
    description: sea_query::Value,
    priority: i64,
    status: &'a str,
    rollout_pct: i64,
    condition_json: &'a str,
    tasks_json: &'a str,
    tags_json: &'a str,
    continue_on_error: bool,
}

/// Build the INSERT query for a workflow row.
fn build_workflow_insert(row: WorkflowInsertRow<'_>) -> (String, sea_query_binder::SqlxValues) {
    let mut q = Query::insert();
    q.into_table(Workflows::Table)
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
            Expr::val(row.workflow_id).into(),
            Expr::val(row.version).into(),
            Expr::val(row.name).into(),
            Expr::val(row.description).into(),
            Expr::val(row.priority).into(),
            Expr::val(row.status).into(),
            Expr::val(row.rollout_pct).into(),
            Expr::val(row.condition_json).into(),
            Expr::val(row.tasks_json).into(),
            Expr::val(row.tags_json).into(),
            Expr::val(row.continue_on_error).into(),
        ]);
    build_sqlx(&mut q)
}

/// Activate the draft version at 100% rollout: archive any existing active
/// versions, then promote the draft.
async fn activate_full_rollout(
    tx: &mut crate::storage::DbTransaction,
    workflow_id: &str,
    draft_version: i64,
    active_versions: &[Workflow],
) -> Result<(), OrionError> {
    if !active_versions.is_empty() {
        let (sql, values) = archive_active_workflows_query(workflow_id, None);
        tx.execute_query(&sql, values).await?;
    }
    let (sql, values) = activate_workflow_version_query(workflow_id, draft_version, 100);
    tx.execute_query(&sql, values).await?;
    Ok(())
}

/// Activate the draft version at a partial rollout: keep the primary active
/// (highest version) at `100 - rollout_pct`, archive everything else, and
/// promote the draft to `rollout_pct`.
async fn activate_partial_rollout(
    tx: &mut crate::storage::DbTransaction,
    workflow_id: &str,
    draft_version: i64,
    active_versions: &[Workflow],
    rollout_pct: i64,
) -> Result<(), OrionError> {
    if let Some(primary_active) = active_versions.first() {
        if active_versions.len() > 1 {
            let (sql, values) =
                archive_active_workflows_query(workflow_id, Some(primary_active.version));
            tx.execute_query(&sql, values).await?;
        }
        let (sql, values) =
            set_workflow_rollout_query(workflow_id, primary_active.version, 100 - rollout_pct);
        tx.execute_query(&sql, values).await?;
    }
    let (sql, values) = activate_workflow_version_query(workflow_id, draft_version, rollout_pct);
    tx.execute_query(&sql, values).await?;
    Ok(())
}

/// Build the UPDATE query that sets `Status = Archived` for one specific
/// version of a workflow.
fn set_workflow_archived_query(
    workflow_id: &str,
    version: i64,
) -> (String, sea_query_binder::SqlxValues) {
    let mut q = Query::update();
    q.table(Workflows::Table)
        .value(Workflows::Status, EntityStatus::Archived.as_str())
        .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
        .and_where(Expr::col(Workflows::Version).eq(version));
    build_sqlx(&mut q)
}

/// Build the UPDATE query that sets `RolloutPercentage` for one specific
/// version of a workflow.
fn set_workflow_rollout_query(
    workflow_id: &str,
    version: i64,
    pct: i64,
) -> (String, sea_query_binder::SqlxValues) {
    let mut q = Query::update();
    q.table(Workflows::Table)
        .value(Workflows::RolloutPercentage, pct)
        .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
        .and_where(Expr::col(Workflows::Version).eq(version));
    build_sqlx(&mut q)
}

/// Build the UPDATE query that promotes a draft version to `Active` with
/// the given rollout percentage.
fn activate_workflow_version_query(
    workflow_id: &str,
    version: i64,
    pct: i64,
) -> (String, sea_query_binder::SqlxValues) {
    let mut q = Query::update();
    q.table(Workflows::Table)
        .value(Workflows::Status, EntityStatus::Active.as_str())
        .value(Workflows::RolloutPercentage, pct)
        .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
        .and_where(Expr::col(Workflows::Version).eq(version));
    build_sqlx(&mut q)
}

/// Build the UPDATE query that archives all active versions of a workflow.
/// `exclude_version`, when set, leaves that specific version untouched
/// (used by partial-rollout activation to preserve the primary active row).
fn archive_active_workflows_query(
    workflow_id: &str,
    exclude_version: Option<i64>,
) -> (String, sea_query_binder::SqlxValues) {
    let mut q = Query::update();
    q.table(Workflows::Table)
        .value(Workflows::Status, EntityStatus::Archived.as_str())
        .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
        .and_where(Expr::col(Workflows::Status).eq(EntityStatus::Active.as_str()));
    if let Some(v) = exclude_version {
        q.and_where(Expr::col(Workflows::Version).ne(v));
    }
    build_sqlx(&mut q)
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
        cond = cond.add(Expr::col(Workflows::Tags).like(format!("%\"{escaped}\"%")));
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

            let (sql, values) = build_workflow_insert(WorkflowInsertRow {
                workflow_id: workflow_id.as_str(),
                version: 1,
                name: req.name.as_str(),
                description: description_val,
                priority: req.priority,
                status: EntityStatus::Draft.as_str(),
                rollout_pct: 100,
                condition_json: condition_json.as_str(),
                tasks_json: tasks_json.as_str(),
                tags_json: tags_json.as_str(),
                continue_on_error: req.continue_on_error,
            });

            self.pool.execute_query(&sql, values).await?;

            self.get_version(&workflow_id, 1).await
        })
        .await
    }

    async fn get_by_id(&self, workflow_id: &str) -> Result<Workflow, OrionError> {
        crate::metrics::timed_db_op("workflows.get_by_id", async {
            let (sql, values) = build_sqlx(
                Query::select()
                    .column(Asterisk)
                    .from(Workflows::Table)
                    .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                    .order_by(Workflows::Version, Order::Desc)
                    .limit(1),
            );

            self.pool
                .fetch_optional_as::<Workflow>(&sql, values)
                .await?
                .ok_or_else(|| {
                    OrionError::NotFound(format!("Workflow '{workflow_id}' not found"))
                })
        })
        .await
    }

    async fn get_version(&self, workflow_id: &str, version: i64) -> Result<Workflow, OrionError> {
        let (sql, values) = build_sqlx(
            Query::select()
                .column(Asterisk)
                .from(Workflows::Table)
                .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                .and_where(Expr::col(Workflows::Version).eq(version)),
        );

        self.pool
            .fetch_optional_as::<Workflow>(&sql, values)
            .await?
            .ok_or_else(|| {
                OrionError::NotFound(format!(
                    "Workflow '{workflow_id}' version {version} not found"
                ))
            })
    }

    async fn list(&self, filter: &WorkflowFilter) -> Result<Vec<Workflow>, OrionError> {
        let cond = build_condition(filter);
        let (sql, values) = build_sqlx(
            Query::select()
                .column(Asterisk)
                .from(CurrentWorkflows::Table)
                .cond_where(cond)
                .order_by(Workflows::Priority, Order::Desc)
                .order_by(Workflows::Name, Order::Asc),
        );

        Ok(self.pool.fetch_all_as::<Workflow>(&sql, values).await?)
    }

    async fn list_paginated(
        &self,
        filter: &WorkflowFilter,
    ) -> Result<PaginatedResult<Workflow>, OrionError> {
        crate::metrics::timed_db_op("workflows.list_paginated", async {
            let cond = build_condition(filter);
            let (limit, offset) = clamp_pagination(filter.limit, filter.offset);

            let total = count_where(&self.pool, CurrentWorkflows::Table, cond.clone()).await?;

            // Sort column mapping
            let sort_iden = match filter.sort_by.as_deref() {
                Some("name") => Workflows::Name,
                Some("status") => Workflows::Status,
                Some("created_at") => Workflows::CreatedAt,
                Some("updated_at") => Workflows::UpdatedAt,
                _ => Workflows::Priority,
            };
            let order = parse_sort_order(filter.sort_order.as_deref());

            // Data
            let (sql, values) = build_sqlx(
                Query::select()
                    .column(Asterisk)
                    .from(CurrentWorkflows::Table)
                    .cond_where(cond)
                    .order_by(sort_iden, order)
                    .limit(limit as u64)
                    .offset(offset as u64),
            );
            let data = self.pool.fetch_all_as::<Workflow>(&sql, values).await?;

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
            let (sql, values) = build_sqlx(
                Query::select()
                    .column(Asterisk)
                    .from(Workflows::Table)
                    .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                    .and_where(Expr::col(Workflows::Status).eq(EntityStatus::Draft.as_str())),
            );

            let existing: Workflow = fetch_required(&self.pool, &sql, values, || {
                OrionError::BadRequest(format!(
                    "No draft version found for workflow '{workflow_id}'"
                ))
            })
            .await?;

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

            let (sql, values) = build_sqlx(
                Query::update()
                    .table(Workflows::Table)
                    .value(Workflows::Name, name)
                    .value(Workflows::Description, description_val)
                    .value(Workflows::Priority, priority)
                    .value(Workflows::ConditionJson, condition_json.as_str())
                    .value(Workflows::TasksJson, tasks_json.as_str())
                    .value(Workflows::Tags, tags_json.as_str())
                    .value(Workflows::ContinueOnError, continue_on_error)
                    .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                    .and_where(Expr::col(Workflows::Status).eq(EntityStatus::Draft.as_str())),
            );

            self.pool.execute_query(&sql, values).await?;

            self.get_version(workflow_id, existing.version).await
        })
        .await
    }

    async fn delete(&self, workflow_id: &str) -> Result<(), OrionError> {
        crate::metrics::timed_db_op("workflows.delete", async {
            let (sql, values) = build_sqlx(
                Query::delete()
                    .from_table(Workflows::Table)
                    .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id)),
            );

            let rows_affected = self.pool.execute_query(&sql, values).await?;

            if rows_affected == 0 {
                return Err(OrionError::NotFound(format!(
                    "Workflow '{workflow_id}' not found"
                )));
            }

            Ok(())
        })
        .await
    }

    async fn list_active(&self) -> Result<Vec<Workflow>, OrionError> {
        crate::metrics::timed_db_op("workflows.list_active", async {
            let (sql, values) = build_sqlx(
                Query::select()
                    .column(Asterisk)
                    .from(Workflows::Table)
                    .and_where(Expr::col(Workflows::Status).eq(EntityStatus::Active.as_str()))
                    .order_by(Workflows::Priority, Order::Desc),
            );

            Ok(self.pool.fetch_all_as::<Workflow>(&sql, values).await?)
        })
        .await
    }

    async fn list_active_by_ids(&self, workflow_ids: &[&str]) -> Result<Vec<Workflow>, OrionError> {
        crate::metrics::timed_db_op("workflows.list_active_by_ids", async {
            if workflow_ids.is_empty() {
                return Ok(Vec::new());
            }

            let (sql, values) = build_sqlx(
                Query::select()
                    .column(Asterisk)
                    .from(Workflows::Table)
                    .and_where(Expr::col(Workflows::Status).eq(EntityStatus::Active.as_str()))
                    .and_where(Expr::col(Workflows::WorkflowId).is_in(workflow_ids.iter().copied()))
                    .order_by(Workflows::Priority, Order::Desc),
            );

            Ok(self.pool.fetch_all_as::<Workflow>(&sql, values).await?)
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
            let mut tx = self.pool.begin_tx().await?;

            // Fetch draft version
            let (sql, values) = build_sqlx(
                Query::select()
                    .column(Asterisk)
                    .from(Workflows::Table)
                    .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                    .and_where(Expr::col(Workflows::Status).eq(EntityStatus::Draft.as_str())),
            );

            let draft: Workflow = fetch_required_tx(&mut tx, &sql, values, || {
                OrionError::BadRequest(format!(
                    "No draft version found for workflow '{workflow_id}'"
                ))
            })
            .await?;

            // Fetch active versions
            let (sql, values) = build_sqlx(
                Query::select()
                    .column(Asterisk)
                    .from(Workflows::Table)
                    .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                    .and_where(Expr::col(Workflows::Status).eq(EntityStatus::Active.as_str()))
                    .order_by(Workflows::Version, Order::Desc),
            );

            let active_versions: Vec<Workflow> = tx.fetch_all_as::<Workflow>(&sql, values).await?;

            if rollout_pct == 100 {
                activate_full_rollout(&mut tx, workflow_id, draft.version, &active_versions)
                    .await?;
            } else {
                activate_partial_rollout(
                    &mut tx,
                    workflow_id,
                    draft.version,
                    &active_versions,
                    rollout_pct,
                )
                .await?;
            }

            tx.commit().await?;

            self.get_version(workflow_id, draft.version).await
        })
        .await
    }

    async fn archive(&self, workflow_id: &str) -> Result<Workflow, OrionError> {
        crate::metrics::timed_db_op("workflows.archive", async {
            // Fetch latest active version
            let (sql, values) = build_sqlx(
                Query::select()
                    .column(Asterisk)
                    .from(Workflows::Table)
                    .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                    .and_where(Expr::col(Workflows::Status).eq(EntityStatus::Active.as_str()))
                    .order_by(Workflows::Version, Order::Desc)
                    .limit(1),
            );

            let active: Workflow = fetch_required(&self.pool, &sql, values, || {
                OrionError::BadRequest(format!(
                    "No active version found for workflow '{workflow_id}'"
                ))
            })
            .await?;

            // Archive all active versions
            let (sql, values) = archive_active_workflows_query(workflow_id, None);

            self.pool.execute_query(&sql, values).await?;

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
            let mut tx = self.pool.begin_tx().await?;

            // Get active versions ordered by version DESC (newest first)
            let (sql, values) = build_sqlx(
                Query::select()
                    .column(Asterisk)
                    .from(Workflows::Table)
                    .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                    .and_where(Expr::col(Workflows::Status).eq(EntityStatus::Active.as_str()))
                    .order_by(Workflows::Version, Order::Desc),
            );

            let active_versions: Vec<Workflow> = tx.fetch_all_as::<Workflow>(&sql, values).await?;

            if active_versions.is_empty() {
                return Err(OrionError::BadRequest(format!(
                    "No active versions found for workflow '{workflow_id}'"
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
                // Promote the newer version: archive the older and set newer to 100%.
                let (sql, values) = set_workflow_archived_query(workflow_id, older.version);
                tx.execute_query(&sql, values).await?;
                let (sql, values) = set_workflow_rollout_query(workflow_id, newer.version, 100);
                tx.execute_query(&sql, values).await?;
            } else {
                // Split traffic: newer = pct, older = 100 - pct.
                let (sql, values) = set_workflow_rollout_query(workflow_id, newer.version, pct);
                tx.execute_query(&sql, values).await?;
                let (sql, values) =
                    set_workflow_rollout_query(workflow_id, older.version, 100 - pct);
                tx.execute_query(&sql, values).await?;
            }

            tx.commit().await?;

            self.get_version(workflow_id, newer.version).await
        })
        .await
    }

    async fn create_new_version(&self, workflow_id: &str) -> Result<Workflow, OrionError> {
        crate::metrics::timed_db_op("workflows.create_new_version", async {
            // Check no draft already exists
            let (sql, values) = build_sqlx(
                Query::select()
                    .column(Asterisk)
                    .from(Workflows::Table)
                    .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                    .and_where(Expr::col(Workflows::Status).eq(EntityStatus::Draft.as_str())),
            );

            ensure_absent::<Workflow>(&self.pool, &sql, values, || {
                OrionError::Conflict(format!(
                    "Workflow '{workflow_id}' already has a draft version"
                ))
            })
            .await?;

            // Find the latest version to copy from
            let latest = self.get_by_id(workflow_id).await?;

            let new_version = latest.version + 1;

            let description_val = optional_string_value(latest.description.as_deref());

            let (sql, values) = build_workflow_insert(WorkflowInsertRow {
                workflow_id,
                version: new_version,
                name: latest.name.as_str(),
                description: description_val,
                priority: latest.priority,
                status: EntityStatus::Draft.as_str(),
                rollout_pct: 100,
                condition_json: latest.condition_json.as_str(),
                tasks_json: latest.tasks_json.as_str(),
                tags_json: latest.tags.as_str(),
                continue_on_error: latest.continue_on_error,
            });

            self.pool.execute_query(&sql, values).await?;

            self.get_version(workflow_id, new_version).await
        })
        .await
    }

    async fn bulk_create(
        &self,
        workflows: &[CreateWorkflowRequest],
    ) -> Result<Vec<Result<Workflow, OrionError>>, OrionError> {
        crate::metrics::timed_db_op("workflows.bulk_create", async {
            let mut tx = self.pool.begin_tx().await?;
            let mut results = Vec::with_capacity(workflows.len());

            for req in workflows {
                let result: Result<Workflow, OrionError> = async {
                    let workflow_id = req
                        .workflow_id
                        .clone()
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                    let condition_json = serde_json::to_string(&req.condition)?;
                    let tasks_json = serde_json::to_string(&req.tasks)?;
                    let tags_json = serde_json::to_string(&req.tags)?;
                    let description_val = optional_string_value(req.description.as_deref());

                    let (sql, values) = build_workflow_insert(WorkflowInsertRow {
                        workflow_id: workflow_id.as_str(),
                        version: 1,
                        name: req.name.as_str(),
                        description: description_val,
                        priority: req.priority,
                        status: EntityStatus::Draft.as_str(),
                        rollout_pct: 100,
                        condition_json: condition_json.as_str(),
                        tasks_json: tasks_json.as_str(),
                        tags_json: tags_json.as_str(),
                        continue_on_error: req.continue_on_error,
                    });

                    tx.execute_query(&sql, values).await?;

                    let (sql, values) = build_sqlx(
                        Query::select()
                            .column(Asterisk)
                            .from(Workflows::Table)
                            .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id.as_str()))
                            .and_where(Expr::col(Workflows::Version).eq(1i64)),
                    );

                    tx.fetch_optional_as::<Workflow>(&sql, values)
                        .await?
                        .ok_or_else(|| {
                            OrionError::NotFound(format!(
                                "Workflow '{workflow_id}' version 1 not found after insert"
                            ))
                        })
                }
                .await;
                results.push(result);
            }

            tx.commit().await?;
            Ok(results)
        })
        .await
    }

    async fn list_versions(
        &self,
        workflow_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<PaginatedResult<Workflow>, OrionError> {
        let limit = limit.clamp(1, 1000);
        let offset = offset.max(0);

        let total = count_where(
            &self.pool,
            Workflows::Table,
            Condition::all().add(Expr::col(Workflows::WorkflowId).eq(workflow_id)),
        )
        .await?;

        // Data
        let (sql, values) = build_sqlx(
            Query::select()
                .column(Asterisk)
                .from(Workflows::Table)
                .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id))
                .order_by(Workflows::Version, Order::Desc)
                .limit(limit as u64)
                .offset(offset as u64),
        );
        let data = self.pool.fetch_all_as::<Workflow>(&sql, values).await?;

        Ok(PaginatedResult {
            data,
            total,
            limit,
            offset,
        })
    }

    async fn ping(&self) -> Result<(), OrionError> {
        let (sql, values) = build_sqlx(Query::select().expr(Expr::val(1i32)));
        self.pool.fetch_scalar::<i32>(&sql, values).await?;
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
        "status": EntityStatus::Active.as_str(),
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
#[allow(clippy::unwrap_used)]
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
            status: EntityStatus::Active.as_str().to_string(),
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
            status: EntityStatus::Active.as_str().to_string(),
            rollout_percentage: 100,
            condition_json: r#"{"==": [{"var": "type"}, "order"]}"#.to_string(),
            tasks_json: r#"[{"id":"t1","name":"Process","function":{"name":"log","input":{"message":"test"}}}]"#
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
            status: EntityStatus::Active.as_str().to_string(),
            rollout_percentage: 50,
            condition_json: "true".to_string(),
            tasks_json: r#"[{"id":"t1","name":"Noop","function":{"name":"log","input":{"message":"test"}}}]"#
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
            status: Some(EntityStatus::Active.as_str().to_string()),
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
            status: Some(EntityStatus::Draft.as_str().to_string()),
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
        assert_eq!(req.status, EntityStatus::Active);
        assert_eq!(req.rollout_percentage, Some(50));
    }

    #[test]
    fn test_status_change_request_no_rollout() {
        let json = r#"{"status": "archived"}"#;
        let req: StatusChangeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.status, EntityStatus::Archived);
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
