use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::client::OrionClient;
use crate::utils;
use orion_client::paths;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DlqListParams {
    #[schemars(description = "Filter by channel name")]
    pub channel: Option<String>,
    #[schemars(description = "Only entries whose retries are exhausted")]
    pub exhausted: Option<bool>,
    #[schemars(description = "Maximum number of entries to return")]
    pub limit: Option<i64>,
    #[schemars(description = "Number of entries to skip for pagination")]
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DlqEntryParams {
    #[schemars(description = "The DLQ entry ID")]
    pub id: String,
}

pub async fn list(client: &OrionClient, params: DlqListParams) -> Result<String, String> {
    let qs = utils::build_query_string(&[
        ("channel", params.channel),
        ("exhausted", params.exhausted.map(|e| e.to_string())),
        ("limit", params.limit.map(|l| l.to_string())),
        ("offset", params.offset.map(|o| o.to_string())),
    ]);
    let resp: Value = client
        .get(&format!("{}{qs}", paths::TRACE_DLQ))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn get(client: &OrionClient, params: DlqEntryParams) -> Result<String, String> {
    let resp: Value = client
        .get(&paths::trace_dlq_entry(&params.id))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn requeue(client: &OrionClient, params: DlqEntryParams) -> Result<String, String> {
    let resp: Value = client
        .post_empty(&paths::trace_dlq_requeue(&params.id))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}
