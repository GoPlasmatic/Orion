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
pub mod traces;
pub mod workflows;

use serde_json::Value;

use crate::client::OrionClient;

/// Shared bulk-import for the workflows/channels/connectors MCP tools. POSTs a
/// JSON array to `base_path`, optionally with `?dry_run=true` (server-side
/// validation without writing), and returns the import summary as pretty JSON.
pub(crate) async fn import_resource(
    client: &OrionClient,
    base_path: &str,
    label: &str,
    items_json: &str,
    dry_run: bool,
) -> Result<String, String> {
    let items: Value =
        serde_json::from_str(items_json).map_err(|e| format!("Invalid {label} JSON: {e}"))?;
    if !items.is_array() {
        return Err(format!("Import data must be a JSON array of {label}s"));
    }
    let path = if dry_run {
        format!("{base_path}?dry_run=true")
    } else {
        base_path.to_string()
    };
    let resp: Value = client
        .post(&path, &items)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
}
