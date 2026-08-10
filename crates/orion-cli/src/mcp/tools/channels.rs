use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::client::OrionClient;
use crate::utils;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChannelsListParams {
    #[schemars(description = "Filter by channel status: draft, active, or archived")]
    pub status: Option<String>,
    #[schemars(description = "Filter by channel type: sync or async")]
    pub channel_type: Option<String>,
    #[schemars(description = "Filter by protocol: http, rest, or kafka")]
    pub protocol: Option<String>,
    #[schemars(description = "Maximum number of channels to return")]
    pub limit: Option<i64>,
    #[schemars(description = "Number of channels to skip for pagination")]
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChannelsGetParams {
    #[schemars(description = "The channel ID to retrieve")]
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChannelsCreateParams {
    #[schemars(description = include_str!("descriptions/param_channel_json.md"))]
    pub channel_json: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChannelsUpdateParams {
    #[schemars(description = "The channel ID to update")]
    pub id: String,
    #[schemars(description = include_str!("descriptions/param_channel_json.md"))]
    pub channel_json: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChannelsDeleteParams {
    #[schemars(description = "The channel ID to delete")]
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChannelsStatusParams {
    #[schemars(description = "The channel ID to change status for")]
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChannelsVersionsParams {
    #[schemars(description = "The channel ID to list versions for")]
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChannelsImportParams {
    #[schemars(
        description = "JSON string containing an array of channel definitions to import. Each element must be a complete channel object (see channels_create for format)."
    )]
    pub channels_json: String,
    #[schemars(
        description = "If true, validate on the server without writing any changes (returns would_create/would_fail counts)"
    )]
    pub dry_run: Option<bool>,
}

pub async fn list(client: &OrionClient, params: ChannelsListParams) -> Result<String, String> {
    let qs = utils::build_query_string(&[
        ("status", params.status),
        ("channel_type", params.channel_type),
        ("protocol", params.protocol),
        ("limit", params.limit.map(|l| l.to_string())),
        ("offset", params.offset.map(|o| o.to_string())),
    ]);
    let resp: Value = client
        .get(&format!("/api/v1/admin/channels{qs}"))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn get(client: &OrionClient, params: ChannelsGetParams) -> Result<String, String> {
    let resp: Value = client
        .get(&format!("/api/v1/admin/channels/{}", params.id))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn create(client: &OrionClient, params: ChannelsCreateParams) -> Result<String, String> {
    let body: Value = serde_json::from_str(&params.channel_json)
        .map_err(|e| format!("Invalid channel JSON: {e}"))?;
    let resp: Value = client
        .post("/api/v1/admin/channels", &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn update(client: &OrionClient, params: ChannelsUpdateParams) -> Result<String, String> {
    let body: Value = serde_json::from_str(&params.channel_json)
        .map_err(|e| format!("Invalid channel JSON: {e}"))?;
    let resp: Value = client
        .put(&format!("/api/v1/admin/channels/{}", params.id), &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn delete(client: &OrionClient, params: ChannelsDeleteParams) -> Result<String, String> {
    client
        .delete_request(&format!("/api/v1/admin/channels/{}", params.id))
        .await
        .map_err(|e| e.to_string())?;
    Ok(format!("Channel {} deleted successfully", params.id))
}

pub async fn activate(
    client: &OrionClient,
    params: ChannelsStatusParams,
) -> Result<String, String> {
    change_status(client, &params.id, "active").await
}

pub async fn archive(client: &OrionClient, params: ChannelsStatusParams) -> Result<String, String> {
    change_status(client, &params.id, "archived").await
}

async fn change_status(client: &OrionClient, id: &str, status: &str) -> Result<String, String> {
    let body = serde_json::json!({ "status": status });
    let resp: Value = client
        .patch(&format!("/api/v1/admin/channels/{id}/status"), &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn versions(
    client: &OrionClient,
    params: ChannelsVersionsParams,
) -> Result<String, String> {
    let resp: Value = client
        .get(&format!("/api/v1/admin/channels/{}/versions", params.id))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn create_version(
    client: &OrionClient,
    params: ChannelsVersionsParams,
) -> Result<String, String> {
    let resp: Value = client
        .post_empty(&format!("/api/v1/admin/channels/{}/versions", params.id))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn import(client: &OrionClient, params: ChannelsImportParams) -> Result<String, String> {
    super::import_resource(
        client,
        "/api/v1/admin/channels/import",
        "channel",
        &params.channels_json,
        params.dry_run.unwrap_or(false),
    )
    .await
}
