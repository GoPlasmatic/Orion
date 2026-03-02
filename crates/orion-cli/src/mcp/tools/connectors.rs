use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::client::OrionClient;

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

pub async fn list(client: &OrionClient, params: ConnectorsListParams) -> Result<String, String> {
    let mut query = Vec::new();
    if let Some(l) = params.limit {
        query.push(format!("limit={l}"));
    }
    if let Some(o) = params.offset {
        query.push(format!("offset={o}"));
    }
    let qs = if query.is_empty() {
        String::new()
    } else {
        format!("?{}", query.join("&"))
    };
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
