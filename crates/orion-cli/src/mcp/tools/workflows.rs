use orion_api::{STATUS_ACTIVE, STATUS_ARCHIVED};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::client::OrionClient;
use crate::utils;
use orion_client::paths;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowsListParams {
    #[schemars(description = "Filter by workflow status: draft, active, or archived")]
    pub status: Option<String>,
    #[schemars(description = "Filter by tag")]
    pub tag: Option<String>,
    #[schemars(description = "Maximum number of workflows to return (default 50, max 1000)")]
    pub limit: Option<i64>,
    #[schemars(description = "Number of workflows to skip for pagination")]
    pub offset: Option<i64>,
    #[schemars(
        description = "Sort by column: priority (default), name, status, created_at, updated_at"
    )]
    pub sort_by: Option<String>,
    #[schemars(description = "Sort direction: asc or desc")]
    pub sort_order: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowsGetParams {
    #[schemars(description = "The workflow ID to retrieve")]
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowsCreateParams {
    #[schemars(description = include_str!("descriptions/param_workflow_json.md"))]
    pub workflow_json: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowsUpdateParams {
    #[schemars(description = "The workflow ID to update")]
    pub id: String,
    #[schemars(description = include_str!("descriptions/param_workflow_json.md"))]
    pub workflow_json: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowsDeleteParams {
    #[schemars(description = "The workflow ID to delete")]
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowsStatusParams {
    #[schemars(description = "The workflow ID to change status for")]
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowsTestParams {
    #[schemars(description = "The workflow ID to test")]
    pub id: String,
    #[schemars(
        description = "JSON string of the test data payload. Provide the raw business data that would arrive on the channel, e.g. {\"id\": \"order-123\", \"amount\": 250.00}"
    )]
    pub data: String,
    #[schemars(description = "Optional JSON string of metadata")]
    pub metadata: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowsExportParams {
    #[schemars(description = "Filter exported workflows by status")]
    pub status: Option<String>,
    #[schemars(description = "Filter exported workflows by tag")]
    pub tag: Option<String>,
    #[schemars(description = "Maximum number of workflows to export")]
    pub limit: Option<i64>,
    #[schemars(description = "Number of workflows to skip for pagination")]
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowsValidateParams {
    #[schemars(description = include_str!("descriptions/param_workflow_json.md"))]
    pub workflow_json: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowsRolloutParams {
    #[schemars(description = "The workflow ID to update rollout for")]
    pub id: String,
    #[schemars(
        description = "Rollout percentage (0-100). Controls what percentage of matching data is processed by this workflow."
    )]
    pub rollout_percentage: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowsVersionsParams {
    #[schemars(description = "The workflow ID to list versions for")]
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowsImportParams {
    #[schemars(
        description = "JSON string containing an array of workflow definitions to import. Each element must be a complete workflow object (see workflows_create for format)."
    )]
    pub workflows_json: String,
    #[schemars(description = "If true, preview what would be imported without actually importing")]
    pub dry_run: Option<bool>,
    #[schemars(
        description = "What an already-stored conflict means: fail (default, the item is refused), skip, or new_version (update the draft in place, or cut a new draft version over an active workflow)"
    )]
    pub on_conflict: Option<String>,
}

pub async fn list(client: &OrionClient, params: WorkflowsListParams) -> Result<String, String> {
    let qs = utils::build_query_string(&[
        ("status", params.status),
        ("tag", params.tag),
        ("limit", params.limit.map(|l| l.to_string())),
        ("offset", params.offset.map(|o| o.to_string())),
        ("sort_by", params.sort_by),
        ("sort_order", params.sort_order),
    ]);
    let resp: Value = client
        .get(&format!("{}{qs}", paths::WORKFLOWS))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn get(client: &OrionClient, params: WorkflowsGetParams) -> Result<String, String> {
    let resp: Value = client
        .get(&paths::workflow(&params.id))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn create(client: &OrionClient, params: WorkflowsCreateParams) -> Result<String, String> {
    let body: Value = serde_json::from_str(&params.workflow_json)
        .map_err(|e| format!("Invalid workflow JSON: {e}"))?;
    let resp: Value = client
        .post(paths::WORKFLOWS, &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn update(client: &OrionClient, params: WorkflowsUpdateParams) -> Result<String, String> {
    let body: Value = serde_json::from_str(&params.workflow_json)
        .map_err(|e| format!("Invalid workflow JSON: {e}"))?;
    let resp: Value = client
        .put(&paths::workflow(&params.id), &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn delete(client: &OrionClient, params: WorkflowsDeleteParams) -> Result<String, String> {
    client
        .delete_request(&paths::workflow(&params.id))
        .await
        .map_err(|e| e.to_string())?;
    Ok(format!("Workflow {} deleted successfully", params.id))
}

pub async fn activate(
    client: &OrionClient,
    params: WorkflowsStatusParams,
) -> Result<String, String> {
    change_status(client, &params.id, STATUS_ACTIVE).await
}

pub async fn archive(
    client: &OrionClient,
    params: WorkflowsStatusParams,
) -> Result<String, String> {
    change_status(client, &params.id, STATUS_ARCHIVED).await
}

async fn change_status(client: &OrionClient, id: &str, status: &str) -> Result<String, String> {
    let body = serde_json::json!({ "status": status });
    let resp: Value = client
        .patch(&paths::workflow_status(id), &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn test(client: &OrionClient, params: WorkflowsTestParams) -> Result<String, String> {
    let data: Value =
        serde_json::from_str(&params.data).map_err(|e| format!("Invalid test data JSON: {e}"))?;

    let mut body = serde_json::json!({ "data": data });
    if let Some(meta_str) = &params.metadata {
        let meta: Value =
            serde_json::from_str(meta_str).map_err(|e| format!("Invalid metadata JSON: {e}"))?;
        body["metadata"] = meta;
    }

    let resp: Value = client
        .post(&paths::workflow_test(&params.id), &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn export(client: &OrionClient, params: WorkflowsExportParams) -> Result<String, String> {
    let qs = utils::build_query_string(&[
        ("status", params.status),
        ("tag", params.tag),
        ("limit", params.limit.map(|l| l.to_string())),
        ("offset", params.offset.map(|o| o.to_string())),
    ]);
    let resp: Value = client
        .get(&format!("{}{qs}", paths::WORKFLOWS_EXPORT))
        .await
        .map_err(|e| e.to_string())?;
    let data = resp.get("data").unwrap_or(&resp);
    serde_json::to_string_pretty(data).map_err(|e| e.to_string())
}

pub async fn import(client: &OrionClient, params: WorkflowsImportParams) -> Result<String, String> {
    super::import_resource(
        client,
        paths::WORKFLOWS_IMPORT,
        "workflow",
        &params.workflows_json,
        params.dry_run.unwrap_or(false),
        params.on_conflict,
    )
    .await
}

pub async fn validate(
    client: &OrionClient,
    params: WorkflowsValidateParams,
) -> Result<String, String> {
    super::validate_resource(
        client,
        paths::WORKFLOWS_VALIDATE,
        "workflow",
        &params.workflow_json,
    )
    .await
}

pub async fn rollout(
    client: &OrionClient,
    params: WorkflowsRolloutParams,
) -> Result<String, String> {
    let body = serde_json::json!({ "rollout_percentage": params.rollout_percentage });
    let resp: Value = client
        .patch(&paths::workflow_rollout(&params.id), &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn versions(
    client: &OrionClient,
    params: WorkflowsVersionsParams,
) -> Result<String, String> {
    let resp: Value = client
        .get(&paths::workflow_versions(&params.id))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn create_version(
    client: &OrionClient,
    params: WorkflowsVersionsParams,
) -> Result<String, String> {
    let resp: Value = client
        .post_empty(&paths::workflow_versions(&params.id))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn dependencies(
    client: &OrionClient,
    params: WorkflowsGetParams,
) -> Result<String, String> {
    let resp: Value = client
        .get(&paths::workflow_dependencies(&params.id))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}
