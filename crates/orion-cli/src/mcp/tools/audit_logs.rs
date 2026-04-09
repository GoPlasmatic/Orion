use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::client::OrionClient;
use crate::utils;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AuditLogsListParams {
    #[schemars(
        description = "Maximum number of audit log entries to return (default: 50, max: 1000)"
    )]
    pub limit: Option<i64>,
    #[schemars(description = "Number of entries to skip for pagination")]
    pub offset: Option<i64>,
}

pub async fn list(client: &OrionClient, params: AuditLogsListParams) -> Result<String, String> {
    let qs = utils::build_query_string(&[
        ("limit", params.limit.map(|l| l.to_string())),
        ("offset", params.offset.map(|o| o.to_string())),
    ]);
    let resp: Value = client
        .get(&format!("/api/v1/admin/audit-logs{qs}"))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}
