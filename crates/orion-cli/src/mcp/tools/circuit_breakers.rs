use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::client::OrionClient;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CircuitBreakersListParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CircuitBreakerResetParams {
    #[schemars(description = "The circuit breaker key to reset (format: connector:channel)")]
    pub key: String,
}

pub async fn list(
    client: &OrionClient,
    _params: CircuitBreakersListParams,
) -> Result<String, String> {
    let resp: Value = client
        .get("/api/v1/admin/connectors/circuit-breakers")
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn reset(
    client: &OrionClient,
    params: CircuitBreakerResetParams,
) -> Result<String, String> {
    let _: Value = client
        .post_empty(&format!(
            "/api/v1/admin/connectors/circuit-breakers/{}",
            params.key
        ))
        .await
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "Circuit breaker '{}' reset successfully",
        params.key
    ))
}
