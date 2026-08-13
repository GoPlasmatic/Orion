use serde_json::Value;

use crate::client::OrionClient;
use orion_client::paths;

pub async fn check(client: &OrionClient) -> Result<String, String> {
    let resp: Value = client.get(paths::HEALTH).await.map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}
