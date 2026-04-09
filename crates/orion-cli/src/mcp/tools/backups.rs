use serde_json::Value;

use crate::client::OrionClient;

pub async fn create(client: &OrionClient) -> Result<String, String> {
    let resp: Value = client
        .post_empty("/api/v1/admin/backups")
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn list(client: &OrionClient) -> Result<String, String> {
    let resp: Value = client
        .get("/api/v1/admin/backups")
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}
