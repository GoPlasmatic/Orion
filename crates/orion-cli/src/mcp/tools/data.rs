use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::client::OrionClient;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DataSendSyncParams {
    #[schemars(description = "Channel name to send data to (e.g. \"default\", \"orders\")")]
    pub channel: String,
    #[schemars(
        description = "JSON string of the data payload — the raw business data, e.g. {\"id\": \"order-123\", \"amount\": 250.00}"
    )]
    pub data: String,
    #[schemars(description = "Optional JSON string of metadata")]
    pub metadata: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DataSendAsyncParams {
    #[schemars(description = "Channel name to send data to (e.g. \"default\", \"orders\")")]
    pub channel: String,
    #[schemars(
        description = "JSON string of the data payload — the raw business data, e.g. {\"id\": \"order-123\", \"amount\": 250.00}"
    )]
    pub data: String,
    #[schemars(description = "Optional JSON string of metadata")]
    pub metadata: Option<String>,
}

fn build_data_body(data: &str, metadata: &Option<String>) -> Result<Value, String> {
    let data: Value = serde_json::from_str(data).map_err(|e| format!("Invalid data JSON: {e}"))?;
    let mut body = serde_json::json!({ "data": data });
    if let Some(meta_str) = metadata {
        let meta: Value =
            serde_json::from_str(meta_str).map_err(|e| format!("Invalid metadata JSON: {e}"))?;
        body["metadata"] = meta;
    }
    Ok(body)
}

pub async fn send_sync(client: &OrionClient, params: DataSendSyncParams) -> Result<String, String> {
    let body = build_data_body(&params.data, &params.metadata)?;
    let resp: Value = client
        .post(&format!("/api/v1/data/{}", params.channel), &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn send_async(
    client: &OrionClient,
    params: DataSendAsyncParams,
) -> Result<String, String> {
    let body = build_data_body(&params.data, &params.metadata)?;
    let resp: Value = client
        .post(&format!("/api/v1/data/{}/async", params.channel), &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}
