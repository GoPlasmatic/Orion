//! Every endpoint path, built in one place.
//!
//! Static paths are `const`s; parameterized ones are functions. IDs are
//! interpolated verbatim (no percent-encoding), matching what the CLI and
//! the package CLI have always sent.

// -- Admin: workflows --
pub const WORKFLOWS: &str = "/api/v1/admin/workflows";
pub const WORKFLOWS_EXPORT: &str = "/api/v1/admin/workflows/export";
pub const WORKFLOWS_IMPORT: &str = "/api/v1/admin/workflows/import";
pub const WORKFLOWS_VALIDATE: &str = "/api/v1/admin/workflows/validate";

pub fn workflow(id: &str) -> String {
    format!("{WORKFLOWS}/{id}")
}
pub fn workflow_status(id: &str) -> String {
    format!("{WORKFLOWS}/{id}/status")
}
pub fn workflow_rollout(id: &str) -> String {
    format!("{WORKFLOWS}/{id}/rollout")
}
pub fn workflow_test(id: &str) -> String {
    format!("{WORKFLOWS}/{id}/test")
}
pub fn workflow_versions(id: &str) -> String {
    format!("{WORKFLOWS}/{id}/versions")
}
pub fn workflow_dependencies(id: &str) -> String {
    format!("{WORKFLOWS}/{id}/dependencies")
}

// -- Admin: channels --
pub const CHANNELS: &str = "/api/v1/admin/channels";
pub const CHANNELS_EXPORT: &str = "/api/v1/admin/channels/export";
pub const CHANNELS_IMPORT: &str = "/api/v1/admin/channels/import";
pub const CHANNELS_VALIDATE: &str = "/api/v1/admin/channels/validate";

pub fn channel(id: &str) -> String {
    format!("{CHANNELS}/{id}")
}
pub fn channel_status(id: &str) -> String {
    format!("{CHANNELS}/{id}/status")
}
pub fn channel_versions(id: &str) -> String {
    format!("{CHANNELS}/{id}/versions")
}

// -- Admin: connectors --
pub const CONNECTORS: &str = "/api/v1/admin/connectors";
pub const CONNECTORS_EXPORT: &str = "/api/v1/admin/connectors/export";
pub const CONNECTORS_IMPORT: &str = "/api/v1/admin/connectors/import";
pub const CONNECTORS_VALIDATE: &str = "/api/v1/admin/connectors/validate";
pub const CIRCUIT_BREAKERS: &str = "/api/v1/admin/connectors/circuit-breakers";

pub fn connector(id: &str) -> String {
    format!("{CONNECTORS}/{id}")
}
pub fn connector_test(id: &str) -> String {
    format!("{CONNECTORS}/{id}/test")
}
pub fn circuit_breaker(id: &str) -> String {
    format!("{CIRCUIT_BREAKERS}/{id}")
}

// -- Admin: packages (promotion receipts) --
pub const PACKAGES: &str = "/api/v1/admin/packages";

pub fn package(name: &str) -> String {
    format!("{PACKAGES}/{name}")
}

// -- Admin: traces & DLQ --
pub const TRACES: &str = "/api/v1/admin/traces";
pub const TRACE_DLQ: &str = "/api/v1/admin/trace-dlq";
pub const TRACE_DLQ_PURGE: &str = "/api/v1/admin/trace-dlq/purge";

pub fn trace(id: &str) -> String {
    format!("{TRACES}/{id}")
}
pub fn trace_dlq_entry(id: &str) -> String {
    format!("{TRACE_DLQ}/{id}")
}
pub fn trace_dlq_requeue(id: &str) -> String {
    format!("{TRACE_DLQ}/{id}/requeue")
}

// -- Admin: the rest --
pub const AUDIT_LOGS: &str = "/api/v1/admin/audit-logs";
pub const BACKUPS: &str = "/api/v1/admin/backups";
pub const ENGINE_STATUS: &str = "/api/v1/admin/engine/status";
pub const ENGINE_RELOAD: &str = "/api/v1/admin/engine/reload";
pub const FUNCTIONS: &str = "/api/v1/admin/functions";

// -- Data plane --
pub fn data(channel: &str) -> String {
    format!("/api/v1/data/{channel}")
}
pub fn data_async(channel: &str) -> String {
    format!("/api/v1/data/{channel}/async")
}

// -- Operational --
pub const HEALTH: &str = "/health";
pub const METRICS: &str = "/metrics";

/// Build a URL query string from key-value pairs, skipping `None` values.
/// Returns `""` when nothing is set, `"?k=v&k2=v2"` otherwise.
pub fn query_string(params: &[(&str, Option<String>)]) -> String {
    let parts: Vec<String> = params
        .iter()
        .filter_map(|(k, v)| v.as_ref().map(|val| format!("{k}={val}")))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameterized_paths_interpolate() {
        assert_eq!(workflow("wf-1"), "/api/v1/admin/workflows/wf-1");
        assert_eq!(
            workflow_status("wf-1"),
            "/api/v1/admin/workflows/wf-1/status"
        );
        assert_eq!(
            circuit_breaker("db"),
            "/api/v1/admin/connectors/circuit-breakers/db"
        );
        assert_eq!(data_async("orders"), "/api/v1/data/orders/async");
        assert_eq!(package("billing"), "/api/v1/admin/packages/billing");
    }

    #[test]
    fn query_string_skips_none_and_prefixes_once() {
        assert_eq!(query_string(&[("a", None)]), "");
        assert_eq!(
            query_string(&[
                ("status", Some("active".to_string())),
                ("tag", None),
                ("limit", Some("10".to_string())),
            ]),
            "?status=active&limit=10"
        );
    }
}
