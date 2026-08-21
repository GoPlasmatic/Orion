pub mod audit_logs;
pub mod backups;
pub mod channels;
pub mod circuit_breakers;
pub mod connectors;
pub mod data;
pub mod engine;
pub mod functions;
pub mod health;
pub mod metrics;
pub mod packages;
pub mod trace_dlq;
pub mod traces;
pub mod workflows;

use serde_json::Value;

use crate::client::OrionClient;

/// Shared bulk-import for the workflows/channels/connectors MCP tools. POSTs a
/// JSON array to `base_path`, optionally with `?dry_run=true` (server-side
/// validation without writing) and `?on_conflict=` (what an already-stored
/// key means), and returns the import summary as pretty JSON.
pub(crate) async fn import_resource(
    client: &OrionClient,
    base_path: &str,
    label: &str,
    items_json: &str,
    dry_run: bool,
    on_conflict: Option<String>,
) -> Result<String, String> {
    let items: Value =
        serde_json::from_str(items_json).map_err(|e| format!("Invalid {label} JSON: {e}"))?;
    if !items.is_array() {
        return Err(format!("Import data must be a JSON array of {label}s"));
    }
    let qs = crate::utils::build_query_string(&[
        ("dry_run", dry_run.then(|| "true".to_string())),
        ("on_conflict", on_conflict),
    ]);
    let resp: Value = client
        .post(&format!("{base_path}{qs}"), &items)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}

/// Shared `POST /{kind}/validate` for the MCP tools: check a definition
/// without creating it, returning the `{valid, errors, warnings}` envelope.
pub(crate) async fn validate_resource(
    client: &OrionClient,
    path: &str,
    label: &str,
    item_json: &str,
) -> Result<String, String> {
    let body: Value =
        serde_json::from_str(item_json).map_err(|e| format!("Invalid {label} JSON: {e}"))?;
    let resp: Value = client.post(path, &body).await.map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}
