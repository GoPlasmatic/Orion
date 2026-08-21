use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::client::OrionClient;
use crate::utils;
use orion_client::paths;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TracesListParams {
    #[schemars(description = "Filter by trace status (e.g. completed, failed)")]
    pub status: Option<String>,
    #[schemars(description = "Filter by channel name")]
    pub channel: Option<String>,
    #[schemars(description = "Filter by processing mode (e.g. sync, async)")]
    pub mode: Option<String>,
    #[schemars(description = "Maximum number of traces to return")]
    pub limit: Option<i64>,
    #[schemars(description = "Number of traces to skip for pagination")]
    pub offset: Option<i64>,
    #[schemars(
        description = "Field to sort by: created_at (default), updated_at, status, channel, mode"
    )]
    pub sort_by: Option<String>,
    #[schemars(description = "Sort order: asc or desc")]
    pub sort_order: Option<String>,
    #[schemars(
        description = "Keyset cursor from a previous page's next_cursor — pass it back unmodified. Valid only with the default created_at ordering, and mutually exclusive with offset."
    )]
    pub cursor: Option<String>,
    #[schemars(
        description = "Ask the server to compute `total` for this page. Off by default because it scans the whole filtered set."
    )]
    pub include_total: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TracesGetParams {
    #[schemars(description = "The trace ID to retrieve")]
    pub id: String,
    #[schemars(
        description = "Trace access token from the async submit — required to read a trace without an admin credential (v1.0)"
    )]
    pub token: Option<String>,
}

pub async fn list(client: &OrionClient, params: TracesListParams) -> Result<String, String> {
    let qs = utils::build_query_string(&[
        ("status", params.status),
        ("channel", params.channel),
        ("mode", params.mode),
        ("limit", params.limit.map(|l| l.to_string())),
        ("offset", params.offset.map(|o| o.to_string())),
        ("sort_by", params.sort_by),
        ("sort_order", params.sort_order),
        ("cursor", params.cursor),
        (
            "include_total",
            params.include_total.and_then(|v| v.then(|| "true".into())),
        ),
    ]);
    let resp: Value = client
        .get(&format!("{}{qs}", paths::TRACES))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn get(client: &OrionClient, params: TracesGetParams) -> Result<String, String> {
    let qs = utils::build_query_string(&[("token", params.token)]);
    let resp: Value = client
        .get(&format!("{}{qs}", paths::trace(&params.id)))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}
