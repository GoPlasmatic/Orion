use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::client::OrionClient;
use crate::utils;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConnectorsListParams {
    #[schemars(description = "Maximum number of connectors to return")]
    pub limit: Option<i64>,
    #[schemars(description = "Number of connectors to skip for pagination")]
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConnectorsGetParams {
    #[schemars(description = "The connector ID to retrieve")]
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConnectorsCreateParams {
    #[schemars(description = include_str!("descriptions/param_connector_json.md"))]
    pub connector_json: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConnectorsUpdateParams {
    #[schemars(description = "The connector ID to update")]
    pub id: String,
    #[schemars(description = include_str!("descriptions/param_connector_json.md"))]
    pub connector_json: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConnectorsDeleteParams {
    #[schemars(description = "The connector ID to delete")]
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConnectorsToggleParams {
    #[schemars(description = "The connector ID to enable or disable")]
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConnectorsImportParams {
    #[schemars(
        description = "JSON string containing an array of connector definitions to import. Each element must be a complete connector object (see connectors_create for format)."
    )]
    pub connectors_json: String,
    #[schemars(
        description = "If true, validate on the server without writing any changes (returns would_create/would_fail counts)"
    )]
    pub dry_run: Option<bool>,
}

pub async fn list(client: &OrionClient, params: ConnectorsListParams) -> Result<String, String> {
    let qs = utils::build_query_string(&[
        ("limit", params.limit.map(|l| l.to_string())),
        ("offset", params.offset.map(|o| o.to_string())),
    ]);
    let resp: Value = client
        .get(&format!("/api/v1/admin/connectors{qs}"))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn get(client: &OrionClient, params: ConnectorsGetParams) -> Result<String, String> {
    let resp: Value = client
        .get(&format!("/api/v1/admin/connectors/{}", params.id))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn create(
    client: &OrionClient,
    params: ConnectorsCreateParams,
) -> Result<String, String> {
    let body: Value = serde_json::from_str(&params.connector_json)
        .map_err(|e| format!("Invalid connector JSON: {e}"))?;
    let resp: Value = client
        .post("/api/v1/admin/connectors", &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn update(
    client: &OrionClient,
    params: ConnectorsUpdateParams,
) -> Result<String, String> {
    let body: Value = serde_json::from_str(&params.connector_json)
        .map_err(|e| format!("Invalid connector JSON: {e}"))?;
    let resp: Value = client
        .put(&format!("/api/v1/admin/connectors/{}", params.id), &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn delete(
    client: &OrionClient,
    params: ConnectorsDeleteParams,
) -> Result<String, String> {
    client
        .delete_request(&format!("/api/v1/admin/connectors/{}", params.id))
        .await
        .map_err(|e| e.to_string())?;
    Ok(format!("Connector {} deleted successfully", params.id))
}

pub async fn enable(
    client: &OrionClient,
    params: ConnectorsToggleParams,
) -> Result<String, String> {
    let body = serde_json::json!({ "enabled": true });
    let resp: Value = client
        .put(&format!("/api/v1/admin/connectors/{}", params.id), &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn disable(
    client: &OrionClient,
    params: ConnectorsToggleParams,
) -> Result<String, String> {
    let body = serde_json::json!({ "enabled": false });
    let resp: Value = client
        .put(&format!("/api/v1/admin/connectors/{}", params.id), &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn import(
    client: &OrionClient,
    params: ConnectorsImportParams,
) -> Result<String, String> {
    super::import_resource(
        client,
        "/api/v1/admin/connectors/import",
        "connector",
        &params.connectors_json,
        params.dry_run.unwrap_or(false),
    )
    .await
}
