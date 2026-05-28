use serde::Serialize;
use utoipa::OpenApi;

/// Error response body matching Orion's `{"error": {"code": "...", "message": "..."}}` format.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ErrorResponse {
    error: ErrorDetail,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ErrorDetail {
    code: String,
    message: String,
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Orion — Declarative Services Runtime API",
        version = env!("CARGO_PKG_VERSION"),
        description = "Declarative services runtime platform",
        license(name = "Apache-2.0"),
    ),
    tags(
        (name = "Channels", description = "Channel management"),
        (name = "Workflows", description = "Workflow management"),
        (name = "Connectors", description = "Connector management"),
        (name = "Engine", description = "Engine control"),
        (name = "Functions", description = "Engine function schemas"),
        (name = "Audit", description = "Admin audit-log history"),
        (name = "Backups", description = "Database backup management (SQLite only)"),
        (name = "Data", description = "Data processing"),
        (name = "Operational", description = "Health and metrics"),
    ),
    paths(
        // Channels
        super::admin::channels::list_channels,
        super::admin::channels::create_channel,
        super::admin::channels::get_channel,
        super::admin::channels::update_channel,
        super::admin::channels::delete_channel,
        super::admin::channels::change_channel_status,
        super::admin::channels::list_channel_versions,
        super::admin::channels::create_new_channel_version,
        super::admin::channels::import_channels,
        // Workflows
        super::admin::workflows::list_workflows,
        super::admin::workflows::create_workflow,
        super::admin::workflows::get_workflow,
        super::admin::workflows::update_workflow,
        super::admin::workflows::delete_workflow,
        super::admin::workflows::change_workflow_status,
        super::admin::workflows::update_rollout,
        super::admin::workflows::list_workflow_versions,
        super::admin::workflows::create_new_workflow_version,
        super::admin::workflows::test_workflow,
        super::admin::workflows::import_workflows,
        super::admin::workflows::export_workflows,
        super::admin::workflows::validate_workflow,
        // Connectors
        super::admin::connectors::list_connectors,
        super::admin::connectors::create_connector,
        super::admin::connectors::get_connector,
        super::admin::connectors::update_connector,
        super::admin::connectors::delete_connector,
        super::admin::connectors::list_circuit_breakers,
        super::admin::connectors::reset_circuit_breaker,
        super::admin::connectors::import_connectors,
        // Engine
        super::admin::engine::engine_status,
        super::admin::engine::engine_reload,
        // Functions (A1: input-schema registry surfaced for tooling)
        super::admin::functions::list_functions,
        // Audit logs
        super::admin::audit::list_audit_logs,
        // Backups
        super::admin::backups::create_backup,
        super::admin::backups::list_backups,
        // Data
        super::data::list_traces,
        super::data::get_trace,
        // Operational
        super::health_check,
        super::metrics_endpoint,
    ),
    components(
        schemas(
            crate::storage::models::Workflow,
            crate::storage::models::Channel,
            crate::storage::models::Connector,
            crate::storage::models::Trace,
            crate::storage::repositories::workflows::CreateWorkflowRequest,
            crate::storage::repositories::workflows::UpdateWorkflowRequest,
            crate::storage::repositories::workflows::StatusChangeRequest,
            crate::storage::repositories::workflows::RolloutUpdateRequest,
            crate::storage::repositories::channels::CreateChannelRequest,
            crate::storage::repositories::channels::UpdateChannelRequest,
            crate::storage::repositories::channels::ChannelStatusChangeRequest,
            crate::storage::repositories::connectors::CreateConnectorRequest,
            crate::storage::repositories::connectors::UpdateConnectorRequest,
            super::data::ProcessRequest,
            ErrorResponse,
            ErrorDetail,
        )
    )
)]
pub(crate) struct ApiDoc;

/// The public HTTP API's OpenAPI 3.1 spec, pretty-printed as JSON.
///
/// Shared by the `orion-server dump-openapi` subcommand and the drift-check
/// integration test so both serialize the spec identically. The committed
/// copy lives at `docs/openapi.json`; regenerate it with
/// `cargo run -- dump-openapi > docs/openapi.json` whenever the API changes.
pub fn pretty_json() -> String {
    serde_json::to_string_pretty(&ApiDoc::openapi())
        .expect("OpenAPI spec is always serializable to JSON")
}
