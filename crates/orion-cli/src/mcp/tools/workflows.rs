use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::client::OrionClient;
use crate::utils;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowsListParams {
    #[schemars(description = "Filter by workflow status: draft, active, or archived")]
    pub status: Option<String>,
    #[schemars(description = "Filter by tag")]
    pub tag: Option<String>,
    #[schemars(description = "Maximum number of workflows to return")]
    pub limit: Option<i64>,
    #[schemars(description = "Number of workflows to skip for pagination")]
    pub offset: Option<i64>,
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
}

pub async fn list(client: &OrionClient, params: WorkflowsListParams) -> Result<String, String> {
    let qs = utils::build_query_string(&[
        ("status", params.status),
        ("tag", params.tag),
        ("limit", params.limit.map(|l| l.to_string())),
        ("offset", params.offset.map(|o| o.to_string())),
    ]);
    let resp: Value = client
        .get(&format!("/api/v1/admin/workflows{qs}"))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn get(client: &OrionClient, params: WorkflowsGetParams) -> Result<String, String> {
    let resp: Value = client
        .get(&format!("/api/v1/admin/workflows/{}", params.id))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn create(client: &OrionClient, params: WorkflowsCreateParams) -> Result<String, String> {
    let body: Value = serde_json::from_str(&params.workflow_json)
        .map_err(|e| format!("Invalid workflow JSON: {e}"))?;
    let resp: Value = client
        .post("/api/v1/admin/workflows", &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn update(client: &OrionClient, params: WorkflowsUpdateParams) -> Result<String, String> {
    let body: Value = serde_json::from_str(&params.workflow_json)
        .map_err(|e| format!("Invalid workflow JSON: {e}"))?;
    let resp: Value = client
        .put(&format!("/api/v1/admin/workflows/{}", params.id), &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn delete(client: &OrionClient, params: WorkflowsDeleteParams) -> Result<String, String> {
    client
        .delete_request(&format!("/api/v1/admin/workflows/{}", params.id))
        .await
        .map_err(|e| e.to_string())?;
    Ok(format!("Workflow {} deleted successfully", params.id))
}

pub async fn activate(
    client: &OrionClient,
    params: WorkflowsStatusParams,
) -> Result<String, String> {
    change_status(client, &params.id, "active").await
}

pub async fn archive(
    client: &OrionClient,
    params: WorkflowsStatusParams,
) -> Result<String, String> {
    change_status(client, &params.id, "archived").await
}

async fn change_status(client: &OrionClient, id: &str, status: &str) -> Result<String, String> {
    let body = serde_json::json!({ "status": status });
    let resp: Value = client
        .patch(&format!("/api/v1/admin/workflows/{id}/status"), &body)
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
        .post(
            &format!("/api/v1/admin/workflows/{}/test", params.id),
            &body,
        )
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
        .get(&format!("/api/v1/admin/workflows/export{qs}"))
        .await
        .map_err(|e| e.to_string())?;
    let data = resp.get("data").unwrap_or(&resp);
    serde_json::to_string_pretty(data).map_err(|e| e.to_string())
}

pub async fn import(client: &OrionClient, params: WorkflowsImportParams) -> Result<String, String> {
    super::import_resource(
        client,
        "/api/v1/admin/workflows/import",
        "workflow",
        &params.workflows_json,
        params.dry_run.unwrap_or(false),
    )
    .await
}

pub async fn validate(
    client: &OrionClient,
    params: WorkflowsValidateParams,
) -> Result<String, String> {
    let body: Value = serde_json::from_str(&params.workflow_json)
        .map_err(|e| format!("Invalid workflow JSON: {e}"))?;
    let resp: Value = client
        .post("/api/v1/admin/workflows/validate", &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn rollout(
    client: &OrionClient,
    params: WorkflowsRolloutParams,
) -> Result<String, String> {
    let body = serde_json::json!({ "rollout_percentage": params.rollout_percentage });
    let resp: Value = client
        .patch(
            &format!("/api/v1/admin/workflows/{}/rollout", params.id),
            &body,
        )
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn versions(
    client: &OrionClient,
    params: WorkflowsVersionsParams,
) -> Result<String, String> {
    let resp: Value = client
        .get(&format!("/api/v1/admin/workflows/{}/versions", params.id))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn create_version(
    client: &OrionClient,
    params: WorkflowsVersionsParams,
) -> Result<String, String> {
    let resp: Value = client
        .post_empty(&format!("/api/v1/admin/workflows/{}/versions", params.id))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}
