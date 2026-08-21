use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use orion_api::{STATUS_ACTIVE, STATUS_ARCHIVED};

use crate::client::OrionClient;
use crate::utils;
use orion_client::paths;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChannelsListParams {
    #[schemars(description = "Filter by channel status: draft, active, or archived")]
    pub status: Option<String>,
    #[schemars(description = "Filter by channel type: sync or async")]
    pub channel_type: Option<String>,
    #[schemars(description = "Filter by protocol: http, rest, or kafka")]
    pub protocol: Option<String>,
    #[schemars(description = "Filter by tag")]
    pub tag: Option<String>,
    #[schemars(description = "Maximum number of channels to return (default 50, max 1000)")]
    pub limit: Option<i64>,
    #[schemars(description = "Number of channels to skip for pagination")]
    pub offset: Option<i64>,
    #[schemars(
        description = "Sort by column: priority (default), name, status, channel_type, protocol, created_at, updated_at"
    )]
    pub sort_by: Option<String>,
    #[schemars(description = "Sort direction: asc or desc")]
    pub sort_order: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChannelsExportParams {
    #[schemars(description = "Filter by channel status: draft, active, or archived")]
    pub status: Option<String>,
    #[schemars(description = "Filter by tag")]
    pub tag: Option<String>,
    #[schemars(description = "Filter by channel type: sync or async")]
    pub channel_type: Option<String>,
    #[schemars(description = "Filter by protocol: http, rest, or kafka")]
    pub protocol: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChannelsValidateParams {
    #[schemars(description = include_str!("descriptions/param_channel_json.md"))]
    pub channel_json: String,
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
        description = "If true, validate on the server without writing any changes (returns imported/unchanged/skipped/failed counts)"
    )]
    pub dry_run: Option<bool>,
    #[schemars(
        description = "What an already-stored conflict means: fail (default, the item is refused), skip, or new_version (update the draft in place, or cut a new draft version over an active channel)"
    )]
    pub on_conflict: Option<String>,
}

pub async fn list(client: &OrionClient, params: ChannelsListParams) -> Result<String, String> {
    let qs = utils::build_query_string(&[
        ("status", params.status),
        ("channel_type", params.channel_type),
        ("protocol", params.protocol),
        ("tag", params.tag),
        ("limit", params.limit.map(|l| l.to_string())),
        ("offset", params.offset.map(|o| o.to_string())),
        ("sort_by", params.sort_by),
        ("sort_order", params.sort_order),
    ]);
    let resp: Value = client
        .get(&format!("{}{qs}", paths::CHANNELS))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn get(client: &OrionClient, params: ChannelsGetParams) -> Result<String, String> {
    let resp: Value = client
        .get(&paths::channel(&params.id))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn create(client: &OrionClient, params: ChannelsCreateParams) -> Result<String, String> {
    let body: Value = serde_json::from_str(&params.channel_json)
        .map_err(|e| format!("Invalid channel JSON: {e}"))?;
    let resp: Value = client
        .post(paths::CHANNELS, &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn update(client: &OrionClient, params: ChannelsUpdateParams) -> Result<String, String> {
    let body: Value = serde_json::from_str(&params.channel_json)
        .map_err(|e| format!("Invalid channel JSON: {e}"))?;
    let resp: Value = client
        .put(&paths::channel(&params.id), &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn delete(client: &OrionClient, params: ChannelsDeleteParams) -> Result<String, String> {
    client
        .delete_request(&paths::channel(&params.id))
        .await
        .map_err(|e| e.to_string())?;
    Ok(format!("Channel {} deleted successfully", params.id))
}

pub async fn activate(
    client: &OrionClient,
    params: ChannelsStatusParams,
) -> Result<String, String> {
    change_status(client, &params.id, STATUS_ACTIVE).await
}

pub async fn archive(client: &OrionClient, params: ChannelsStatusParams) -> Result<String, String> {
    change_status(client, &params.id, STATUS_ARCHIVED).await
}

async fn change_status(client: &OrionClient, id: &str, status: &str) -> Result<String, String> {
    let body = serde_json::json!({ "status": status });
    let resp: Value = client
        .patch(&paths::channel_status(id), &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn versions(
    client: &OrionClient,
    params: ChannelsVersionsParams,
) -> Result<String, String> {
    let resp: Value = client
        .get(&paths::channel_versions(&params.id))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn create_version(
    client: &OrionClient,
    params: ChannelsVersionsParams,
) -> Result<String, String> {
    let resp: Value = client
        .post_empty(&paths::channel_versions(&params.id))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn import(client: &OrionClient, params: ChannelsImportParams) -> Result<String, String> {
    super::import_resource(
        client,
        paths::CHANNELS_IMPORT,
        "channel",
        &params.channels_json,
        params.dry_run.unwrap_or(false),
        params.on_conflict,
    )
    .await
}

pub async fn export(client: &OrionClient, params: ChannelsExportParams) -> Result<String, String> {
    let qs = utils::build_query_string(&[
        ("status", params.status),
        ("tag", params.tag),
        ("channel_type", params.channel_type),
        ("protocol", params.protocol),
    ]);
    let resp: Value = client
        .get(&format!("{}{qs}", paths::CHANNELS_EXPORT))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn validate(
    client: &OrionClient,
    params: ChannelsValidateParams,
) -> Result<String, String> {
    super::validate_resource(
        client,
        paths::CHANNELS_VALIDATE,
        "channel",
        &params.channel_json,
    )
    .await
}
