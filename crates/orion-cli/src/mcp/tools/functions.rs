use serde_json::Value;

use crate::client::OrionClient;

pub async fn list(client: &OrionClient) -> Result<String, String> {
    let resp: Value = client
        .get("/api/v1/admin/functions")
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}
