use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::client::OrionClient;
use orion_client::paths;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DataSendSyncParams {
    #[schemars(description = "Channel name to send data to (e.g. \"default\", \"orders\")")]
    pub channel: String,
    #[schemars(
        description = "JSON string of the data payload — the raw business data, e.g. {\"id\": \"order-123\", \"amount\": 250.00}"
    )]
    pub data: String,
    #[schemars(description = "Optional JSON string of metadata")]
    pub metadata: Option<String>,
    #[schemars(
        description = "Send the payload as the request body verbatim, with no {\"data\": ...} envelope. Required for a channel configured with request.body_mode = \"payload\"; such a channel accepts no caller metadata, so do not combine with metadata."
    )]
    pub raw: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DataSendAsyncParams {
    #[schemars(description = "Channel name to send data to (e.g. \"default\", \"orders\")")]
    pub channel: String,
    #[schemars(
        description = "JSON string of the data payload — the raw business data, e.g. {\"id\": \"order-123\", \"amount\": 250.00}"
    )]
    pub data: String,
    #[schemars(description = "Optional JSON string of metadata")]
    pub metadata: Option<String>,
    #[schemars(
        description = "Send the payload as the request body verbatim, with no {\"data\": ...} envelope. Required for a channel configured with request.body_mode = \"payload\"; such a channel accepts no caller metadata, so do not combine with metadata."
    )]
    pub raw: Option<bool>,
}

/// The request body carrying `data`.
///
/// Mirrors `SendCmd::build_body`: the default wraps in the Orion envelope,
/// `raw` sends the payload verbatim for a channel configured with
/// `request.body_mode = "payload"` (#282). Such a channel stamps metadata
/// server-side and accepts none from the caller, so combining the two is a
/// usage error rather than a silent drop — the CLI enforces the same thing
/// through clap's `conflicts_with`.
fn build_data_body(
    data: &str,
    metadata: &Option<String>,
    raw: Option<bool>,
) -> Result<Value, String> {
    let data: Value = serde_json::from_str(data).map_err(|e| format!("Invalid data JSON: {e}"))?;
    if raw == Some(true) {
        if metadata.is_some() {
            return Err(
                "raw and metadata cannot be combined: a body_mode = \"payload\" channel \
                 accepts no caller metadata"
                    .to_string(),
            );
        }
        return Ok(data);
    }
    let mut body = serde_json::json!({ "data": data });
    if let Some(meta_str) = metadata {
        let meta: Value =
            serde_json::from_str(meta_str).map_err(|e| format!("Invalid metadata JSON: {e}"))?;
        body["metadata"] = meta;
    }
    Ok(body)
}

pub async fn send_sync(client: &OrionClient, params: DataSendSyncParams) -> Result<String, String> {
    let body = build_data_body(&params.data, &params.metadata, params.raw)?;
    let resp: Value = client
        .post(&paths::data(&params.channel), &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

pub async fn send_async(
    client: &OrionClient,
    params: DataSendAsyncParams,
) -> Result<String, String> {
    let body = build_data_body(&params.data, &params.metadata, params.raw)?;
    let resp: Value = client
        .post(&paths::data_async(&params.channel), &body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::build_data_body;

    #[test]
    fn the_default_wraps_the_payload() {
        let body = build_data_body(r#"{"platform":"ios"}"#, &None, None).expect("test");
        assert_eq!(body, serde_json::json!({"data": {"platform": "ios"}}));
    }

    #[test]
    fn raw_sends_the_payload_verbatim() {
        let body = build_data_body(r#"{"platform":"ios","data":{"t":1}}"#, &None, Some(true))
            .expect("test");
        assert_eq!(
            body,
            serde_json::json!({"platform": "ios", "data": {"t": 1}})
        );
    }

    /// The CLI gets this from clap's `conflicts_with`; MCP has no such layer,
    /// so the check is explicit and must stay an error rather than a drop.
    #[test]
    fn raw_and_metadata_are_refused_together() {
        let err = build_data_body(
            r#"{"a":1}"#,
            &Some(r#"{"src":"mcp"}"#.to_string()),
            Some(true),
        )
        .expect_err("must refuse the combination");
        assert!(err.contains("raw"), "{err}");
    }

    #[test]
    fn raw_false_is_the_default_behaviour() {
        let body = build_data_body(r#"{"a":1}"#, &None, Some(false)).expect("test");
        assert_eq!(body, serde_json::json!({"data": {"a": 1}}));
    }
}
