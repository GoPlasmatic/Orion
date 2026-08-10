use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::client::OrionClient;
use crate::utils;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PackagesListParams {
    #[schemars(description = "Maximum number of receipts to return")]
    pub limit: Option<i64>,
    #[schemars(description = "Number of receipts to skip for pagination")]
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PackagesGetParams {
    #[schemars(description = "The package name to look up")]
    pub name: String,
}

pub async fn list(client: &OrionClient, params: PackagesListParams) -> Result<String, String> {
    let qs = utils::build_query_string(&[
        ("limit", params.limit.map(|l| l.to_string())),
        ("offset", params.offset.map(|o| o.to_string())),
    ]);
    let resp: Value = client
        .get(&format!("/api/v1/admin/packages{qs}"))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn get(client: &OrionClient, params: PackagesGetParams) -> Result<String, String> {
    let resp: Value = client
        .get(&format!("/api/v1/admin/packages/{}", params.name))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}
