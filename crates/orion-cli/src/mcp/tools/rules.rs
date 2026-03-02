use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::client::OrionClient;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RulesListParams {
    #[schemars(description = "Filter by rule status: active, paused, or archived")]
    pub status: Option<String>,
    #[schemars(description = "Filter by channel name")]
    pub channel: Option<String>,
    #[schemars(description = "Filter by tag")]
    pub tag: Option<String>,
    #[schemars(description = "Maximum number of rules to return")]
    pub limit: Option<i64>,
    #[schemars(description = "Number of rules to skip for pagination")]
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RulesGetParams {
    #[schemars(description = "The rule ID to retrieve")]
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RulesCreateParams {
    #[schemars(description = include_str!("descriptions/param_rule_json.md"))]
    pub rule_json: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RulesUpdateParams {
    #[schemars(description = "The rule ID to update")]
    pub id: String,
    #[schemars(description = include_str!("descriptions/param_rule_json.md"))]
    pub rule_json: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RulesDeleteParams {
    #[schemars(description = "The rule ID to delete")]
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RulesStatusParams {
    #[schemars(description = "The rule ID to change status for")]
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RulesTestParams {
    #[schemars(description = "The rule ID to test")]
    pub id: String,
    #[schemars(
        description = "JSON string of the test data payload. Provide the raw business data that would arrive on the channel, e.g. {\"id\": \"order-123\", \"amount\": 250.00}"
    )]
    pub data: String,
    #[schemars(description = "Optional JSON string of metadata")]
    pub metadata: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RulesExportParams {
    #[schemars(description = "Filter exported rules by status")]
    pub status: Option<String>,
    #[schemars(description = "Filter exported rules by channel")]
    pub channel: Option<String>,
    #[schemars(description = "Filter exported rules by tag")]
    pub tag: Option<String>,
    #[schemars(description = "Maximum number of rules to export")]
    pub limit: Option<i64>,
    #[schemars(description = "Number of rules to skip for pagination")]
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RulesValidateParams {
    #[schemars(description = include_str!("descriptions/param_rule_json.md"))]
    pub rule_json: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RulesRolloutParams {
    #[schemars(description = "The rule ID to update rollout for")]
    pub id: String,
    #[schemars(
        description = "Rollout percentage (0-100). Controls what percentage of matching data is processed by this rule."
    )]
    pub rollout_percentage: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RulesVersionsParams {
    #[schemars(description = "The rule ID to list versions for")]
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RulesImportParams {
    #[schemars(
        description = "JSON string containing an array of rule definitions to import. Each element must be a complete rule object (see rules_create for format)."
    )]
    pub rules_json: String,
    #[schemars(description = "If true, preview what would be imported without actually importing")]
    pub dry_run: Option<bool>,
}

fn build_query_string(
    status: &Option<String>,
    channel: &Option<String>,
    tag: &Option<String>,
    limit: &Option<i64>,
    offset: &Option<i64>,
) -> String {
    let mut query = Vec::new();
    if let Some(s) = status {
        query.push(format!("status={s}"));
    }
    if let Some(c) = channel {
        query.push(format!("channel={c}"));
    }
    if let Some(t) = tag {
        query.push(format!("tag={t}"));
    }
    if let Some(l) = limit {
        query.push(format!("limit={l}"));
    }
    if let Some(o) = offset {
        query.push(format!("offset={o}"));
    }
    if query.is_empty() {
        String::new()
    } else {
        format!("?{}", query.join("&"))
    }
}

pub async fn list(client: &OrionClient, params: RulesListParams) -> Result<String, String> {
    let qs = build_query_string(
        &params.status,
        &params.channel,
        &params.tag,
        &params.limit,
        &params.offset,
    );
    let resp: Value = client
        .get(&format!("/api/v1/admin/rules{qs}"))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn get(client: &OrionClient, params: RulesGetParams) -> Result<String, String> {
    let resp: Value = client
        .get(&format!("/api/v1/admin/rules/{}", params.id))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn create(client: &OrionClient, params: RulesCreateParams) -> Result<String, String> {
    let body: Value =
        serde_json::from_str(&params.rule_json).map_err(|e| format!("Invalid rule JSON: {e}"))?;
    let resp: Value = client
        .post("/api/v1/admin/rules", &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn update(client: &OrionClient, params: RulesUpdateParams) -> Result<String, String> {
    let body: Value =
        serde_json::from_str(&params.rule_json).map_err(|e| format!("Invalid rule JSON: {e}"))?;
    let resp: Value = client
        .put(&format!("/api/v1/admin/rules/{}", params.id), &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn delete(client: &OrionClient, params: RulesDeleteParams) -> Result<String, String> {
    client
        .delete_request(&format!("/api/v1/admin/rules/{}", params.id))
        .await
        .map_err(|e| e.to_string())?;
    Ok(format!("Rule {} deleted successfully", params.id))
}

pub async fn activate(client: &OrionClient, params: RulesStatusParams) -> Result<String, String> {
    change_status(client, &params.id, "active").await
}

pub async fn pause(client: &OrionClient, params: RulesStatusParams) -> Result<String, String> {
    change_status(client, &params.id, "paused").await
}

pub async fn archive(client: &OrionClient, params: RulesStatusParams) -> Result<String, String> {
    change_status(client, &params.id, "archived").await
}

async fn change_status(client: &OrionClient, id: &str, status: &str) -> Result<String, String> {
    let body = serde_json::json!({ "status": status });
    let resp: Value = client
        .patch(&format!("/api/v1/admin/rules/{id}/status"), &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn test(client: &OrionClient, params: RulesTestParams) -> Result<String, String> {
    let data: Value =
        serde_json::from_str(&params.data).map_err(|e| format!("Invalid test data JSON: {e}"))?;

    let mut body = serde_json::json!({ "data": data });
    if let Some(meta_str) = &params.metadata {
        let meta: Value =
            serde_json::from_str(meta_str).map_err(|e| format!("Invalid metadata JSON: {e}"))?;
        body["metadata"] = meta;
    }

    let resp: Value = client
        .post(&format!("/api/v1/admin/rules/{}/test", params.id), &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn export(client: &OrionClient, params: RulesExportParams) -> Result<String, String> {
    let qs = build_query_string(
        &params.status,
        &params.channel,
        &params.tag,
        &params.limit,
        &params.offset,
    );
    let resp: Value = client
        .get(&format!("/api/v1/admin/rules/export{qs}"))
        .await
        .map_err(|e| e.to_string())?;
    let data = resp.get("data").unwrap_or(&resp);
    serde_json::to_string_pretty(data).map_err(|e| e.to_string())
}

pub async fn import(client: &OrionClient, params: RulesImportParams) -> Result<String, String> {
    let rules: Value =
        serde_json::from_str(&params.rules_json).map_err(|e| format!("Invalid rules JSON: {e}"))?;

    if !rules.is_array() {
        return Err("Import data must be a JSON array of rules".to_string());
    }

    if params.dry_run.unwrap_or(false) {
        let count = rules.as_array().map(|a| a.len()).unwrap_or(0);
        let mut preview = format!("Dry run: would import {count} rule(s)\n");
        if let Some(arr) = rules.as_array() {
            for (i, rule) in arr.iter().enumerate() {
                let name = rule["name"].as_str().unwrap_or("(unnamed)");
                let channel = rule["channel"].as_str().unwrap_or("default");
                preview.push_str(&format!("  {}. {name} (channel: {channel})\n", i + 1));
            }
        }
        return Ok(preview);
    }

    let resp: Value = client
        .post("/api/v1/admin/rules/import", &rules)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn validate(client: &OrionClient, params: RulesValidateParams) -> Result<String, String> {
    let body: Value =
        serde_json::from_str(&params.rule_json).map_err(|e| format!("Invalid rule JSON: {e}"))?;
    let resp: Value = client
        .post("/api/v1/admin/rules/validate", &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn rollout(client: &OrionClient, params: RulesRolloutParams) -> Result<String, String> {
    let body = serde_json::json!({ "rollout_percentage": params.rollout_percentage });
    let resp: Value = client
        .patch(&format!("/api/v1/admin/rules/{}/rollout", params.id), &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn versions(client: &OrionClient, params: RulesVersionsParams) -> Result<String, String> {
    let resp: Value = client
        .get(&format!("/api/v1/admin/rules/{}/versions", params.id))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn create_version(
    client: &OrionClient,
    params: RulesVersionsParams,
) -> Result<String, String> {
    let resp: Value = client
        .post_empty(&format!("/api/v1/admin/rules/{}/versions", params.id))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}
