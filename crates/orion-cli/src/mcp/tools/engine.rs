use serde_json::Value;

use crate::client::OrionClient;
use orion_client::paths;

pub async fn status(client: &OrionClient) -> Result<String, String> {
    let resp: Value = client
        .get(paths::ENGINE_STATUS)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn reload(client: &OrionClient) -> Result<String, String> {
    let resp: Value = client
        .post_empty(paths::ENGINE_RELOAD)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}
