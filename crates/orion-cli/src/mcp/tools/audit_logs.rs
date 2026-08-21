use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::client::OrionClient;
use crate::utils;
use orion_client::paths;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AuditLogsListParams {
    #[schemars(
        description = "Exact-match filter on the action: create, create_version, update, delete, import, status_active, status_archived, update_rollout, test, reset, purge, requeue, package_staged, package_applied, reload"
    )]
    pub action: Option<String>,
    #[schemars(
        description = "Exact-match filter on the resource type: workflow, channel, connector, engine, backup, circuit_breaker, trace_dlq, package"
    )]
    pub resource_type: Option<String>,
    #[schemars(description = "Exact-match filter on the resource ID")]
    pub resource_id: Option<String>,
    #[schemars(
        description = "Exact-match filter on the acting principal (the admin key id, or 'anonymous')"
    )]
    pub principal: Option<String>,
    #[schemars(
        description = "Inclusive lower bound on created_at, RFC 3339 (e.g. 2026-07-01T00:00:00Z)"
    )]
    pub start_time: Option<String>,
    #[schemars(description = "Exclusive upper bound on created_at, RFC 3339")]
    pub end_time: Option<String>,
    #[schemars(
        description = "Maximum number of audit log entries to return (default: 50, max: 1000)"
    )]
    pub limit: Option<i64>,
    #[schemars(description = "Number of entries to skip for pagination")]
    pub offset: Option<i64>,
}

pub async fn list(client: &OrionClient, params: AuditLogsListParams) -> Result<String, String> {
    let qs = utils::build_query_string(&[
        ("action", params.action),
        ("resource_type", params.resource_type),
        ("resource_id", params.resource_id),
        ("principal", params.principal),
        ("start_time", params.start_time),
        ("end_time", params.end_time),
        ("limit", params.limit.map(|l| l.to_string())),
        ("offset", params.offset.map(|o| o.to_string())),
    ]);
    let resp: Value = client
        .get(&format!("{}{qs}", paths::AUDIT_LOGS))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}
