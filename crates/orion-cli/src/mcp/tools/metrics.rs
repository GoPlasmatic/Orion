use crate::client::OrionClient;
use orion_client::paths;

pub async fn get(client: &OrionClient) -> Result<String, String> {
    client
        .get_text(paths::METRICS)
        .await
        .map_err(|e| e.to_string())
}
