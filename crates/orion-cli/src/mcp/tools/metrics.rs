use crate::client::OrionClient;

pub async fn get(client: &OrionClient) -> Result<String, String> {
    client.get_text("/metrics").await.map_err(|e| e.to_string())
}
