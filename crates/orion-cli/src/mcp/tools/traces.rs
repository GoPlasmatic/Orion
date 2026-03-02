use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::client::OrionClient;

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
    #[schemars(description = "Field to sort by (e.g. created_at, duration_ms)")]
    pub sort_by: Option<String>,
    #[schemars(description = "Sort order: asc or desc")]
    pub sort_order: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TracesGetParams {
    #[schemars(description = "The trace ID to retrieve")]
    pub id: String,
}

pub async fn list(client: &OrionClient, params: TracesListParams) -> Result<String, String> {
    let mut query = Vec::new();
    if let Some(s) = &params.status {
        query.push(format!("status={s}"));
    }
    if let Some(c) = &params.channel {
        query.push(format!("channel={c}"));
    }
    if let Some(m) = &params.mode {
        query.push(format!("mode={m}"));
    }
    if let Some(l) = params.limit {
        query.push(format!("limit={l}"));
    }
    if let Some(o) = params.offset {
        query.push(format!("offset={o}"));
    }
    if let Some(sb) = &params.sort_by {
        query.push(format!("sort_by={sb}"));
    }
    if let Some(so) = &params.sort_order {
        query.push(format!("sort_order={so}"));
    }
    let qs = if query.is_empty() {
        String::new()
    } else {
        format!("?{}", query.join("&"))
    };
    let resp: Value = client
        .get(&format!("/api/v1/data/traces{qs}"))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn get(client: &OrionClient, params: TracesGetParams) -> Result<String, String> {
    let resp: Value = client
        .get(&format!("/api/v1/data/traces/{}", params.id))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}
