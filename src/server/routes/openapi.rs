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
        (name = "Data", description = "Data processing"),
        (name = "Operational", description = "Health and metrics"),
    ),
    paths(
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
